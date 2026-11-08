//! Library facade for embedding cm in Rust programs.
//!
//! The CLI is a thin viewer over the same library. Hosts can open a store,
//! recall context, write memories, manage task frames, absorb exploration
//! output, and run maintenance without spawning a process.

use std::collections::HashSet;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};

use crate::brief::{brief_prompt, collect_feedback_stats, json_tokens, BriefMemory};
use crate::explore_api::{self, ExploreCursor, ExploreOptions, ExplorePlan, MechanicalPlan};
use crate::frames::{self, TaskFrame};
use crate::model::{
    cmx_line, estimate_tokens, memory_from_input, memory_payload, now_ms, validate_input,
    validate_memory, validate_trust, AppendInput, ImportLine, Memory, MemoryOut,
};
use crate::mutate::{self, AbsorbOptions, AbsorbReport};
use crate::search::{
    select_context, select_context_scored, RecallHit, RecallQuery, RecallResponse, SearchOut,
    SearchQuery,
};
use crate::source::source_snippet;
use crate::store::{
    acquire_store_lock_with, append_record_unlocked, checkpoint_file, create_store,
    find_tail_duplicate, reindex_file, verify_full, Store, StoreOptions, T_MEMORY, T_MERGE,
};

/// Input for [`Cm::remember`].
#[derive(Clone, Debug)]
pub struct NewMemory {
    pub kind: String,
    pub scope: String,
    pub subject: String,
    pub text: String,
    pub source: String,
    pub confidence: f64,
    pub trust: Option<String>,
    pub supersedes: Vec<String>,
    pub conflict_of: Option<String>,
    pub valid_from: u64,
    pub valid_to: Option<u64>,
    pub source_commit: Option<String>,
}

impl NewMemory {
    pub fn new(
        kind: impl Into<String>,
        scope: impl Into<String>,
        subject: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        Self {
            kind: kind.into(),
            scope: scope.into(),
            subject: subject.into(),
            text: text.into(),
            source: "agent:library".to_string(),
            confidence: 0.8,
            trust: None,
            supersedes: Vec::new(),
            conflict_of: None,
            valid_from: 0,
            valid_to: None,
            source_commit: None,
        }
    }

    pub fn source(mut self, source: impl Into<String>) -> Self {
        self.source = source.into();
        self
    }

    pub fn confidence(mut self, confidence: f64) -> Self {
        self.confidence = confidence.clamp(0.0, 1.0);
        self
    }

    pub fn trust(mut self, trust: impl Into<String>) -> Self {
        self.trust = Some(trust.into());
        self
    }

    pub fn supersedes(mut self, ids: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.supersedes = ids.into_iter().map(Into::into).collect();
        self
    }

    pub fn conflict_of(mut self, id: impl Into<String>) -> Self {
        self.conflict_of = Some(id.into());
        self
    }

    pub fn valid_from(mut self, timestamp_ms: u64) -> Self {
        self.valid_from = timestamp_ms;
        self
    }

    pub fn valid_to(mut self, timestamp_ms: u64) -> Self {
        self.valid_to = Some(timestamp_ms);
        self
    }

    pub fn source_commit(mut self, commit: impl Into<String>) -> Self {
        self.source_commit = Some(commit.into());
        self
    }
}

/// Result of a library write. `duplicate` is true when the same content was
/// already active and the existing record was returned instead of appending.
#[derive(Clone, Debug)]
pub struct RememberResult {
    pub memory: Memory,
    pub duplicate: bool,
}

#[derive(Clone, Debug)]
pub struct BriefResponse {
    pub frame: Option<TaskFrame>,
    pub query: String,
    pub budget: usize,
    pub estimated_tokens: usize,
    pub memories: Vec<RecallHit>,
    pub prompt: String,
}

#[derive(Clone, Debug)]
pub struct StoreStatus {
    pub path: PathBuf,
    pub doc_count: u64,
    pub active_count: u64,
    pub tail_count: usize,
    pub generation: u64,
    pub bytes: u64,
    pub cache_bytes: u64,
    pub cache_capacity: u32,
    pub cache_entries: u32,
    pub cache_data_entries: u32,
    pub cache_index_entries: u32,
    pub cache_hits: u32,
    pub cache_misses: u32,
    pub cache_evictions: u32,
}

