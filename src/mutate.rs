use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::brief::{Feedback, FEEDBACK_KIND, FEEDBACK_SCOPE};
use crate::model::{
    build_merged, memory_from_input, memory_from_input_at, memory_payload, put_text, put_u64,
    AppendInput, Memory,
};
use crate::source::{source_anchor, source_anchor_at, source_parts};
use crate::store::{
    acquire_store_lock_with, append_record, append_record_unlocked, find_tail_duplicate, Store,
    T_MEMORY, T_TOMBSTONE,
};

#[derive(Deserialize)]
pub(crate) struct AbsorbOp {
    pub(crate) op: String,
    #[serde(default)]
    pub(crate) kind: Option<String>,
    #[serde(default)]
    pub(crate) scope: Option<String>,
    #[serde(default)]
    pub(crate) subject: Option<String>,
    #[serde(default)]
    pub(crate) text: Option<String>,
    #[serde(default)]
    pub(crate) source: Option<String>,
    #[serde(default)]
    pub(crate) confidence: Option<f64>,
    #[serde(default)]
    pub(crate) evidence: Vec<String>,
    #[serde(default)]
    pub(crate) id: Option<String>,
    #[serde(default)]
    pub(crate) supersedes: Vec<String>,
    #[serde(default)]
    pub(crate) reason: Option<String>,
}

pub(crate) fn parse_absorb_ops(content: &str) -> Result<Vec<AbsorbOp>> {
    let mut ops = Vec::new();
    for (index, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line == "```" || line == "```json" {
            continue;
        }
        let op: AbsorbOp = serde_json::from_str(line)
            .with_context(|| format!("line {}: invalid JSONL", index + 1))?;
        ops.push(op);
    }
    Ok(ops)
}

pub(crate) fn validate_absorb_ops(
    store: &mut Store,
    ops: &[AbsorbOp],
    allow_long_text: bool,
    root: Option<&Path>,
) -> Result<()> {
    let mut superseded = HashSet::new();
    for (index, op) in ops.iter().enumerate() {
        let line = index + 1;
        match op.op.as_str() {
            "remember" | "supersede" => {
                let kind = op
                    .kind
                    .as_deref()
                    .ok_or_else(|| anyhow!("line {line}: kind is required"))?;
                let scope = op
                    .scope
                    .as_deref()
                    .ok_or_else(|| anyhow!("line {line}: scope is required"))?;
                if scope.trim().is_empty() {
                    bail!("line {line}: scope must not be empty");
                }
                let subject = op
                    .subject
                    .as_deref()
                    .ok_or_else(|| anyhow!("line {line}: subject is required"))?;
                if subject.trim().is_empty() {
                    bail!("line {line}: subject must not be empty");
                }
                let text = op
                    .text
                    .as_deref()
                    .ok_or_else(|| anyhow!("line {line}: text is required"))?;
                if text.trim().is_empty() {
                    bail!("line {line}: text must not be empty");
                }
                let source = op
                    .source
                    .as_deref()
                    .ok_or_else(|| anyhow!("line {line}: source is required"))?;
                if source.trim().is_empty() {
                    bail!("line {line}: source must not be empty");
                }
                if matches!(kind, "frame" | "feedback") {
                    bail!("line {line}: kind {kind:?} is reserved");
                }
                if scope.starts_with("__cm_") {
                    bail!("line {line}: scope {scope:?} is reserved");
                }
                if !allow_long_text && text.chars().count() > 800 {
                    bail!("line {line}: text must be <=800 chars; pass --allow-long-text to re-anchor existing records");
                }
                if op.evidence.is_empty() {
                    bail!("line {line}: evidence is required");
                }
                for id in &op.evidence {
                    if !active_memory_exists(store, id)? {
                        bail!("line {line}: evidence memory not found: {id}");
                    }
                }
                if let Some(reason) = &op.reason {
                    if reason.chars().count() > 200 {
                        bail!("line {line}: reason must be <=200 chars");
                    }
                }
                if op.op == "supersede" {
                    let mut targets = op.supersedes.clone();
                    if let Some(id) = &op.id {
                        targets.push(id.clone());
                    }
                    if targets.is_empty() {
                        bail!("line {line}: supersede requires supersedes or id");
                    }
                    for id in targets {
                        if !superseded.insert(id.clone()) {
                            bail!("line {line}: memory {id} is superseded more than once");
                        }
                        if !active_memory_exists(store, &id)? {
                            bail!("line {line}: superseded memory not found: {id}");
                        }
                    }
                }
                let anchor = match root {
                    Some(root) => source_anchor_at(source, root),
                    None => source_anchor(source),
                };
                if source_parts(source).is_some() && anchor.is_none() {
                    bail!("line {line}: source does not anchor: {source:?}");
                }
            }
            "forget" => {
                let Some(id) = op.id.as_deref() else {
                    bail!("line {line}: forget requires id");
                };
                if !active_memory_exists(store, id)? {
                    bail!("line {line}: active memory not found: {id}");
                }
            }
            other => bail!("line {line}: invalid op {other:?}; expected remember/supersede/forget"),
        }
    }
    Ok(())
}

