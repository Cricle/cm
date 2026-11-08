use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{bail, Result};
use serde::Serialize;

use crate::brief::FEEDBACK_KIND;
use crate::frames::FRAME_KIND;
use crate::model::{cmx_line, estimate_tokens, kind_id, memory_snippet, preview, Memory};
use crate::store::Store;

#[derive(Clone, Debug)]
pub struct SearchQuery {
    pub query: String,
    pub top: usize,
    pub scope: Option<String>,
    pub task: Option<String>,
    pub kind: Option<String>,
    pub min_trust: Option<String>,
    pub min_score: f64,
    pub include_expired: bool,
}

impl Default for SearchQuery {
    fn default() -> Self {
        Self {
            query: String::new(),
            top: 8,
            scope: None,
            task: None,
            kind: None,
            min_trust: None,
            min_score: 0.0,
            include_expired: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct RecallQuery {
    pub query: String,
    pub budget_tokens: usize,
    pub top: usize,
    pub max_items: usize,
    pub scope: Option<String>,
    pub task: Option<String>,
    pub kind: Option<String>,
    pub min_trust: Option<String>,
    pub min_score: f64,
    pub compact: bool,
    pub snippet_lines: usize,
}

impl Default for RecallQuery {
    fn default() -> Self {
        Self {
            query: String::new(),
            budget_tokens: 1200,
            top: 16,
            max_items: 5,
            scope: None,
            task: None,
            kind: None,
            min_trust: Some("imported".to_string()),
            min_score: 0.0,
            compact: true,
            snippet_lines: 0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct RecallHit {
    pub memory: Memory,
    pub score: f64,
    pub estimated_tokens: usize,
    pub snippet: Option<String>,
}

#[derive(Clone, Debug)]
pub struct RecallResponse {
    pub query: String,
    pub estimated_tokens: usize,
    pub memories: Vec<RecallHit>,
}

#[derive(Serialize)]
pub struct SearchHit {
    pub id: String,
    pub bm25: f64,
    pub kind: String,
    pub scope: String,
    pub subject: String,
    pub preview: String,
}

#[derive(Serialize)]
pub struct SearchOut {
    pub query: String,
    pub candidates: Vec<SearchHit>,
}

pub(crate) fn search_out(path: &Path, q: &SearchQuery) -> Result<SearchOut> {
    let SearchQuery {
        query,
        top,
        scope,
        task,
        kind,
        min_score,
        min_trust,
        include_expired,
    } = q;
    let top = *top;
    let min_score = *min_score;
    let include_expired = *include_expired;
    let mut store = Store::open(path)?;
    store.set_cache_affinity(scope.as_deref(), task.as_deref());
    let tail = store.tail_view()?;
    let scope_id = match scope.as_deref() {
        Some(value) => store.scope_id(value)?,
        None => None,
    };
    let kind_id = match kind.as_deref() {
        Some(value) => match kind_id(value) {
            Some(id) => Some(id),
            None => {
                return Ok(SearchOut {
                    query: query.to_string(),
                    candidates: Vec::new(),
                });
            }
        },
        None => None,
    };
    let allow_meta = kind
        .as_deref()
        .is_some_and(|kind| kind == FRAME_KIND || kind == FEEDBACK_KIND);
    let min_rank = match min_trust.as_deref() {
        Some(value) => trust_rank(value),
        None => -1,
    };
    if min_trust.is_some() && min_rank < 0 {
        bail!("invalid trust level {:?}", min_trust);
    }
    let now = crate::model::now_ms();
    let mut hidden_docs = HashSet::new();
    for id in &tail.hidden {
        if let Some((doc_idx, _)) = store.find_doc(id)? {
            hidden_docs.insert(doc_idx);
        }
    }
    let mut hits: HashMap<String, (f64, crate::model::Memory)> = HashMap::new();
    if scope.is_none() || scope_id.is_some() {
        for (doc_idx, score) in store.bm25(
            query,
            scope_id,
            kind_id,
            None,
            Some(&hidden_docs),
            top.min(100),
        )? {
            let meta = store.doc_meta(doc_idx)?;
            let memory = store.doc_memory(&meta)?;
            if !tail.hidden.contains(&memory.id) {
                hits.insert(memory.id.clone(), (score, memory));
            }
        }
    }
    for (score, memory) in tail.bm25(query, scope.as_deref(), kind.as_deref(), None, top.min(100)) {
        hits.insert(memory.id.clone(), (score, memory));
    }
    let mut candidates: Vec<SearchHit> = hits
        .into_values()
        .filter(|(bm25, memory)| {
            (allow_meta || (memory.kind != FRAME_KIND && memory.kind != FEEDBACK_KIND))
                && *bm25 >= min_score
                && (include_expired || memory.valid_to.is_none_or(|end| end > now))
                && trust_rank(&memory.trust) >= min_rank
        })
        .map(|(bm25, memory)| SearchHit {
            id: memory.id.clone(),
            bm25,
            kind: memory.kind.clone(),
            scope: memory.scope.clone(),
            subject: memory.subject.clone(),
            preview: preview(&memory.text, 160),
        })
        .collect();
    candidates.sort_by(|a, b| {
        b.bm25
            .partial_cmp(&a.bm25)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
    candidates.truncate(top);
    let ids: Vec<String> = candidates.iter().map(|hit| hit.id.clone()).collect();
    store.prefetch_memory_ids(&ids)?;
    Ok(SearchOut {
        query: query.to_string(),
        candidates,
    })
}

pub(crate) fn trust_rank(trust: &str) -> i32 {
    match trust {
        "verified" => 3,
        "authored" => 2,
        "imported" => 1,
        "inferred" => 0,
        _ => -1,
    }
}

pub(crate) fn select_context(store: &mut Store, q: &RecallQuery) -> Result<(Vec<Memory>, usize)> {
    let (scored, estimated) = select_context_scored(store, q)?;
    Ok((
        scored.into_iter().map(|(_, memory)| memory).collect(),
        estimated,
    ))
}

pub(crate) fn select_context_scored(
    store: &mut Store,
    q: &RecallQuery,
) -> Result<(Vec<(f64, Memory)>, usize)> {
    let RecallQuery {
        query,
        budget_tokens,
        top,
        max_items,
        scope,
        task,
        kind,
        min_trust,
        min_score,
        compact,
        ..
    } = q;
    let budget = *budget_tokens;
    let top = *top;
    let max_items = *max_items;
    let min_score = *min_score;
    let compact = *compact;
    store.set_cache_affinity(scope.as_deref(), task.as_deref());
    let tail = store.tail_view()?;
    let now = crate::model::now_ms();
    let min_rank = match min_trust.as_deref() {
        Some(value) => trust_rank(value),
        None => -1,
    };
    if min_rank < 0 && min_trust.is_some() {
        bail!("invalid trust level {:?}", min_trust);
    }
    let scope_id = match scope.as_deref() {
        Some(value) => store.scope_id(value)?,
        None => None,
    };
    let kind_id = match kind.as_deref() {
        Some(value) => match kind_id(value) {
            Some(id) => Some(id),
            None => {
                bail!("invalid kind {:?}", value);
            }
        },
        None => None,
    };
    let allow_meta = kind
        .as_deref()
        .is_some_and(|kind| kind == FRAME_KIND || kind == FEEDBACK_KIND);
    let mut hits: HashMap<String, (f64, crate::model::Memory)> = HashMap::new();
    if scope.is_none() || scope_id.is_some() {
        for (doc_idx, score) in store.bm25(
            query,
            scope_id,
            kind_id,
            None,
            None,
            top.saturating_mul(4).min(256),
        )? {
            let meta = store.doc_meta(doc_idx)?;
            let memory = store.doc_memory(&meta)?;
            if tail.hidden.contains(&memory.id) {
                continue;
            }
            hits.insert(memory.id.clone(), (score, memory));
        }
    }
    for (score, memory) in tail.bm25(
        query,
        scope.as_deref(),
        kind.as_deref(),
        None,
        top.saturating_mul(4).min(256),
    ) {
        hits.insert(memory.id.clone(), (score, memory));
    }
    let mut candidates: Vec<(f64, crate::model::Memory)> = hits
        .into_values()
        .filter(|(score, memory)| {
            let not_expired = memory.valid_to.is_none_or(|end| end > now);
            let trust_ok = trust_rank(&memory.trust) >= min_rank;
            (allow_meta || (memory.kind != FRAME_KIND && memory.kind != FEEDBACK_KIND))
                && *score >= min_score
                && not_expired
                && trust_ok
        })
        .collect();
    candidates.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.id.cmp(&b.1.id))
    });

    let mut selected: Vec<(f64, Memory)> = Vec::new();
    let mut estimated = 0usize;
    for (score, memory) in candidates {
        if selected.len() >= max_items {
            break;
        }
        let mut projected = memory;
        if compact {
            if !selected.is_empty() {
                let snippet = memory_snippet(&projected, query, 240);
                projected.text = snippet;
            } else {
                let line = cmx_line(&projected);
                let line_tokens = estimate_tokens(&line);
                if estimated + line_tokens > budget {
                    let snippet = memory_snippet(&projected, query, 240);
                    projected.text = snippet;
                }
            }
        }
        let line = cmx_line(&projected);
        let line_tokens = estimate_tokens(&line);
        if estimated + line_tokens > budget && !selected.is_empty() {
            break;
        }
        if estimated + line_tokens > budget {
            continue;
        }
        estimated += line_tokens;
        selected.push((score, projected));
    }
    let ids: Vec<String> = selected
        .iter()
        .map(|(_, memory)| memory.id.clone())
        .collect();
    store.prefetch_memory_ids(&ids)?;
    Ok((selected, estimated))
}

pub(crate) fn active_memories(store: &mut Store) -> Result<Vec<crate::model::Memory>> {
    let tail = store.tail_view()?;
    let mut memories = store.active_memories_bulk()?;
    memories.retain(|memory| !tail.hidden.contains(&memory.id));
    memories.extend(tail.memories);
    Ok(memories)
}
