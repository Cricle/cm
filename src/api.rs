//! Library facade for embedding cm in Rust programs.
//!
//! The CLI intentionally stays the primary interface. This module exposes the
//! small, stable surface needed by a host process: initialize/open a store,
//! write one memory, fetch it, browse active memories, and run a BM25 search.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::cli::{create_store, memory_from_input, validate_input, AppendInput};
use crate::model::Memory;
use crate::store::{append_record, find_tail_duplicate, Store, T_MEMORY};

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
}

/// Result of a library write. `duplicate` is true when the same content was
/// already active and the existing record was returned instead of appending.
#[derive(Clone, Debug)]
pub struct RememberResult {
    pub memory: Memory,
    pub duplicate: bool,
}

/// Handle to one cm file. The handle is cheap; each operation opens the file
/// and uses the same mmap/index path as the CLI.
#[derive(Clone, Debug)]
pub struct Cm {
    path: PathBuf,
}

impl Cm {
    /// Create a new store. With `empty = true`, no seed memories are written.
    pub fn init(path: impl AsRef<Path>, empty: bool) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        create_store(&path, empty)?;
        Ok(Self { path })
    }

    /// Open an existing store and validate its header.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        Store::open(&path)?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one durable memory. Exact duplicate content returns the existing
    /// active record instead of creating another copy.
    pub fn remember(&self, input: NewMemory) -> Result<RememberResult> {
        let append = AppendInput {
            kind: input.kind,
            scope: input.scope,
            subject: input.subject,
            text: input.text,
            source: input.source,
            confidence: input.confidence,
            valid_from: 0,
            valid_to: None,
            supersedes: Vec::new(),
            conflict_of: None,
            trust: input.trust,
            source_commit: None,
        };
        validate_input(&append)?;
        let memory = memory_from_input(&append)?;
        if let Some(existing_id) = find_tail_duplicate(&self.path, &memory)? {
            if let Some(existing) = self.get(&existing_id)? {
                return Ok(RememberResult {
                    memory: existing,
                    duplicate: true,
                });
            }
        }
        append_record(
            &self.path,
            T_MEMORY,
            &crate::model::memory_payload(&memory),
            true,
        )?;
        Ok(RememberResult {
            memory,
            duplicate: false,
        })
    }

    /// Return one active memory by id.
    pub fn get(&self, id: &str) -> Result<Option<Memory>> {
        let mut store = Store::open(&self.path)?;
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
        let store = Store::open(&self.path)?;
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
        let mut store = Store::open(&self.path)?;
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
}
