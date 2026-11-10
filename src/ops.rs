//! Reusable command implementations shared by the `cm` CLI and the library
//! facade ([`crate::api::Cm`]).
//!
//! The CLI is a thin argument parser over these functions: every operation
//! here is available to embedders without the `cli` feature, and the JSON
//! shapes are the ones the CLI prints.

use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};

use crate::brief::{
    brief_prompt, collect_feedback_stats, BriefMemory, Feedback, FeedbackStats, FEEDBACK_KIND,
};
use crate::explore_api::changed_paths;
use crate::frames::{
    append_values, collect_frames, ensure_frame_owner, frame_json, frame_query, frame_subject,
    latest_frame, parse_frame, save_frame, uncovered_touches, TaskFrame, FRAME_KIND,
};
use crate::model::{
    build_merged, cmx_line, content_hash, estimate_tokens, kind_id, memory_from_input,
    memory_from_input_at, memory_payload, minhash_signature, minhash_similarity, pair_similarity,
    preview, validate_input, validate_memory, validate_trust, AppendInput, Dsu, ImportLine, Memory,
    MemoryOut,
};
use crate::mutate::write_feedback;
use crate::search::{active_memories, select_context_scored, RecallHit, RecallQuery};
use crate::source::{
    anchor_text, confined_source_path, source_anchor, source_parts, source_snippet,
};
use crate::store::{
    acquire_store_lock, append_record, append_record_unlocked, checkpoint_file, create_store,
    find_subject_memories, find_tail_duplicate, reindex_file, verify_full, SourcePolicy, Store,
    T_MEMORY,
};

/// Options for [`remember_with`].
#[derive(Clone, Debug, Default)]
pub struct RememberOptions {
    pub durable: bool,
    pub force: bool,
    pub replace: bool,
    pub verify_source: bool,
}

/// Result of a library write. `duplicate` is true when the same content was
/// already active and the existing record was returned instead of appending.
#[derive(Clone, Debug)]
pub struct RememberOutcome {
    pub memory: Memory,
    pub duplicate: bool,
}

/// Options for [`govern`].
#[derive(Clone, Debug)]
pub struct GovernOptions {
    pub apply: bool,
    pub merge: bool,
    pub stale_days: u64,
    pub wrong_threshold: u32,
    pub merge_threshold: f64,
}

impl Default for GovernOptions {
    fn default() -> Self {
        Self {
            apply: false,
            merge: false,
            stale_days: 90,
            wrong_threshold: 2,
            merge_threshold: 0.85,
        }
    }
}

/// Options for [`audit`].
#[derive(Clone, Debug)]
pub struct AuditOptions {
    pub root: PathBuf,
    pub apply: bool,
    pub refresh: bool,
    pub changed: bool,
    pub scope: Option<String>,
    pub limit: Option<usize>,
}

impl Default for AuditOptions {
    fn default() -> Self {
        Self {
            root: PathBuf::from("."),
            apply: false,
            refresh: false,
            changed: false,
            scope: None,
            limit: None,
        }
    }
}

/// Options for [`finish_with`].
#[derive(Clone, Debug)]
pub struct FinishOptions {
    pub task: String,
    pub summary: Option<String>,
    pub root: PathBuf,
    pub apply: bool,
    pub strict: bool,
    pub changed: bool,
    pub owner: Option<String>,
    pub scope: String,
}

impl Default for FinishOptions {
    fn default() -> Self {
        Self {
            task: "current".to_string(),
            summary: None,
            root: PathBuf::from("."),
            apply: false,
            strict: false,
            changed: false,
            owner: None,
            scope: "repo".to_string(),
        }
    }
}

/// Options for [`frame_set`]. Mirrors the CLI `frame set` merge semantics.
#[derive(Clone, Debug)]
pub struct FrameSetOptions {
    pub task: String,
    pub goal: Option<String>,
    pub status: Option<String>,
    pub constraints: Vec<String>,
    pub next: Vec<String>,
    pub open: Vec<String>,
    pub touches: Vec<String>,
    pub notes: Vec<String>,
    pub owner: Option<String>,
    pub source: String,
    pub source_commit: Option<String>,
}

impl Default for FrameSetOptions {
    fn default() -> Self {
        Self {
            task: "current".to_string(),
            goal: None,
            status: None,
            constraints: Vec::new(),
            next: Vec::new(),
            open: Vec::new(),
            touches: Vec::new(),
            notes: Vec::new(),
            owner: None,
            source: "agent".to_string(),
            source_commit: None,
        }
    }
}

/// Options shared by `brief` and `pack`.
#[derive(Clone, Debug)]
pub struct BriefOptions {
    pub task: Option<String>,
    pub query: Option<String>,
    pub budget: usize,
    pub top: usize,
    pub max_items: usize,
    pub scope: Option<String>,
    pub min_trust: Option<String>,
    /// Minimum BM25 score a recalled memory must reach (frames are never
    /// filtered by this; they are explicit task context).
    pub min_score: f64,
    pub snippet_lines: usize,
    pub root: PathBuf,
}

impl Default for BriefOptions {
    fn default() -> Self {
        Self {
            task: None,
            query: None,
            budget: 1200,
            top: 16,
            max_items: 5,
            scope: None,
            min_trust: Some("imported".to_string()),
            min_score: 0.0,
            snippet_lines: 0,
            root: PathBuf::from("."),
        }
    }
}

/// Export format for [`export_text`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportFormat {
    Cmx,
    Jsonl,
}

impl ExportFormat {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "cmx" => Ok(Self::Cmx),
            "jsonl" => Ok(Self::Jsonl),
            other => bail!("invalid export format {other:?}; expected cmx or jsonl"),
        }
    }
}

#[derive(Serialize)]
pub struct CandidateOut {
    pub id: String,
    pub bm25: f64,
    pub minhash: f64,
    pub subject: String,
    pub preview: String,
}

#[derive(Serialize)]
pub struct SimilarOut {
    pub id: String,
    pub candidates: Vec<CandidateOut>,
}

#[derive(Serialize)]
pub struct MergeGroupOut {
    pub sources: Vec<String>,
    pub strategy: String,
    pub score: f64,
    pub text: String,
}

#[derive(Serialize)]
pub struct ConsolidateOut {
    pub groups: Vec<MergeGroupOut>,
    pub applied: bool,
}

#[derive(Serialize)]
pub struct ConflictGroupOut {
    pub scope: String,
    pub kind: String,
    pub subject: String,
    pub records: Vec<MemoryOut>,
}

#[derive(Serialize)]
struct PackMemory {
    id: String,
    kind: String,
    scope: String,
    subject: String,
    text: String,
    source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    snippet: Option<String>,
}

