use std::collections::HashMap;
use std::path::Path;

#[cfg(feature = "cli")]
use anyhow::anyhow;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::model::{kind_id, memory_from_input, memory_payload, AppendInput};
#[cfg(feature = "cli")]
use crate::source::source_parts;
use crate::store::{append_record, Store};

pub(crate) const FRAME_SCOPE: &str = "__cm_frame__";
pub(crate) const FRAME_KIND: &str = "frame";

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct TaskFrame {
    pub task: String,
    pub goal: String,
    pub status: String,
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default)]
    pub next: Vec<String>,
    #[serde(default)]
    pub open: Vec<String>,
    #[serde(default)]
    pub touches: Vec<String>,
    #[serde(default)]
    pub notes: Vec<String>,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub lease_until: Option<u64>,
    pub updated_at: u64,
}

pub(crate) fn frame_subject(task: &str) -> String {
    format!("frame:{task}")
}

#[cfg(feature = "cli")]
pub(crate) fn append_values(target: &mut Vec<String>, values: Vec<String>) {
    for value in values {
        if !value.trim().is_empty() && !target.contains(&value) {
            target.push(value);
        }
    }
}

pub(crate) fn parse_frame(memory: &crate::model::Memory) -> Option<TaskFrame> {
    serde_json::from_str(&memory.text).ok()
}

pub(crate) fn collect_frames(store: &mut Store, limit: usize) -> Result<Vec<crate::model::Memory>> {
    let scope_id = store.scope_id(FRAME_SCOPE)?;
    let kind = kind_id(FRAME_KIND);
    let tail = store.tail_view()?;
    let mut hits: HashMap<String, crate::model::Memory> = HashMap::new();
    if scope_id.is_some() {
        for (doc_idx, _) in store.bm25("frame", scope_id, kind, None, None, limit.min(512))? {
            let meta = store.doc_meta(doc_idx)?;
            let memory = store.doc_memory(&meta)?;
            if !tail.hidden.contains(&memory.id) && memory.subject.starts_with("frame:") {
                hits.insert(memory.id.clone(), memory);
            }
        }
    }
    for (_, memory) in tail.bm25(
        "frame",
        Some(FRAME_SCOPE),
        Some(FRAME_KIND),
        None,
        limit.min(512),
    ) {
        if !tail.hidden.contains(&memory.id) && memory.subject.starts_with("frame:") {
            hits.insert(memory.id.clone(), memory);
        }
    }
    let mut by_task: HashMap<String, crate::model::Memory> = HashMap::new();
    for memory in hits.into_values() {
        match by_task.get(&memory.subject) {
            Some(existing) if existing.created_at >= memory.created_at => {}
            _ => {
                by_task.insert(memory.subject.clone(), memory);
            }
        }
    }
    let mut frames: Vec<_> = by_task.into_values().collect();
    frames.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| a.subject.cmp(&b.subject))
    });
    frames.truncate(limit);
    Ok(frames)
}

pub(crate) fn latest_frame(
    store: &mut Store,
    task: Option<&str>,
) -> Result<Option<(crate::model::Memory, TaskFrame)>> {
    let mut frames: Vec<(crate::model::Memory, TaskFrame)> = collect_frames(store, 512)?
        .into_iter()
        .filter_map(|memory| parse_frame(&memory).map(|frame| (memory, frame)))
        .collect();
    if let Some(task) = task.filter(|task| *task != "current") {
        return Ok(frames
            .into_iter()
            .find(|(memory, _)| memory.subject == frame_subject(task)));
    }
    let index = frames
        .iter()
        .position(|(_, frame)| frame.status == "active")
        .or_else(|| frames.iter().position(|(_, frame)| frame.status != "done"))
        .unwrap_or(0);
    if frames.is_empty() {
        return Ok(None);
    }
    Ok(Some(frames.swap_remove(index)))
}

