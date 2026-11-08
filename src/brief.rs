use std::collections::HashMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::model::{estimate_tokens, kind_id};
use crate::store::Store;

pub(crate) const FEEDBACK_SCOPE: &str = "__cm_feedback__";
pub(crate) const FEEDBACK_KIND: &str = "feedback";

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Feedback {
    pub(crate) target: String,
    pub(crate) signal: String,
    #[serde(default)]
    pub(crate) note: Option<String>,
    pub(crate) updated_at: u64,
}

#[derive(Default)]
pub(crate) struct FeedbackStats {
    pub(crate) useful: u32,
    pub(crate) stale: u32,
    pub(crate) wrong: u32,
    pub(crate) last: u64,
}

#[derive(Serialize)]
pub(crate) struct BriefMemory {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) subject: String,
    pub(crate) text: String,
    pub(crate) source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source_commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) snippet: Option<String>,
}

impl From<&crate::model::Memory> for BriefMemory {
    fn from(memory: &crate::model::Memory) -> Self {
        Self {
            id: memory.id.clone(),
            kind: memory.kind.clone(),
            subject: memory.subject.clone(),
            text: memory.text.clone(),
            source: memory.source.clone(),
            source_line: memory.source_line,
            source_commit: memory.source_commit.clone(),
            snippet: None,
        }
    }
}

fn push_prompt_list(out: &mut String, label: &str, value: Option<&serde_json::Value>) {
    let Some(items) = value.and_then(serde_json::Value::as_array) else {
        return;
    };
    if items.is_empty() {
        return;
    }
    out.push_str(label);
    out.push('\n');
    for item in items {
        if let Some(item) = item.as_str() {
            out.push_str("- ");
            out.push_str(item);
            out.push('\n');
        }
    }
}

pub(crate) fn brief_prompt(
    frame: &Option<serde_json::Value>,
    query: &str,
    memories: &[BriefMemory],
) -> String {
    let mut out = String::new();
    if let Some(frame) = frame {
        out.push_str("TASK: ");
        out.push_str(frame["task"].as_str().unwrap_or(""));
        out.push('\n');
        out.push_str("STATUS: ");
        out.push_str(frame["status"].as_str().unwrap_or(""));
        out.push('\n');
        out.push_str("GOAL: ");
        out.push_str(frame["goal"].as_str().unwrap_or(""));
        out.push('\n');
        push_prompt_list(&mut out, "CONSTRAINTS:", frame.get("constraints"));
        push_prompt_list(&mut out, "NEXT:", frame.get("next"));
        push_prompt_list(&mut out, "OPEN:", frame.get("open"));
        push_prompt_list(&mut out, "TOUCHES:", frame.get("touches"));
        push_prompt_list(&mut out, "NOTES:", frame.get("notes"));
        if let Some(owner) = frame.get("owner").and_then(serde_json::Value::as_str) {
            out.push_str("OWNER: ");
            out.push_str(owner);
            out.push('\n');
        }
        if let Some(lease) = frame.get("lease_until").and_then(serde_json::Value::as_u64) {
            out.push_str("LEASE_UNTIL: ");
            out.push_str(&lease.to_string());
            out.push('\n');
        }
    }
    out.push_str("QUERY: ");
    out.push_str(query);
    out.push_str("\n\n");
    for memory in memories {
        out.push('[');
        out.push_str(&memory.id);
        out.push_str("] ");
        out.push_str(&memory.kind);
        out.push(' ');
        out.push_str(&memory.subject);
        out.push('\n');
        out.push_str(&memory.text);
        out.push('\n');
        if !memory.source.is_empty() {
            out.push_str("source: ");
            out.push_str(&memory.source);
            if let Some(line) = memory.source_line {
                out.push(':');
                out.push_str(&line.to_string());
            }
            out.push('\n');
        }
        if let Some(snippet) = &memory.snippet {
            out.push_str("snippet:\n");
            out.push_str(snippet);
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

pub(crate) fn json_tokens(memories: &[BriefMemory]) -> Result<usize> {
    let mut total = 0usize;
    for memory in memories {
        total += estimate_tokens(&serde_json::to_string(memory)?);
    }
    Ok(total)
}

pub(crate) fn collect_feedback_stats(
    store: &mut Store,
    limit: usize,
) -> Result<HashMap<String, FeedbackStats>> {
    let scope_id = store.scope_id(FEEDBACK_SCOPE)?;
    let kind = kind_id(FEEDBACK_KIND);
    let tail = store.tail_view()?;
    let mut hits: HashMap<String, crate::model::Memory> = HashMap::new();
    if scope_id.is_some() {
        for (doc_idx, _) in store.bm25("feedback", scope_id, kind, None, None, limit.min(512))? {
            let meta = store.doc_meta(doc_idx)?;
            let memory = store.doc_memory(&meta)?;
            if !tail.hidden.contains(&memory.id) && memory.subject.starts_with("feedback:") {
                hits.insert(memory.id.clone(), memory);
            }
        }
    }
    for (_, memory) in tail.bm25(
        "feedback",
        Some(FEEDBACK_SCOPE),
        Some(FEEDBACK_KIND),
        None,
        limit.min(512),
    ) {
        if !tail.hidden.contains(&memory.id) && memory.subject.starts_with("feedback:") {
            hits.insert(memory.id.clone(), memory);
        }
    }
    let mut stats: HashMap<String, FeedbackStats> = HashMap::new();
    for memory in hits.into_values() {
        let Ok(item) = serde_json::from_str::<Feedback>(&memory.text) else {
            continue;
        };
        let entry = stats.entry(item.target).or_default();
        match item.signal.as_str() {
            "useful" => entry.useful += 1,
            "stale" => entry.stale += 1,
            "wrong" => entry.wrong += 1,
            _ => {}
        }
        entry.last = entry.last.max(item.updated_at);
    }
    Ok(stats)
}