pub(crate) fn absorb_input(path: &Path, op: &AbsorbOp, replace: bool) -> Result<AppendInput> {
    let mut supersedes = op.supersedes.clone();
    if let Some(id) = &op.id {
        supersedes.push(id.clone());
    }
    if replace && op.op == "remember" && supersedes.is_empty() {
        if let Some(existing) = op
            .kind
            .as_deref()
            .zip(op.scope.as_deref())
            .zip(op.subject.as_deref())
            .and_then(|((kind, scope), subject)| {
                crate::store::find_subject_memory(path, scope, kind, subject).ok()
            })
            .flatten()
        {
            supersedes.push(existing.id);
        }
    }
    Ok(AppendInput {
        kind: op.kind.clone().unwrap_or_default(),
        scope: op.scope.clone().unwrap_or_default(),
        subject: op.subject.clone().unwrap_or_default(),
        text: op.text.clone().unwrap_or_default(),
        source: op.source.clone().unwrap_or_default(),
        confidence: op.confidence.unwrap_or(0.8).clamp(0.0, 1.0),
        valid_from: 0,
        valid_to: None,
        supersedes,
        conflict_of: None,
        trust: Some("authored".to_string()),
        source_commit: None,
    })
}

pub(crate) enum AbsorbOutcome {
    Written,
    Duplicate,
    Forgotten,
}

/// Apply one absorb operation. The caller must already hold the store lock.
pub(crate) fn apply_absorb_op_unlocked(
    path: &Path,
    op: &AbsorbOp,
    replace: bool,
    root: Option<&Path>,
) -> Result<AbsorbOutcome> {
    match op.op.as_str() {
        "remember" | "supersede" => {
            let input = absorb_input(path, op, replace)?;
            let memory = memory_from_input_at(&input, root)?;
            if find_tail_duplicate(path, &memory)?.is_some() {
                return Ok(AbsorbOutcome::Duplicate);
            }
            append_record_unlocked(path, T_MEMORY, &memory_payload(&memory), true)?;
            Ok(AbsorbOutcome::Written)
        }
        "forget" => {
            let id = op.id.clone().unwrap_or_default();
            append_tombstone_unlocked(path, &id, op.reason.as_deref().unwrap_or("absorb"))?;
            Ok(AbsorbOutcome::Forgotten)
        }
        other => bail!("invalid op {other:?}"),
    }
}