impl From<&Memory> for PackMemory {
    fn from(memory: &Memory) -> Self {
        Self {
            id: memory.id.clone(),
            kind: memory.kind.clone(),
            scope: memory.scope.clone(),
            subject: memory.subject.clone(),
            text: memory.text.clone(),
            source: memory.source.clone(),
            snippet: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Write paths
// ---------------------------------------------------------------------------

/// Append one validated record without duplicate checking (`cm append`).
pub fn append_input(path: &Path, input: &AppendInput, durable: bool) -> Result<Memory> {
    append_input_at(path, input, durable, None)
}

/// [`append_input`] with an explicit anchor root (the library facade passes
/// the configured `SourcePolicy.root`, never the process cwd).
pub fn append_input_at(
    path: &Path,
    input: &AppendInput,
    durable: bool,
    root: Option<&Path>,
) -> Result<Memory> {
    validate_input(input)?;
    Store::open(path)?;
    let memory = memory_from_input_at(input, root)?;
    append_record(path, 2, &memory_payload(&memory), durable)?;
    Ok(memory)
}

/// Parse the CLI `--json` shape and append it.
pub fn append_json(path: &Path, input_json: &str, durable: bool) -> Result<Memory> {
    let input: AppendInput = serde_json::from_str(input_json).context("parse --json input")?;
    append_input(path, &input, durable)
}

/// `cm remember` with the full CLI option set.
pub fn remember_with(
    path: &Path,
    input: &AppendInput,
    options: &RememberOptions,
) -> Result<RememberOutcome> {
    remember_with_at(path, input, options, None)
}

/// [`remember_with`] with an explicit anchor root for `--verify-source` and
/// the stored `source_hash`/`source_line`.
pub fn remember_with_at(
    path: &Path,
    input: &AppendInput,
    options: &RememberOptions,
    root: Option<&Path>,
) -> Result<RememberOutcome> {
    validate_input(input)?;
    Store::open(path)?;
    // The duplicate check, replace lookup and append are one critical section:
    // two concurrent `remember --replace` calls must not both create an active
    // record for the same scope+kind+subject.
    let _lock = acquire_store_lock(path)?;
    if options.verify_source && source_parts(&input.source).is_some() {
        let anchored = match root {
            Some(root) => crate::source::source_anchor_at(&input.source, root),
            None => source_anchor(&input.source),
        };
        if anchored.is_none() {
            bail!("source does not anchor: {:?}", input.source);
        }
    }
    let mut memory = memory_from_input_at(input, root)?;
    if options.replace && memory.supersedes.is_empty() {
        let existing = find_subject_memories(path, &memory.scope, &memory.kind, &memory.subject)?;
        if !existing.is_empty() {
            // Supersede every active duplicate, not only the newest: older
            // records would otherwise stay visible after a replace.
            memory.supersedes = existing.into_iter().map(|record| record.id).collect();
        }
    }
    if !options.force {
        if let Some(existing_id) = find_tail_duplicate(path, &memory)? {
            if let Some(existing) = get_memory(path, &existing_id)? {
                return Ok(RememberOutcome {
                    memory: existing,
                    duplicate: true,
                });
            }
            // The duplicate id is not resolvable (a concurrent purge). Fall
            // through and write the record rather than silently succeeding.
        }
    }
    append_record_unlocked(path, 2, &memory_payload(&memory), options.durable)?;
    Ok(RememberOutcome {
        memory,
        duplicate: false,
    })
}

/// Return one active memory by id.
pub fn get_memory(path: &Path, id: &str) -> Result<Option<Memory>> {
    let mut store = Store::open(path)?;
    let tail = store.tail_view()?;
    if tail.hidden.contains(id) {
        return Ok(None);
    }
    let now = crate::model::now_ms();
    let active = |memory: &Memory| memory.valid_to.is_none_or(|end| end > now);
    if let Some((_, memory)) = store.find_doc(id)? {
        return Ok(active(&memory).then_some(memory));
    }
    Ok(tail.get(id).filter(|memory| active(memory)).cloned())
}

/// `cm forget` (tombstone).
pub fn forget(path: &Path, id: &str, reason: &str) -> Result<()> {
    crate::mutate::forget(path, id, reason, None)
}

/// `cm merge`.
pub fn merge(path: &Path, ids: &[String], text: Option<&str>) -> Result<Memory> {
    crate::mutate::merge(path, ids, text, None)
}

/// `cm feedback`.
pub fn feedback(
    path: &Path,
    id: &str,
    signal: &str,
    note: Option<&str>,
    source: &str,
) -> Result<()> {
    if !matches!(signal, "useful" | "stale" | "wrong") {
        bail!("invalid feedback signal {signal:?}; expected useful/stale/wrong");
    }
    write_feedback(path, id, signal, note, source)
}

// ---------------------------------------------------------------------------
// Task frames
// ---------------------------------------------------------------------------

/// `cm frame set` with merge semantics.
pub fn frame_set(path: &Path, options: &FrameSetOptions) -> Result<Memory> {
    let _lock = acquire_store_lock(path)?;
    let mut store = Store::open(path)?;
    let existing = collect_frames(&mut store, 64)?
        .into_iter()
        .find(|memory| memory.subject == frame_subject(&options.task));
    let existing_id = existing.as_ref().map(|memory| memory.id.clone());
    if let Some(existing_frame) = existing.as_ref().and_then(parse_frame) {
        ensure_frame_owner(&existing_frame, options.owner.as_deref(), "update")?;
    }
    let mut frame = existing
        .as_ref()
        .and_then(parse_frame)
        .unwrap_or_else(|| TaskFrame {
            task: options.task.clone(),
            goal: String::new(),
            status: "active".to_string(),
            constraints: Vec::new(),
            next: Vec::new(),
            open: Vec::new(),
            touches: Vec::new(),
            notes: Vec::new(),
            owner: None,
            lease_until: None,
            updated_at: 0,
        });
    frame.task = options.task.clone();
    if let Some(goal) = &options.goal {
        frame.goal = goal.clone();
    }
    if frame.goal.trim().is_empty() {
        bail!("frame requires --goal for a new task");
    }
    if let Some(status) = &options.status {
        if !matches!(status.as_str(), "active" | "paused" | "done") {
            bail!("invalid frame status {status:?}; expected active/paused/done");
        }
        frame.status = status.clone();
        if frame.status == "done" {
            frame.owner = None;
            frame.lease_until = None;
        }
    }
    append_values(&mut frame.constraints, options.constraints.clone());
    append_values(&mut frame.next, options.next.clone());
    append_values(&mut frame.open, options.open.clone());
    append_values(&mut frame.touches, options.touches.clone());
    append_values(&mut frame.notes, options.notes.clone());
    frame.updated_at = crate::model::now_ms();
    save_frame(
        path,
        &frame,
        existing_id,
        &options.source,
        options.source_commit.clone(),
        true,
    )
}

pub fn frame_get_value(path: &Path, task: &str) -> Result<Value> {
    frame_get_value_with(path, task, &crate::store::StoreOptions::default())
}

pub fn frame_get_value_with(
    path: &Path,
    task: &str,
    store_options: &crate::store::StoreOptions,
) -> Result<Value> {
    let mut store = Store::open_with(path, store_options)?;
    let memory = collect_frames(&mut store, 64)?
        .into_iter()
        .find(|memory| memory.subject == frame_subject(task));
    match memory {
        Some(memory) => {
            let frame = parse_frame(&memory)
                .ok_or_else(|| anyhow!("frame record {} has invalid payload", memory.id))?;
            Ok(frame_json(&memory, &frame))
        }
        None => Ok(json!({"task": task, "found": false})),
    }
}

pub fn frame_list_value(path: &Path) -> Result<Value> {
    frame_list_value_with(path, &crate::store::StoreOptions::default())
}

pub fn frame_list_value_with(
    path: &Path,
    store_options: &crate::store::StoreOptions,
) -> Result<Value> {
    let mut store = Store::open_with(path, store_options)?;
    let frames: Vec<Value> = collect_frames(&mut store, 512)?
        .iter()
        .filter_map(|memory| parse_frame(memory).map(|frame| frame_json(memory, &frame)))
        .collect();
    Ok(Value::Array(frames))
}

pub fn claim(path: &Path, task: &str, owner: &str, lease_minutes: u64) -> Result<Value> {
    let _lock = acquire_store_lock(path)?;
    let mut store = Store::open(path)?;
    let (memory, mut frame) = latest_frame(&mut store, Some(task))?
        .ok_or_else(|| anyhow!("no frame for task {task:?}"))?;
    let now = crate::model::now_ms();
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
    let lease_until = now.saturating_add(lease_minutes.max(1).saturating_mul(60_000));
    frame.owner = Some(owner.to_string());
    frame.lease_until = Some(lease_until);
    frame.updated_at = now;
    let saved = save_frame(path, &frame, Some(memory.id), "agent:claim", None, true)?;
    Ok(json!({
        "ok": true,
        "id": saved.id,
        "task": frame.task,
        "owner": owner,
        "lease_until": lease_until
    }))
}

pub fn release(path: &Path, task: &str, owner: Option<&str>) -> Result<Value> {
    let _lock = acquire_store_lock(path)?;
    let mut store = Store::open(path)?;
    let (memory, mut frame) = latest_frame(&mut store, Some(task))?
        .ok_or_else(|| anyhow!("no frame for task {task:?}"))?;
    let now = crate::model::now_ms();
    if let (Some(requested), Some(existing)) = (owner, frame.owner.as_deref()) {
        if requested != existing && frame.lease_until.is_some_and(|end| end > now) {
            bail!("task {:?} is held by {}", frame.task, existing);
        }
    }
    frame.owner = None;
    frame.lease_until = None;
    frame.updated_at = now;
    let saved = save_frame(path, &frame, Some(memory.id), "agent:release", None, true)?;
    Ok(json!({"ok": true, "id": saved.id, "task": frame.task, "released": true}))
}

/// `cm finish` with strict/apply audit+govern close-out.
pub fn finish_with(path: &Path, options: &FinishOptions) -> Result<Value> {
    let _lock = acquire_store_lock(path)?;
    let mut store = Store::open(path)?;
    let (memory, mut frame) = latest_frame(&mut store, Some(&options.task))?
        .ok_or_else(|| anyhow!("no frame for task {:?}", options.task))?;
    ensure_frame_owner(&frame, options.owner.as_deref(), "finish")?;
    if options.strict && !frame.open.is_empty() {
        bail!(
            "task {:?} still has {} open question(s); resolve them or omit --strict",
            frame.task,
            frame.open.len()
        );
    }
    if options.strict {
        if !frame.touches.is_empty() {
            let memories = active_memories(&mut store)?;
            let uncovered = uncovered_touches(&frame, &memories);
            if !uncovered.is_empty() {
                bail!(
                    "task {:?} touches {} path(s) with no cited memory: {}",
                    frame.task,
                    uncovered.len(),
                    uncovered.join(", ")
                );
            }
        }
        let audit = audit(
            path,
            &AuditOptions {
                root: options.root.clone(),
                changed: options.changed,
                ..AuditOptions::default()
            },
        )?;
        let issue_count = audit["issue_count"].as_u64().unwrap_or(0);
        if issue_count > 0 {
            bail!(
                "task {:?} has {} source issue(s); repair or omit --strict",
                frame.task,
                issue_count
            );
        }
    }
    let summary = options
        .summary
        .clone()
        .unwrap_or_else(|| format!("Completed: {}", frame.goal));
    frame.status = "done".to_string();
    frame.owner = None;
    frame.lease_until = None;
    frame.updated_at = crate::model::now_ms();
    let note = format!("finished: {summary}");
    if !frame.notes.contains(&note) {
        frame.notes.push(note);
    }
    let frame_memory = save_frame(path, &frame, Some(memory.id), "agent:finish", None, true)?;

    let input = AppendInput {
        kind: "episode".to_string(),
        scope: options.scope.clone(),
        subject: format!("task:{} summary", frame.task),
        text: summary.clone(),
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
    append_record_unlocked(path, T_MEMORY, &memory_payload(&summary_memory), true)?;
    let summary_id = summary_memory.id;

    drop(store);
    drop(_lock);
    let audit_report = audit(
        path,
        &AuditOptions {
            root: options.root.clone(),
            apply: options.apply,
            changed: options.changed,
            ..AuditOptions::default()
        },
    )?;
    let govern_report = govern(
        path,
        &GovernOptions {
            apply: options.apply,
            ..GovernOptions::default()
        },
    )?;
    Ok(json!({
        "ok": true,
        "task": frame.task,
        "status": frame.status,
        "frame_id": frame_memory.id,
        "summary_id": summary_id,
        "strict": options.strict,
        "changed_only": options.changed,
        "audit": audit_report,
        "govern": govern_report
    }))
}

// ---------------------------------------------------------------------------
// Governance / maintenance
// ---------------------------------------------------------------------------

pub fn govern(path: &Path, options: &GovernOptions) -> Result<Value> {
    let merged_groups = if options.merge {
        if options.apply {
            // Consolidation walks the index; fold tail records in first so a
            // governance pass also sees decisions written since the last
            // reindex.
            reindex_file(path)?;
        }
        run_consolidate(path, options.apply, 8, 5000, 0, options.merge_threshold)?
            .groups
            .len()
    } else {
        0
    };
    let mut store = Store::open(path)?;
    let memories = active_memories(&mut store)?;
    let mut feedback: HashMap<String, FeedbackStats> = HashMap::new();
    for memory in &memories {
        if memory.kind != FEEDBACK_KIND {
            continue;
        }
        let Ok(item) = serde_json::from_str::<Feedback>(&memory.text) else {
            continue;
        };
        let stats = feedback.entry(item.target).or_default();
        match item.signal.as_str() {
            "useful" => stats.useful += 1,
            "stale" => stats.stale += 1,
            "wrong" => stats.wrong += 1,
            _ => {}
        }
        stats.last = stats.last.max(item.updated_at);
    }
    let now = crate::model::now_ms();
    let mut duplicate_targets: HashMap<String, String> = HashMap::new();
    let mut by_content: HashMap<(String, String, String, String), Vec<&Memory>> = HashMap::new();
    for memory in &memories {
        if matches!(memory.kind.as_str(), FRAME_KIND | FEEDBACK_KIND) {
            continue;
        }
        by_content
            .entry((
                memory.scope.clone(),
                memory.kind.clone(),
                memory.subject.clone(),
                content_hash(memory),
            ))
            .or_default()
            .push(memory);
    }
    for mut group in by_content.into_values() {
        if group.len() < 2 {
            continue;
        }
        group.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        for duplicate in &group[1..] {
            duplicate_targets.insert(
                duplicate.id.clone(),
                format!("exact duplicate of {}", group[0].id),
            );
        }
    }
    let conflicts = conflict_groups(&memories);
    let mut suggestions = Vec::new();
    for memory in &memories {
        if matches!(memory.kind.as_str(), FRAME_KIND | FEEDBACK_KIND) {
            continue;
        }
        let stats = feedback.get(&memory.id);
        let useful = stats.map_or(0, |s| s.useful);
        let stale = stats.map_or(0, |s| s.stale);
        let wrong = stats.map_or(0, |s| s.wrong);
        let age_days = now.saturating_sub(memory.created_at) / 86_400_000;
        let score = i64::from(useful) * 2 - i64::from(stale) - i64::from(wrong) * 3;
        let (action, reason) = if let Some(reason) = duplicate_targets.get(&memory.id) {
            ("forget", reason.as_str())
        } else if wrong >= options.wrong_threshold.max(1) || score <= -3 {
            ("forget", "negative feedback")
        } else if stale >= 2 || (age_days >= options.stale_days && useful == 0) {
            ("verify", "stale or unverified")
        } else {
            continue;
        };
        suggestions.push(json!({
            "id": memory.id,
            "kind": memory.kind,
            "scope": memory.scope,
            "subject": memory.subject,
            "action": action,
            "reason": reason,
            "age_days": age_days,
            "score": score,
            "useful": useful,
            "stale": stale,
            "wrong": wrong
        }));
    }
    suggestions.sort_by(|a, b| {
        a["action"]
            .as_str()
            .cmp(&b["action"].as_str())
            .then_with(|| a["id"].as_str().cmp(&b["id"].as_str()))
    });
    suggestions.truncate(200);
    let mut forgotten = Vec::new();
    if options.apply {
        for suggestion in &suggestions {
            if suggestion["action"] == "forget" {
                if let Some(id) = suggestion["id"].as_str() {
                    if !forgotten.iter().any(|value| value == id) {
                        forget(path, id, "user_request")?;
                        forgotten.push(id.to_string());
                    }
                }
            }
        }
    }
    Ok(json!({
        "ok": true,
        "apply": options.apply,
        "conflicts": conflicts.len(),
        "merged_groups": merged_groups,
        "suggestions": suggestions,
        "forgotten": forgotten
    }))
}

/// Relative paths reported by `git status --porcelain=v1 -z -uall`, normalized
/// to lowercase forward slashes. Paths are relative to the git repository root,
/// so `audit --changed` expects `--root` to be that root.
fn audit_source_issue(memory: &Memory, full: &Path, symbol: Option<&str>) -> Option<&'static str> {
    if !full.exists() {
        return Some("missing_source");
    }
    if full.is_dir() {
        return None;
    }
    let text = match fs::read_to_string(full) {
        Ok(text) => text,
        Err(_) => return Some("unreadable_source"),
    };
    if let Some(symbol) = symbol {
        let mut present = text.contains(symbol);
        for separator in ["::", "."] {
            if let Some(last) = symbol
                .rsplit(separator)
                .next()
                .filter(|last| *last != symbol)
            {
                present = present || text.contains(last);
            }
        }
        if !present {
            return Some("missing_symbol");
        }
    }
    if let Some(expected) = &memory.source_hash {
        let Some((_, actual)) = anchor_text(&text, symbol) else {
            return Some("missing_symbol");
        };
        if actual != *expected {
            return Some("source_changed");
        }
    }
    None
}

pub fn audit(path: &Path, options: &AuditOptions) -> Result<Value> {
    let mut store = Store::open(path)?;
    let mut memories = active_memories(&mut store)?;
    if options.changed {
        let changed = changed_paths(&options.root)?;
        memories.retain(|memory| {
            source_parts(&memory.source).is_some_and(|(relative, _)| {
                changed.contains(&relative.to_string_lossy().replace('\\', "/").to_lowercase())
            })
        });
    }
    if let Some(scope) = &options.scope {
        memories.retain(|memory| memory.scope == *scope);
    }
    if let Some(limit) = options.limit {
        memories.truncate(limit);
    }
    let now = crate::model::now_ms();
    let threads = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(1)
        .clamp(1, 16);
    let chunk_size = memories.len().div_ceil(threads).max(1);
    let parts: Vec<(usize, Vec<Value>)> = if memories.is_empty() {
        Vec::new()
    } else {
        std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for chunk in memories.chunks(chunk_size) {
                let root = options.root.clone();
                handles.push(scope.spawn(move || {
                    let mut checked = 0usize;
                    let mut issues = Vec::new();
                    for memory in chunk {
                        if matches!(memory.kind.as_str(), FRAME_KIND | FEEDBACK_KIND) {
                            continue;
                        }
                        let Some((relative, symbol)) = source_parts(&memory.source) else {
                            continue;
                        };
                        checked += 1;
                        let Some(full) = confined_source_path(&root, &relative) else {
                            let age_days = now.saturating_sub(memory.created_at) / 86_400_000;
                            issues.push(json!({
                                "id": memory.id,
                                "source": memory.source,
                                "path": relative.display().to_string(),
                                "issue": "unsafe_source",
                                "source_line": memory.source_line,
                                "age_days": age_days
                            }));
                            continue;
                        };
                        let Some(issue) = audit_source_issue(memory, &full, symbol.as_deref())
                        else {
                            continue;
                        };
                        let age_days = now.saturating_sub(memory.created_at) / 86_400_000;
                        issues.push(json!({
                            "id": memory.id,
                            "source": memory.source,
                            "path": full.display().to_string(),
                            "issue": issue,
                            "source_line": memory.source_line,
                            "age_days": age_days
                        }));
                    }
                    (checked, issues)
                }));
            }
            handles
                .into_iter()
                .map(|handle| handle.join().expect("audit worker panicked"))
                .collect()
        })
    };
    let checked: usize = parts.iter().map(|(checked, _)| checked).sum();
    let mut issues: Vec<Value> = parts.into_iter().flat_map(|(_, issues)| issues).collect();
    issues.sort_by(|a, b| {
        a["id"]
            .as_str()
            .cmp(&b["id"].as_str())
            .then_with(|| a["issue"].as_str().cmp(&b["issue"].as_str()))
    });
    let mut refreshed = Vec::new();
    let mut refreshed_old = HashSet::new();
    if options.refresh {
        for issue in &issues {
            if issue["issue"] != "source_changed" {
                continue;
            }
            let Some(id) = issue["id"].as_str() else {
                continue;
            };
            let Some(memory) = memories.iter().find(|memory| memory.id == id) else {
                continue;
            };
            let Some((relative, symbol)) = source_parts(&memory.source) else {
                continue;
            };
            let Some(full) = confined_source_path(&options.root, &relative) else {
                continue;
            };
            let Ok(text) = fs::read_to_string(&full) else {
                continue;
            };
            let Some((line, hash)) = anchor_text(&text, symbol.as_deref()) else {
                continue;
            };
            // Re-check under the write lock: a concurrent forget between the
            // audit snapshot and this append must not resurrect the record.
            let _lock = acquire_store_lock(path)?;
            let mut store = Store::open(path)?;
            if !crate::mutate::active_memory_exists(&mut store, &memory.id)? {
                continue;
            }
            let mut updated = memory.clone();
            updated.id = crate::model::new_id();
            updated.source_hash = Some(hash);
            updated.source_line = (line > 0).then_some(line);
            updated.supersedes = vec![memory.id.clone()];
            append_record_unlocked(path, T_MEMORY, &memory_payload(&updated), true)?;
            refreshed_old.insert(memory.id.clone());
            refreshed.push(updated.id);
        }
    }
    if options.apply {
        for issue in &issues {
            let Some(id) = issue["id"].as_str() else {
                continue;
            };
            if refreshed_old.contains(id) {
                continue;
            }
            let reason = issue["issue"].as_str().unwrap_or("source drift");
            write_feedback(path, id, "stale", Some(reason), "audit")?;
        }
    }
    let total = issues.len();
    let unresolved = total.saturating_sub(refreshed.len());
    issues.truncate(500);
    Ok(json!({
        "ok": true,
        "unresolved": unresolved,
        "root": options.root.display().to_string(),
        "checked": checked,
        "issues": issues,
        "issue_count": total,
        "apply": options.apply,
        "refreshed": refreshed,
        "changed_only": options.changed,
        "scope": options.scope,
        "limit": options.limit
    }))
}

pub fn conflicts(path: &Path) -> Result<Value> {
    let mut store = Store::open(path)?;
    let memories = active_memories(&mut store)?;
    let groups = conflict_groups(&memories);
    Ok(json!({"ok": true, "conflicts": groups}))
}

fn conflict_groups(memories: &[Memory]) -> Vec<ConflictGroupOut> {
    let mut groups: HashMap<(String, String, String), Vec<&Memory>> = HashMap::new();
    for memory in memories {
        if memory.kind == FRAME_KIND || memory.kind == FEEDBACK_KIND {
            continue;
        }
        groups
            .entry((
                memory.scope.clone(),
                memory.kind.clone(),
                memory.subject.clone(),
            ))
            .or_default()
            .push(memory);
    }
    let mut conflicts = Vec::new();
    for ((scope, kind, subject), records) in groups {
        let mut hashes = HashSet::new();
        for record in &records {
            hashes.insert(content_hash(record));
        }
        if hashes.len() > 1 {
            conflicts.push(ConflictGroupOut {
                scope,
                kind,
                subject,
                records: records
                    .iter()
                    .map(|record| MemoryOut::from(*record))
                    .collect(),
            });
        }
    }
    conflicts.sort_by(|a, b| {
        a.scope
            .cmp(&b.scope)
            .then_with(|| a.kind.cmp(&b.kind))
            .then_with(|| a.subject.cmp(&b.subject))
    });
    conflicts
}

pub fn similar(path: &Path, id: &str, top: usize, scope: Option<&str>) -> Result<Value> {
    let mut store = Store::open(path)?;
    let tail = store.tail_view()?;
    if tail.hidden.contains(id) {
        bail!("memory not found: {id}");
    }
    let (target, target_idx) = if let Some((idx, memory)) = store.find_doc(id)? {
        (memory, Some(idx))
    } else {
        (
            tail.get(id)
                .cloned()
                .ok_or_else(|| anyhow!("memory not found: {id}"))?,
            None,
        )
    };
    let scope = scope.unwrap_or(&target.scope);
    let scope_id = store.scope_id(scope)?;
    let mut hits: HashMap<String, (f64, Memory)> = HashMap::new();
    if let Some(scope_id) = scope_id {
        for (doc_idx, score) in store.bm25(
            &format!("{} {}", target.subject, target.text),
            Some(scope_id),
            None,
            target_idx,
            None,
            top.min(100),
        )? {
            let meta = store.doc_meta(doc_idx)?;
            let memory = store.doc_memory(&meta)?;
            if !tail.hidden.contains(&memory.id) {
                hits.insert(memory.id.clone(), (score, memory));
            }
        }
    }
    for (score, memory) in tail.bm25(
        &format!("{} {}", target.subject, target.text),
        Some(scope),
        None,
        Some(&target.id),
        top.min(100),
    ) {
        hits.insert(memory.id.clone(), (score, memory));
    }
    let signature = minhash_signature(&target);
    let mut candidates: Vec<CandidateOut> = hits
        .into_values()
        .map(|(bm25, memory)| CandidateOut {
            id: memory.id.clone(),
            bm25,
            minhash: minhash_similarity(&signature, &minhash_signature(&memory)),
            subject: memory.subject.clone(),
            preview: preview(&memory.text, 120),
        })
        .collect();
    candidates.sort_by(|a, b| {
        b.bm25
            .partial_cmp(&a.bm25)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
    candidates.truncate(top);
    Ok(serde_json::to_value(SimilarOut {
        id: target.id,
        candidates,
    })?)
}

pub fn purge(path: &Path) -> Result<Value> {
    let header = reindex_file(path)?;
    Ok(json!({
        "ok": true,
        "doc_count": header.doc_count,
        "blocks": header.block_count
    }))
}

pub fn scan(path: &Path, limit: usize, offset: usize, format: &str) -> Result<Value> {
    let mut store = Store::open(path)?;
    let memories = store.scan_page(limit.min(5000), offset)?;
    match format {
        "cmx" => Ok(Value::String(
            memories.iter().map(cmx_line).collect::<Vec<_>>().join("\n"),
        )),
        "json" | "jsonl" => Ok(Value::Array(
            memories
                .iter()
                .map(|memory| serde_json::to_value(MemoryOut::from(memory)).map_err(Into::into))
                .collect::<Result<Vec<_>>>()?,
        )),
        other => bail!("invalid scan format {other:?}; expected cmx or json"),
    }
}

pub fn cache_stats(path: &Path) -> Result<Value> {
    let store = Store::open(path)?;
    let stats = store.cache_stats()?;
    Ok(json!({
        "ok": true,
        "bytes": stats.bytes,
        "capacity": stats.capacity,
        "entries": stats.entries,
        "data_entries": stats.data_entries,
        "index_entries": stats.index_entries,
        "hits": stats.hits,
        "misses": stats.misses,
        "evictions": stats.evictions
    }))
}

fn active_metrics(store: &mut Store) -> Result<(u64, usize)> {
    let tail = store.tail_view()?;
    let mut active_index = store.active_count()?;
    for id in &tail.hidden {
        if let Some((idx, _)) = store.find_doc(id)? {
            if store.is_active(idx)? {
                active_index = active_index.saturating_sub(1);
            }
        }
    }
    Ok((active_index, tail.memories.len()))
}

pub fn verify(path: &Path, full: bool) -> Result<Value> {
    let full_records = if full { Some(verify_full(path)?) } else { None };
    let mut store = Store::open(path)?;
    let (active_index, tail_memories) = active_metrics(&mut store)?;
    Ok(json!({
        "ok": true,
        "bytes": fs::metadata(path)?.len(),
        "version": 1,
        "blocks": store.header.block_count,
        "index_docs": store.header.doc_count,
        "active_index_docs": active_index,
        "tail_memories": tail_memories,
        "active_total": active_index + tail_memories as u64,
        "index_bytes": store.header.index_len,
        "index_raw_bytes": store.header.index_raw_len,
        "full_checksum": full,
        "full_records": full_records
    }))
}

pub fn doctor(path: &Path) -> Result<Value> {
    let full = verify_full(path);
    let (full_checksum, full_records, checksum_error) = match full {
        Ok(records) => (true, Some(records), None),
        Err(error) => (
            false,
            None,
            Some(format!("full checksum failed: {error:#}")),
        ),
    };

    let mut warnings = Vec::new();
    let mut store_ok = true;
    let mut active_total = 0u64;
    let mut expired = 0u64;
    let mut conflicts = 0usize;

    match Store::open(path) {
        Ok(mut store) => match active_memories(&mut store) {
            Ok(memories) => {
                active_total = memories.len() as u64;
                let now = crate::model::now_ms();

                let mut expired_warnings = 0usize;
                for memory in &memories {
                    if let Some(end) = memory.valid_to {
                        if end <= now {
                            expired += 1;
                            if expired_warnings < DOCTOR_WARNING_CAP {
                                warnings.push(format!(
                                    "expired: {} scope={} kind={} subject={} valid_to={end}",
                                    memory.id, memory.scope, memory.kind, memory.subject
                                ));
                                expired_warnings += 1;
                            }
                        }
                    }
                }
                if expired > expired_warnings as u64 {
                    warnings.push(format!(
                        "expired: {} more not shown",
                        expired - expired_warnings as u64
                    ));
                }

                let groups = conflict_groups(&memories);
                conflicts = groups.len();
                append_conflict_warnings(&mut warnings, &groups);
            }
            Err(error) => {
                store_ok = false;
                active_total = store.active_count().unwrap_or(0);
                warnings.push(format!("active scan failed: {error:#}"));
            }
        },
        Err(error) => {
            store_ok = false;
            warnings.push(format!("store open failed: {error:#}"));
        }
    }
    if let Some(error) = checksum_error {
        warnings.push(error);
    }

    let cache_bytes = fs::metadata(crate::cache::cache_path(path))
        .map(|meta| meta.len())
        .unwrap_or(0);
    let ok = store_ok && conflicts == 0 && expired == 0 && full_checksum;
    Ok(json!({
        "ok": ok,
        "active_total": active_total,
        "conflicts": conflicts,
        "expired": expired,
        "full_checksum": full_checksum,
        "full_records": full_records,
        "cache_bytes": cache_bytes,
        "warnings": warnings
    }))
}

pub const DOCTOR_WARNING_CAP: usize = 200;

pub fn append_conflict_warnings(warnings: &mut Vec<String>, groups: &[ConflictGroupOut]) {
    for group in groups.iter().take(DOCTOR_WARNING_CAP) {
        warnings.push(format!(
            "conflict: scope={} kind={} subject={} records={}",
            group.scope,
            group.kind,
            group.subject,
            group.records.len()
        ));
    }
    if groups.len() > DOCTOR_WARNING_CAP {
        warnings.push(format!(
            "conflict: {} more not shown",
            groups.len() - DOCTOR_WARNING_CAP
        ));
    }
}

pub fn checkpoint(path: &Path, in_place: bool) -> Result<Value> {
    let header = checkpoint_file(path, in_place)?;
    Ok(json!({
        "ok": true,
        "mode": if in_place { "in-place" } else { "compact" },
        "generation": header.generation,
        "doc_count": header.doc_count,
        "blocks": header.block_count,
        "index_bytes": header.index_len,
        "tail_start": header.tail_start
    }))
}

pub fn reindex(path: &Path) -> Result<Value> {
    let header = reindex_file(path)?;
    Ok(json!({
        "ok": true,
        "doc_count": header.doc_count,
        "index_bytes": header.index_len,
        "blocks": header.block_count
    }))
}

// ---------------------------------------------------------------------------
// Context selection
// ---------------------------------------------------------------------------

/// Select the frame + memories a `brief`/`pack` prompt is built from.
///
/// The returned hits carry the source snippet when `snippet_lines > 0`, so the
/// library `brief` facade and the pack prompt share one selection path.
pub(crate) fn select_brief(
    path: &Path,
    options: &BriefOptions,
) -> Result<(Option<Value>, String, Vec<RecallHit>, usize)> {
    select_brief_with(path, options, &crate::store::StoreOptions::default())
}

pub(crate) fn select_brief_with(
    path: &Path,
    options: &BriefOptions,
    store_options: &crate::store::StoreOptions,
) -> Result<(Option<Value>, String, Vec<RecallHit>, usize)> {
    let mut store = Store::open_with(path, store_options)?;
    let task_arg = options.task.as_deref().unwrap_or("current");
    let (frame, query, frame_tokens, task) = match latest_frame(&mut store, Some(task_arg))? {
        Some((memory, frame)) => {
            let query = options
                .query
                .clone()
                .or_else(|| frame_query(&frame))
                .ok_or_else(|| {
                    anyhow!(
                        "frame for task {:?} has no goal or next steps; pass --query",
                        frame.task
                    )
                })?;
            let value = frame_json(&memory, &frame);
            let tokens = estimate_tokens(&serde_json::to_string(&value)?);
            (Some(value), query, tokens, Some(frame.task))
        }
        None => {
            let query = options
                .query
                .clone()
                .ok_or_else(|| anyhow!("no frame for this workspace; pass --query"))?;
            (
                None,
                query,
                0,
                (task_arg != "current").then(|| task_arg.to_string()),
            )
        }
    };
    let memory_budget = options.budget.saturating_sub(frame_tokens);
    let query_args = RecallQuery {
        query: query.clone(),
        budget_tokens: memory_budget,
        top: options.top.max(1),
        max_items: options.max_items.max(1),
        scope: options.scope.clone(),
        task,
        kind: None,
        min_trust: options
            .min_trust
            .clone()
            .or_else(|| Some("imported".to_string())),
        min_score: options.min_score,
        compact: true,
        snippet_lines: options.snippet_lines,
    };
    let (mut selected, _) = select_context_scored(&mut store, &query_args)?;
    let feedback = collect_feedback_stats(&mut store, 1024)?;
    selected.retain(|(_, memory)| {
        feedback
            .get(&memory.id)
            .is_none_or(|stats| !(stats.wrong >= 2 || (stats.stale >= 2 && stats.useful == 0)))
    });
    let mut seen_sources = HashSet::new();
    let mut hits: Vec<RecallHit> = Vec::with_capacity(selected.len());
    for (score, memory) in selected {
        let snippet = (options.snippet_lines > 0)
            .then(|| {
                let key = format!("{}\0{}", memory.source, memory.source_line.unwrap_or(0));
                seen_sources
                    .insert(key)
                    .then(|| source_snippet(&memory, &options.root, options.snippet_lines))
                    .flatten()
            })
            .flatten();
        let estimated_tokens = estimate_tokens(&crate::model::cmx_line(&memory));
        hits.push(RecallHit {
            score,
            memory,
            estimated_tokens,
            snippet,
        });
    }
    let mut memory_tokens: usize = hits.iter().map(|hit| hit.estimated_tokens).sum();
    while !hits.is_empty() && frame_tokens + memory_tokens > options.budget {
        hits.pop();
        memory_tokens = hits.iter().map(|hit| hit.estimated_tokens).sum();
    }
    Ok((frame, query, hits, frame_tokens + memory_tokens))
}

/// Build the `cm pack` prompt.
pub fn pack_prompt(path: &Path, options: &BriefOptions) -> Result<String> {
    pack_prompt_with(path, options, &crate::store::StoreOptions::default())
}

/// [`pack_prompt`] with explicit store options (read-only/disk-cache policy).
pub fn pack_prompt_with(
    path: &Path,
    options: &BriefOptions,
    store_options: &crate::store::StoreOptions,
) -> Result<String> {
    let (frame, query, selected, _) = select_brief_with(path, options, store_options)?;
    let memories: Vec<PackMemory> = selected
        .iter()
        .map(|hit| {
            let mut pack = PackMemory::from(&hit.memory);
            pack.snippet = hit.snippet.clone();
            pack
        })
        .collect();
    let mut out = String::new();
    out.push_str("You are compressing cm memory context for a coding agent.\n");
    out.push_str("Return only JSONL, one object per line. No markdown, no prose.\n");
    out.push_str("Use the supplied memories as evidence. Do not invent sources.\n");
    out.push_str("If nothing is worth storing, return an empty response.\n");
    out.push('\n');
    out.push_str("CONTEXT\n");
    out.push_str(&format!("QUERY: {query}\n"));
    if let Some(frame) = &frame {
        out.push_str("TASK\n");
        out.push_str(&serde_json::to_string(frame)?);
        out.push('\n');
    }
    out.push_str("MEMORIES\n");
    for memory in &memories {
        out.push_str(&serde_json::to_string(memory)?);
        out.push('\n');
    }
    out.push('\n');
    out.push_str("OUTPUT\n");
    out.push_str("One JSONL object per line, exactly one of:\n");
    out.push_str("{\"op\":\"remember\",\"kind\":\"fact|decision|preference|constraint|episode|procedure\",\"scope\":\"...\",\"subject\":\"...\",\"text\":\"<=800 chars\",\"source\":\"path:symbol|agent:<name>\",\"evidence\":[\"<memory-id>\"],\"confidence\":0.0}\n");
    out.push_str("{\"op\":\"supersede\",\"kind\":\"...\",\"scope\":\"...\",\"subject\":\"...\",\"text\":\"...\",\"source\":\"...\",\"supersedes\":[\"<memory-id>\"],\"evidence\":[\"<memory-id>\"]}\n");
    out.push_str("{\"op\":\"forget\",\"id\":\"<memory-id>\",\"reason\":\"...\"}\n");
    out.push('\n');
    out.push_str("Rules: evidence is required for remember and supersede. Prefer path:symbol over a bare path; use a bare path only when the whole file is the claim. Never create kind=frame or kind=feedback. Never invent a source path or symbol.\n");
    Ok(out)
}

/// Convenience for the `brief` prompt format.
pub fn brief_prompt_for(path: &Path, options: &BriefOptions) -> Result<String> {
    let (frame, query, selected, _) = select_brief(path, options)?;
    let memories: Vec<BriefMemory> = selected
        .iter()
        .map(|hit| {
            let mut brief = BriefMemory::from(&hit.memory);
            brief.snippet = hit.snippet.clone();
            brief
        })
        .collect();
    Ok(brief_prompt(&frame, &query, &memories))
}

// ---------------------------------------------------------------------------
// Export / import
// ---------------------------------------------------------------------------

pub fn export_text(path: &Path, format: ExportFormat, limit: Option<usize>) -> Result<String> {
    let mut store = Store::open(path)?;
    let limit = limit.unwrap_or(usize::MAX).max(1);
    let memories = store.scan_page(limit.min(5000), 0)?;
    let mut out = String::new();
    for memory in &memories {
        match format {
            ExportFormat::Cmx => {
                out.push_str(&cmx_line(memory));
                out.push('\n');
            }
            ExportFormat::Jsonl => {
                out.push_str(&serde_json::to_string(&MemoryOut::from(memory))?);
                out.push('\n');
            }
        }
    }
    Ok(out)
}

pub fn import_text(path: &Path, text: &str) -> Result<usize> {
    import_text_with_policy(path, text, None, false)
}

/// Import JSONL records with a source policy and optional hardening.
///
/// `harden` is the library/embedded path: caller-supplied ids, trust,
/// provenance and supersede lists are not trusted, so ids are regenerated,
/// trust is capped at `imported`, and merge/supersede metadata is stripped.
pub(crate) fn import_text_with_policy(
    path: &Path,
    text: &str,
    policy: Option<&SourcePolicy>,
    harden: bool,
) -> Result<usize> {
    Store::open(path)?;
    let store_lock = acquire_store_lock(path)?;
    // The store lock file is the writer mutex; a byte-range lock on the store
    // itself would break concurrent readers on Windows.
    let file = OpenOptions::new().read(true).append(true).open(path)?;
    let mut writer = BufWriter::with_capacity(1024 * 1024, file);
    let mut count = 0usize;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let parsed: ImportLine = serde_json::from_str(line).context("parse import line")?;
        let memory = match parsed {
            ImportLine::Full(full) => {
                if full.id.trim().is_empty() {
                    bail!("imported record has empty id");
                }
                validate_memory(&full.kind, &full.scope, &full.subject, &full.text)?;
                if let Some(trust) = &full.trust {
                    validate_trust(trust)?;
                }
                sanitize_imported_memory(full.into_memory(), policy, harden)?
            }
            ImportLine::Append(input) => {
                validate_input(&input)?;
                let root = policy.map(|policy| policy.root.as_path());
                let memory = memory_from_input_at(&input, root)?;
                sanitize_imported_memory(memory, policy, harden)?
            }
        };
        let record_type = if memory.merged { 3 } else { 2 };
        writer.write_all(&crate::store::encode_record(
            record_type,
            0,
            &memory_payload(&memory),
        )?)?;
        count += 1;
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    let file = writer
        .into_inner()
        .map_err(|error| anyhow!("flush import: {error}"))?;
    drop(file);
    drop(store_lock);
    reindex_file(path)?;
    Ok(count)
}

pub(crate) fn sanitize_imported_memory(
    mut memory: Memory,
    policy: Option<&SourcePolicy>,
    harden: bool,
) -> Result<Memory> {
    if matches!(memory.kind.as_str(), FRAME_KIND | FEEDBACK_KIND) {
        bail!(
            "import cannot create {:?} records; use frame/feedback actions",
            memory.kind
        );
    }
    if memory.scope.starts_with("__cm_") {
        bail!("import cannot write reserved scope {:?}", memory.scope);
    }
    if let Some(policy) = policy {
        policy.check(&memory.source)?;
        for source in &memory.sources {
            policy.check(source)?;
        }
    }
    if harden {
        memory.id = crate::model::new_id();
        memory.trust = "imported".to_string();
        memory.supersedes.clear();
        memory.sources.clear();
        memory.merged = false;
        memory.strategy = None;
        memory.conflict_of = None;
        memory.source_hash = None;
        memory.source_line = None;
    } else {
        // Even a faithful CLI import must not mint a trust level above what
        // an imported record can honestly claim.
        memory.trust = match memory.trust.as_str() {
            "inferred" => "inferred".to_string(),
            _ => "imported".to_string(),
        };
    }
    let now = crate::model::now_ms();
    let future_cap = now.saturating_add(86_400_000);
    if memory.created_at == 0 || memory.created_at > future_cap {
        memory.created_at = now;
    }
    if memory.valid_from == 0 || memory.valid_from > future_cap {
        memory.valid_from = now;
    }
    Ok(memory)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn run_consolidate(
    path: &Path,
    apply: bool,
    top: usize,
    limit: usize,
    offset: usize,
    threshold: f64,
) -> Result<ConsolidateOut> {
    let mut store = Store::open(path)?;
    let tail = store.tail_view()?;
    let doc_count = store.doc_count();
    let mut memories = Vec::new();
    let mut skipped = offset;
    for idx in 0..doc_count {
        if !store.is_active(idx)? {
            continue;
        }
        if skipped > 0 {
            skipped -= 1;
            continue;
        }
        if memories.len() >= limit {
            break;
        }
        let meta = store.doc_meta(idx)?;
        let memory = store.doc_memory(&meta)?;
        if !tail.hidden.contains(&memory.id)
            && memory.kind != FRAME_KIND
            && memory.kind != FEEDBACK_KIND
        {
            memories.push((idx, memory));
        }
    }
    let signatures: Vec<Option<Vec<u64>>> =
        memories.iter().map(|(_, m)| minhash_signature(m)).collect();
    let mut dsu = Dsu::new(memories.len());
    let mut pair_scores: HashMap<(usize, usize), (&'static str, f64)> = HashMap::new();
    for (i, (doc_idx, memory)) in memories.iter().enumerate() {
        let scope_id = store.scope_id(&memory.scope)?;
        let kind = kind_id(&memory.kind);
        for (j, _) in store.bm25(
            &format!("{} {}", memory.subject, memory.text),
            scope_id,
            kind,
            Some(*doc_idx),
            None,
            top.min(100),
        )? {
            let Some(pos) = memories.iter().position(|(idx, _)| *idx == j) else {
                continue;
            };
            if i == pos {
                continue;
            }
            if let Some((strategy, score)) = pair_similarity(
                &memories[i].1,
                &memories[pos].1,
                &signatures[i],
                &signatures[pos],
                threshold,
            ) {
                dsu.union(i, pos);
                let key = if i < pos { (i, pos) } else { (pos, i) };
                pair_scores
                    .entry(key)
                    .and_modify(|entry| {
                        if score > entry.1 {
                            *entry = (strategy, score);
                        }
                    })
                    .or_insert((strategy, score));
            }
        }
    }
    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..memories.len() {
        groups.entry(dsu.find(i)).or_default().push(i);
    }
    let mut output = Vec::new();
    let mut pending = Vec::new();
    for mut members in groups.into_values() {
        if members.len() < 2 {
            continue;
        }
        // The newest correction wins over an older high-confidence record;
        // confidence only breaks ties between same-age candidates.
        members.sort_by(|a, b| {
            memories[*b]
                .1
                .created_at
                .cmp(&memories[*a].1.created_at)
                .then_with(|| {
                    memories[*b]
                        .1
                        .confidence
                        .partial_cmp(&memories[*a].1.confidence)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| memories[*a].1.id.cmp(&memories[*b].1.id))
        });
        let canonical = &memories[members[0]].1;
        let sources: Vec<String> = members.iter().map(|i| memories[*i].1.id.clone()).collect();
        let (strategy, score) = members
            .iter()
            .flat_map(|a| members.iter().map(move |b| (*a, *b)))
            .filter(|(a, b)| a < b)
            .filter_map(|(a, b)| pair_scores.get(&(a, b)).copied())
            .max_by(|left, right| {
                left.1
                    .partial_cmp(&right.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap_or(("minhash", 0.0));
        output.push(MergeGroupOut {
            sources: sources.clone(),
            strategy: strategy.to_string(),
            score,
            text: preview(&canonical.text, 200),
        });
        if apply {
            pending.push(build_merged(
                canonical,
                &sources,
                &canonical.text,
                strategy,
                score,
            ));
        }
    }
    if apply {
        for merged in &pending {
            append_record(path, 3, &memory_payload(merged), false)?;
        }
    }
    Ok(ConsolidateOut {
        groups: output,
        applied: apply,
    })
}

/// Initialize a new store (`cm init`).
pub fn init(path: &Path, empty: bool) -> Result<usize> {
    create_store(path, empty)
}

/// `cm skill` text accessor (also used by the CLI/install path).
pub const SKILL_TEXT: &str = include_str!("../assets/SKILL.md");