pub(crate) fn frame_query(frame: &TaskFrame) -> Option<String> {
    let mut parts = vec![frame.goal.clone()];
    parts.extend(frame.next.iter().cloned());
    parts.extend(frame.open.iter().cloned());
    parts.extend(frame.touches.iter().cloned());
    parts.retain(|part| !part.trim().is_empty());
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

#[cfg(feature = "cli")]
pub(crate) fn frame_query_for_task(path: &Path, task: &str) -> Result<String> {
    let mut store = Store::open(path)?;
    let (_, frame) = latest_frame(&mut store, Some(task))?
        .ok_or_else(|| anyhow!("no frame for task {task:?}; pass --query"))?;
    frame_query(&frame)
        .ok_or_else(|| anyhow!("frame for task {task:?} has no goal or next steps; pass --query"))
}

pub(crate) fn frame_json(memory: &crate::model::Memory, frame: &TaskFrame) -> serde_json::Value {
    json!({
        "id": memory.id,
        "task": frame.task,
        "goal": frame.goal,
        "status": frame.status,
        "constraints": frame.constraints,
        "next": frame.next,
        "open": frame.open,
        "touches": frame.touches,
        "notes": frame.notes,
        "owner": frame.owner,
        "lease_until": frame.lease_until,
        "updated_at": frame.updated_at
    })
}

#[cfg(feature = "cli")]
pub(crate) fn path_key(path: &str) -> String {
    let trimmed = path.trim();
    let without_dot = trimmed
        .strip_prefix("./")
        .map(str::to_string)
        .unwrap_or_else(|| trimmed.to_string());
    without_dot.replace('\\', "/").to_lowercase()
}

#[cfg(feature = "cli")]
pub(crate) fn source_covers_path(source: &str, touch: &str) -> bool {
    let Some((relative, _)) = source_parts(source) else {
        return false;
    };
    let source_key = path_key(&relative.to_string_lossy());
    let touch_key = path_key(touch);
    if touch_key.is_empty() || source_key.is_empty() {
        return false;
    }
    source_key == touch_key
        || source_key.starts_with(&format!("{touch_key}:"))
        || source_key.starts_with(&format!("{touch_key}/"))
}

#[cfg(feature = "cli")]
pub(crate) fn uncovered_touches(
    frame: &TaskFrame,
    memories: &[crate::model::Memory],
) -> Vec<String> {
    frame
        .touches
        .iter()
        .filter(|touch| {
            !memories
                .iter()
                .any(|memory| source_covers_path(&memory.source, touch))
        })
        .cloned()
        .collect()
}

pub(crate) fn ensure_frame_owner(
    frame: &TaskFrame,
    owner: Option<&str>,
    action: &str,
) -> Result<()> {
    let now = crate::model::now_ms();
    if let Some(existing) = frame.owner.as_deref() {
        if frame.lease_until.is_some_and(|end| end > now) && owner != Some(existing) {
            bail!(
                "cannot {action} task {:?}: held by {} until {}; pass --owner {}",
                frame.task,
                existing,
                frame.lease_until.unwrap_or(0),
                existing
            );
        }
    }
    Ok(())
}

pub(crate) fn save_frame(
    path: &Path,
    frame: &TaskFrame,
    existing_id: Option<String>,
    source: &str,
    source_commit: Option<String>,
    locked: bool,
) -> Result<crate::model::Memory> {
    let input = AppendInput {
        kind: FRAME_KIND.to_string(),
        scope: FRAME_SCOPE.to_string(),
        subject: frame_subject(&frame.task),
        text: serde_json::to_string(frame)?,
        source: source.to_string(),
        confidence: 1.0,
        valid_from: frame.updated_at,
        valid_to: None,
        supersedes: existing_id.into_iter().collect(),
        conflict_of: None,
        trust: Some("authored".to_string()),
        source_commit,
    };
    let memory = memory_from_input(&input)?;
    if locked {
        crate::store::append_record_unlocked(
            path,
            crate::store::T_MEMORY,
            &memory_payload(&memory),
            true,
        )?;
    } else {
        append_record(path, crate::store::T_MEMORY, &memory_payload(&memory), true)?;
    }
    Ok(memory)
}