#[derive(Clone, Debug, Default)]
pub struct AbsorbOptions {
    pub replace: bool,
    pub dry_run: bool,
    pub allow_long_text: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct AbsorbReport {
    pub written: usize,
    pub duplicated: usize,
    pub forgotten: usize,
    pub superseded: usize,
    pub rejected: Vec<String>,
}

/// Validate and apply a JSONL batch under one lock. Validation and writes are
/// one critical section, so an `absorb` cannot interleave with another writer.
pub(crate) fn absorb(
    path: &Path,
    content: &str,
    options: &AbsorbOptions,
    timeout: Option<Duration>,
    root: Option<&Path>,
) -> Result<AbsorbReport> {
    let ops = parse_absorb_ops(content)?;
    let _lock = acquire_store_lock_with(path, timeout)?;
    let mut store = Store::open(path)?;
    validate_absorb_ops(&mut store, &ops, options.allow_long_text, root)?;
    let mut report = AbsorbReport::default();
    if options.dry_run {
        for op in &ops {
            match op.op.as_str() {
                "remember" | "supersede" => {
                    let input = absorb_input(path, op, options.replace)?;
                    let memory = memory_from_input_at(&input, root)?;
                    if find_tail_duplicate(path, &memory)?.is_some() {
                        report.duplicated += 1;
                    } else {
                        report.written += 1;
                    }
                }
                "forget" => report.forgotten += 1,
                _ => {}
            }
        }
        return Ok(report);
    }
    for op in &ops {
        match apply_absorb_op_unlocked(path, op, options.replace, root)? {
            AbsorbOutcome::Written => {
                report.written += 1;
                if op.op == "supersede" {
                    report.superseded += 1;
                }
            }
            AbsorbOutcome::Duplicate => report.duplicated += 1,
            AbsorbOutcome::Forgotten => report.forgotten += 1,
        }
    }
    Ok(report)
}

/// Forget one active memory. The existence check and tombstone append are one
/// critical section.
pub(crate) fn forget(path: &Path, id: &str, reason: &str, timeout: Option<Duration>) -> Result<()> {
    let _lock = acquire_store_lock_with(path, timeout)?;
    let mut store = Store::open(path)?;
    if !active_memory_exists(&mut store, id)? {
        bail!("active memory not found: {id}");
    }
    append_tombstone_unlocked(path, id, reason)
}

/// Merge several active memories into one record under a single lock.
pub(crate) fn merge(
    path: &Path,
    source_ids: &[String],
    text: Option<&str>,
    timeout: Option<Duration>,
) -> Result<Memory> {
    if source_ids.len() < 2 {
        bail!("merge requires at least two source ids");
    }
    let _lock = acquire_store_lock_with(path, timeout)?;
    let mut store = Store::open(path)?;
    let tail = store.tail_view()?;
    let mut selected = Vec::new();
    for id in source_ids {
        if tail.hidden.contains(id) {
            bail!("active memory not found: {id}");
        }
        let memory = if let Some((_, memory)) = store.find_doc(id)? {
            memory
        } else {
            tail.get(id)
                .cloned()
                .ok_or_else(|| anyhow!("active memory not found: {id}"))?
        };
        selected.push(memory);
    }
    // Newest record is the canonical text; confidence only breaks ties.
    selected.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| {
                b.confidence
                    .partial_cmp(&a.confidence)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.id.cmp(&b.id))
    });
    if selected
        .iter()
        .any(|memory| memory.kind != selected[0].kind || memory.scope != selected[0].scope)
    {
        bail!("manual merge requires the same kind and scope");
    }
    let merged = build_merged(
        &selected[0],
        source_ids,
        text.unwrap_or(&selected[0].text),
        "manual",
        1.0,
    );
    append_record_unlocked(path, crate::store::T_MERGE, &memory_payload(&merged), false)?;
    Ok(merged)
}

pub(crate) fn write_feedback(
    path: &Path,
    id: &str,
    signal: &str,
    note: Option<&str>,
    source: &str,
) -> Result<()> {
    let now = crate::model::now_ms();
    let feedback = Feedback {
        target: id.to_string(),
        signal: signal.to_string(),
        note: note.map(str::to_string),
        updated_at: now,
    };
    let input = AppendInput {
        kind: FEEDBACK_KIND.to_string(),
        scope: FEEDBACK_SCOPE.to_string(),
        subject: format!("feedback:{id}"),
        text: serde_json::to_string(&feedback)?,
        source: source.to_string(),
        confidence: 1.0,
        valid_from: now,
        valid_to: None,
        supersedes: Vec::new(),
        conflict_of: None,
        trust: Some("authored".to_string()),
        source_commit: None,
    };
    let memory = memory_from_input(&input)?;
    append_record(path, crate::store::T_MEMORY, &memory_payload(&memory), true)?;
    Ok(())
}

pub(crate) fn active_memory_exists(store: &mut Store, id: &str) -> Result<bool> {
    let tail = store.tail_view()?;
    Ok(!tail.hidden.contains(id) && (store.find_doc(id)?.is_some() || tail.get(id).is_some()))
}

pub(crate) fn append_tombstone_unlocked(path: &Path, id: &str, reason: &str) -> Result<()> {
    let mut payload = Vec::new();
    put_text(&mut payload, 1, id);
    put_text(&mut payload, 2, reason);
    put_u64(&mut payload, 3, crate::model::now_ms());
    append_record_unlocked(path, T_TOMBSTONE, &payload, false)?;
    Ok(())
}