#[derive(Clone, Debug)]
pub struct CheckpointReport {
    pub generation: u64,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug)]
pub struct ReindexReport {
    pub documents: u64,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug)]
pub struct VerifyReport {
    pub ok: bool,
    pub records: u64,
    pub checksum_mismatches: u64,
}

#[derive(Clone, Debug)]
pub struct ImportReport {
    pub written: usize,
    pub skipped: usize,
}

#[derive(Clone, Debug)]
pub struct ExportReport {
    pub written: usize,
}

#[derive(Clone, Debug)]
pub struct FinishResult {
    pub frame: TaskFrame,
    pub frame_id: String,
    pub summary_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeedbackVerdict {
    Useful,
    Stale,
    Wrong,
}

impl FeedbackVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Useful => "useful",
            Self::Stale => "stale",
            Self::Wrong => "wrong",
        }
    }
}

/// Handle to one cm file. The handle is cheap; each operation opens the file
/// and uses the same mmap/index path as the CLI.
#[derive(Clone, Debug)]
pub struct Cm {
    path: PathBuf,
    options: StoreOptions,
}

impl Cm {
    /// Create a new store. With `empty = true`, no seed memories are written.
    pub fn init(path: impl AsRef<Path>, empty: bool) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        create_store(&path, empty)?;
        Ok(Self {
            path,
            options: StoreOptions::default(),
        })
    }

    /// Open an existing store and validate its header.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with(path, StoreOptions::default())
    }

    /// Open a store with explicit read-only, cache, lock, and source policies.
    pub fn open_with(path: impl AsRef<Path>, options: StoreOptions) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        Store::open_with(&path, &options)?;
        Ok(Self { path, options })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn options(&self) -> &StoreOptions {
        &self.options
    }

    fn open_store(&self) -> Result<Store> {
        Store::open_with(&self.path, &self.options)
    }

    fn ensure_writable(&self) -> Result<()> {
        if self.options.read_only {
            bail!("store is read-only: {}", self.path.display());
        }
        Ok(())
    }

    /// Append one durable memory. Exact duplicate content returns the existing
    /// active record instead of creating another copy. The duplicate check and
    /// append are one critical section.
    pub fn remember(&self, input: NewMemory) -> Result<RememberResult> {
        self.ensure_writable()?;
        self.options.source_policy.check(&input.source)?;
        let append = AppendInput {
            kind: input.kind,
            scope: input.scope,
            subject: input.subject,
            text: input.text,
            source: input.source,
            confidence: input.confidence,
            valid_from: input.valid_from,
            valid_to: input.valid_to,
            supersedes: input.supersedes,
            conflict_of: input.conflict_of,
            trust: input.trust,
            source_commit: input.source_commit,
        };
        validate_input(&append)?;
        let memory = memory_from_input(&append)?;
        let _lock = acquire_store_lock_with(&self.path, self.options.lock_timeout)?;
        if let Some(existing_id) = find_tail_duplicate(&self.path, &memory)? {
            if let Some(existing) = self.get(&existing_id)? {
                return Ok(RememberResult {
                    memory: existing,
                    duplicate: true,
                });
            }
        }
        append_record_unlocked(&self.path, T_MEMORY, &memory_payload(&memory), true)?;
        Ok(RememberResult {
            memory,
            duplicate: false,
        })
    }

    /// Return one active memory by id.
    pub fn get(&self, id: &str) -> Result<Option<Memory>> {
        let mut store = self.open_store()?;
        if let Some((_, memory)) = store.find_doc(id)? {
            let tail = store.tail_view()?;
            if !tail.hidden.contains(id) {
                return Ok(Some(memory));
            }
        }
        Ok(store.tail_view()?.get(id).cloned())
    }

    /// Return all active memories, newest first.
    pub fn all(&self) -> Result<Vec<Memory>> {
        let store = self.open_store()?;
        let tail = store.tail_view()?;
        let mut memories = store.active_memories_bulk()?;
        memories.retain(|memory| !tail.hidden.contains(&memory.id));
        memories.extend(tail.memories.clone());
        let mut seen = HashSet::new();
        memories.retain(|memory| seen.insert(memory.id.clone()));
        memories.sort_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(memories)
    }

    /// Search the active index with the same BM25 implementation as the CLI.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Memory>> {
        let limit = limit.max(1);
        let mut store = self.open_store()?;
        let tail = store.tail_view()?;
        let mut hits: Vec<(f64, Memory)> = Vec::new();
        for (doc_idx, score) in store.bm25(
            query,
            None,
            None,
            None,
            None,
            limit.saturating_mul(4).min(256),
        )? {
            let meta = store.doc_meta(doc_idx)?;
            let memory = store.doc_memory(&meta)?;
            if tail.hidden.contains(&memory.id) {
                continue;
            }
            hits.push((score, memory));
        }
        for (score, memory) in tail.bm25(query, None, None, None, limit.saturating_mul(4)) {
            hits.push((score, memory));
        }
        let mut seen = HashSet::new();
        hits.retain(|(_, memory)| seen.insert(memory.id.clone()));
        hits.sort_by(|left, right| {
            right
                .0
                .partial_cmp(&left.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.1.id.cmp(&right.1.id))
        });
        hits.truncate(limit);
        Ok(hits
            .into_iter()
            .map(|(score, mut memory)| {
                memory.score = Some(score);
                memory
            })
            .collect())
    }

    /// Search with the CLI's full filter set.
    pub fn search_query(&self, query: &SearchQuery) -> Result<SearchOut> {
        crate::search::search_out(&self.path, query)
    }

    /// Recall a token-budgeted context window.
    pub fn recall(&self, query: &RecallQuery) -> Result<RecallResponse> {
        let mut store = self.open_store()?;
        let (selected, estimated) = select_context_scored(&mut store, query)?;
        let mut memories = Vec::with_capacity(selected.len());
        for (score, memory) in selected {
            let snippet = (query.snippet_lines > 0)
                .then(|| {
                    source_snippet(
                        &memory,
                        &self.options.source_policy.root,
                        query.snippet_lines,
                    )
                })
                .flatten();
            let estimated_tokens = estimate_tokens(&cmx_line(&memory));
            memories.push(RecallHit {
                score,
                memory,
                estimated_tokens,
                snippet,
            });
        }
        Ok(RecallResponse {
            query: query.query.clone(),
            estimated_tokens: estimated,
            memories,
        })
    }

    /// Build a stable brief from the active (or named) task frame.
    pub fn brief(&self, task: Option<&str>, budget: usize) -> Result<BriefResponse> {
        self.brief_inner(task, None, budget)
    }

    pub fn brief_for(&self, task: &str, budget: usize) -> Result<BriefResponse> {
        self.brief_inner(Some(task), None, budget)
    }

    pub fn brief_with_query(
        &self,
        task: Option<&str>,
        query: &str,
        budget: usize,
    ) -> Result<BriefResponse> {
        self.brief_inner(task, Some(query), budget)
    }

    fn brief_inner(
        &self,
        task: Option<&str>,
        explicit_query: Option<&str>,
        budget: usize,
    ) -> Result<BriefResponse> {
        let mut store = self.open_store()?;
        let (frame, query, frame_tokens, task) = match frames::latest_frame(&mut store, task)? {
            Some((memory, frame)) => {
                let query = explicit_query
                    .map(str::to_string)
                    .or_else(|| frames::frame_query(&frame))
                    .ok_or_else(|| {
                        anyhow!(
                            "frame for task {:?} has no goal or next steps; pass a query",
                            frame.task
                        )
                    })?;
                let value = frames::frame_json(&memory, &frame);
                let tokens = estimate_tokens(&serde_json::to_string(&value)?);
                let task_name = Some(frame.task.clone());
                (Some((memory, frame)), query, tokens, task_name)
            }
            None => {
                let query = explicit_query
                    .map(str::to_string)
                    .ok_or_else(|| anyhow!("no frame for this workspace; pass a query"))?;
                (None, query, 0, task.map(str::to_string))
            }
        };
        let memory_budget = budget.saturating_sub(frame_tokens);
        let query_args = RecallQuery {
            query: query.clone(),
            budget_tokens: memory_budget,
            top: 16,
            max_items: 5,
            scope: None,
            task: task.clone(),
            kind: None,
            min_trust: Some("imported".to_string()),
            min_score: 0.0,
            compact: true,
            snippet_lines: 0,
        };
        let (mut selected, _) = select_context(&mut store, &query_args)?;
        let feedback = collect_feedback_stats(&mut store, 1024)?;
        selected.retain(|memory| {
            feedback
                .get(&memory.id)
                .is_none_or(|stats| !(stats.wrong >= 2 || (stats.stale >= 2 && stats.useful == 0)))
        });
        let mut briefs: Vec<BriefMemory> = selected.iter().map(BriefMemory::from).collect();
        let mut memory_tokens = json_tokens(&briefs)?;
        while !briefs.is_empty() && frame_tokens + memory_tokens > budget {
            briefs.pop();
            memory_tokens = json_tokens(&briefs)?;
        }
        selected.truncate(briefs.len());
        let frame_value = frame
            .as_ref()
            .map(|(memory, frame)| frames::frame_json(memory, frame));
        let prompt = brief_prompt(&frame_value, &query, &briefs);
        let memories = selected
            .iter()
            .zip(briefs.iter())
            .map(|(memory, brief)| RecallHit {
                memory: memory.clone(),
                score: memory.score.unwrap_or(0.0),
                estimated_tokens: estimate_tokens(
                    &serde_json::to_string(brief).unwrap_or_default(),
                ),
                snippet: brief.snippet.clone(),
            })
            .collect();
        Ok(BriefResponse {
            frame: frame.map(|(_, frame)| frame),
            query,
            budget,
            estimated_tokens: frame_tokens + memory_tokens,
            memories,
            prompt,
        })
    }

    pub fn status(&self) -> Result<StoreStatus> {
        let store = self.open_store()?;
        let tail = store.tail_view()?;
        let cache = store.cache_stats()?;
        Ok(StoreStatus {
            path: self.path.clone(),
            doc_count: store.header.doc_count,
            active_count: store.active_count()? + tail.memories.len() as u64,
            tail_count: tail.memories.len(),
            generation: store.header.generation,
            bytes: std::fs::metadata(&self.path)?.len(),
            cache_bytes: cache.bytes,
            cache_capacity: cache.capacity,
            cache_entries: cache.entries,
            cache_data_entries: cache.data_entries,
            cache_index_entries: cache.index_entries,
            cache_hits: cache.hits,
            cache_misses: cache.misses,
            cache_evictions: cache.evictions,
        })
    }

    pub fn forget(&self, id: &str, reason: Option<&str>) -> Result<()> {
        self.ensure_writable()?;
        mutate::forget(
            &self.path,
            id,
            reason.unwrap_or("user_request"),
            self.options.lock_timeout,
        )
    }

    pub fn merge(&self, ids: &[String], text: Option<&str>) -> Result<Memory> {
        self.ensure_writable()?;
        mutate::merge(&self.path, ids, text, self.options.lock_timeout)
    }

    pub fn frame_set(&self, frame: &TaskFrame) -> Result<Memory> {
        self.ensure_writable()?;
        let _lock = acquire_store_lock_with(&self.path, self.options.lock_timeout)?;
        let mut store = self.open_store()?;
        let existing_id = frames::collect_frames(&mut store, 64)?
            .into_iter()
            .find(|memory| memory.subject == frames::frame_subject(&frame.task))
            .map(|memory| memory.id);
        let mut frame = frame.clone();
        frame.updated_at = now_ms();
        frames::save_frame(&self.path, &frame, existing_id, "agent:library", None, true)
    }

    pub fn frame_get(&self, task: &str) -> Result<Option<TaskFrame>> {
        let mut store = self.open_store()?;
        let memory = frames::collect_frames(&mut store, 64)?
            .into_iter()
            .find(|memory| memory.subject == frames::frame_subject(task));
        memory
            .map(|memory| {
                frames::parse_frame(&memory)
                    .ok_or_else(|| anyhow!("frame record {} has invalid payload", memory.id))
            })
            .transpose()
    }

    pub fn frames(&self) -> Result<Vec<TaskFrame>> {
        let mut store = self.open_store()?;
        Ok(frames::collect_frames(&mut store, 512)?
            .iter()
            .filter_map(frames::parse_frame)
            .collect())
    }

    pub fn claim(&self, task: &str, owner: &str, lease: Duration) -> Result<TaskFrame> {
        self.ensure_writable()?;
        let _lock = acquire_store_lock_with(&self.path, self.options.lock_timeout)?;
        let mut store = self.open_store()?;
        let (memory, mut frame) = frames::latest_frame(&mut store, Some(task))?
            .ok_or_else(|| anyhow!("no frame for task {task:?}"))?;
        let now = now_ms();
        if let Some(existing) = frame.owner.as_deref() {
            if existing != owner && frame.lease_until.is_some_and(|end| end > now) {
                bail!(
                    "task {:?} is held by {} until {}",
                    frame.task,
                    existing,
                    frame.lease_until.unwrap_or(0)
                );
            }
        }
        frame.owner = Some(owner.to_string());
        frame.lease_until = Some(now.saturating_add(lease.as_millis() as u64));
        frame.updated_at = now;
        frames::save_frame(
            &self.path,
            &frame,
            Some(memory.id),
            "agent:claim",
            None,
            true,
        )?;
        Ok(frame)
    }

    pub fn release(&self, task: &str, owner: Option<&str>) -> Result<TaskFrame> {
        self.ensure_writable()?;
        let _lock = acquire_store_lock_with(&self.path, self.options.lock_timeout)?;
        let mut store = self.open_store()?;
        let (memory, mut frame) = frames::latest_frame(&mut store, Some(task))?
            .ok_or_else(|| anyhow!("no frame for task {task:?}"))?;
        let now = now_ms();
        if let (Some(requested), Some(existing)) = (owner, frame.owner.as_deref()) {
            if requested != existing && frame.lease_until.is_some_and(|end| end > now) {
                bail!("task {:?} is held by {}", frame.task, existing);
            }
        }
        frame.owner = None;
        frame.lease_until = None;
        frame.updated_at = now;
        frames::save_frame(
            &self.path,
            &frame,
            Some(memory.id),
            "agent:release",
            None,
            true,
        )?;
        Ok(frame)
    }

    pub fn finish(
        &self,
        task: &str,
        summary: Option<&str>,
        owner: Option<&str>,
    ) -> Result<FinishResult> {
        self.ensure_writable()?;
        let _lock = acquire_store_lock_with(&self.path, self.options.lock_timeout)?;
        let mut store = self.open_store()?;
        let (memory, mut frame) = frames::latest_frame(&mut store, Some(task))?
            .ok_or_else(|| anyhow!("no frame for task {task:?}"))?;
        frames::ensure_frame_owner(&frame, owner, "finish")?;
        let summary = summary
            .map(str::to_string)
            .unwrap_or_else(|| format!("Completed: {}", frame.goal));
        frame.status = "done".to_string();
        frame.owner = None;
        frame.lease_until = None;
        frame.updated_at = now_ms();
        let note = format!("finished: {summary}");
        if !frame.notes.contains(&note) {
            frame.notes.push(note);
        }
        let frame_memory = frames::save_frame(
            &self.path,
            &frame,
            Some(memory.id),
            "agent:finish",
            None,
            true,
        )?;
        let input = AppendInput {
            kind: "episode".to_string(),
            scope: "repo".to_string(),
            subject: format!("task:{} summary", frame.task),
            text: summary,
            source: "agent:finish".to_string(),
            confidence: 0.9,
            valid_from: 0,
            valid_to: None,
            supersedes: Vec::new(),
            conflict_of: None,
            trust: Some("authored".to_string()),
            source_commit: None,
        };
        let summary_memory = memory_from_input(&input)?;
        append_record_unlocked(&self.path, T_MEMORY, &memory_payload(&summary_memory), true)?;
        Ok(FinishResult {
            frame,
            frame_id: frame_memory.id,
            summary_id: summary_memory.id,
        })
    }

    pub fn feedback(&self, id: &str, verdict: FeedbackVerdict, note: Option<&str>) -> Result<()> {
        self.ensure_writable()?;
        mutate::write_feedback(&self.path, id, verdict.as_str(), note, "agent:library")
    }

    pub fn checkpoint(&self, in_place: bool) -> Result<CheckpointReport> {
        self.ensure_writable()?;
        let before = std::fs::metadata(&self.path)?.len();
        let start = Instant::now();
        let header = checkpoint_file(&self.path, in_place)?;
        Ok(CheckpointReport {
            generation: header.generation,
            bytes_before: before,
            bytes_after: std::fs::metadata(&self.path)?.len(),
            elapsed_ms: start.elapsed().as_millis() as u64,
        })
    }

    pub fn reindex(&self) -> Result<ReindexReport> {
        self.ensure_writable()?;
        let before = std::fs::metadata(&self.path)?.len();
        let start = Instant::now();
        let header = reindex_file(&self.path)?;
        Ok(ReindexReport {
            documents: header.doc_count,
            bytes_before: before,
            bytes_after: std::fs::metadata(&self.path)?.len(),
            elapsed_ms: start.elapsed().as_millis() as u64,
        })
    }

    pub fn verify(&self, fast: bool) -> Result<VerifyReport> {
        let records = if fast {
            self.open_store()?.header.doc_count
        } else {
            verify_full(&self.path)?
        };
        Ok(VerifyReport {
            ok: true,
            records,
            checksum_mismatches: 0,
        })
    }

    pub fn export_jsonl<W: Write>(&self, mut writer: W) -> Result<ExportReport> {
        let memories = self.all()?;
        for memory in &memories {
            writeln!(
                writer,
                "{}",
                serde_json::to_string(&MemoryOut::from(memory))?
            )?;
        }
        Ok(ExportReport {
            written: memories.len(),
        })
    }

    pub fn import_jsonl<R: BufRead>(&self, reader: R) -> Result<ImportReport> {
        self.ensure_writable()?;
        let _lock = acquire_store_lock_with(&self.path, self.options.lock_timeout)?;
        let mut written = 0usize;
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let parsed: ImportLine = serde_json::from_str(&line)?;
            let memory = match parsed {
                ImportLine::Full(full) => {
                    if full.id.trim().is_empty() {
                        bail!("imported record has empty id");
                    }
                    validate_memory(&full.kind, &full.scope, &full.subject, &full.text)?;
                    if let Some(trust) = &full.trust {
                        validate_trust(trust)?;
                    }
                    full.into_memory()
                }
                ImportLine::Append(input) => memory_from_input(&input)?,
            };
            let record_type = if memory.merged { T_MERGE } else { T_MEMORY };
            append_record_unlocked(&self.path, record_type, &memory_payload(&memory), true)?;
            written += 1;
        }
        Ok(ImportReport {
            written,
            skipped: 0,
        })
    }

    pub fn explore_plan(&self, options: &ExploreOptions) -> Result<ExplorePlan> {
        explore_api::explore_plan(&self.path, options)
    }

    pub fn mechanical_explore(
        &self,
        options: &ExploreOptions,
        apply: bool,
    ) -> Result<MechanicalPlan> {
        if apply {
            self.ensure_writable()?;
        }
        explore_api::mechanical_explore(&self.path, options, apply)
    }

    pub fn absorb(&self, content: &str, options: &AbsorbOptions) -> Result<AbsorbReport> {
        self.ensure_writable()?;
        self.options.source_policy.check_absorb(content)?;
        mutate::absorb(
            &self.path,
            content,
            options,
            self.options.lock_timeout,
            Some(&self.options.source_policy.root),
        )
    }

    pub fn explore_cursor(&self) -> Result<Option<ExploreCursor>> {
        let memories = self.all()?;
        let Some(memory) = memories
            .iter()
            .find(|memory| memory.subject == "explore:cursor")
        else {
            return Ok(None);
        };
        Ok(serde_json::from_str(&memory.text).ok())
    }

    pub fn set_explore_cursor(&self, cursor: &ExploreCursor) -> Result<RememberResult> {
        self.remember(
            NewMemory::new(
                "episode",
                "repo",
                "explore:cursor",
                serde_json::to_string(cursor)?,
            )
            .source("agent:explore")
            .trust("authored"),
        )
    }

    /// Build a cursor for an exploration run, reading the repository HEAD.
    pub fn explore_cursor_for(
        &self,
        root: &Path,
        mode: &str,
        memories: usize,
    ) -> Result<ExploreCursor> {
        Ok(ExploreCursor {
            git_head: explore_api::git_head(root)?,
            built_at: now_ms(),
            mode: mode.to_string(),
            memories,
        })
    }
}
