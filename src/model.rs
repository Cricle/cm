use std::collections::HashMap;
#[cfg(feature = "cli")]
use std::collections::HashSet;
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use xxhash_rust::xxh3::xxh3_128;
#[cfg(feature = "cli")]
use xxhash_rust::xxh3::xxh3_64_with_seed;

use crate::source::{source_anchor, source_anchor_at};

#[derive(Deserialize)]
pub(crate) struct AppendInput {
    pub(crate) kind: String,
    pub(crate) scope: String,
    pub(crate) subject: String,
    pub(crate) text: String,
    #[serde(default)]
    pub(crate) source: String,
    #[serde(default = "default_confidence")]
    pub(crate) confidence: f64,
    #[serde(default)]
    pub(crate) valid_from: u64,
    #[serde(default)]
    pub(crate) valid_to: Option<u64>,
    #[serde(default)]
    pub(crate) supersedes: Vec<String>,
    #[serde(default)]
    pub(crate) conflict_of: Option<String>,
    #[serde(default)]
    pub(crate) trust: Option<String>,
    #[serde(default)]
    pub(crate) source_commit: Option<String>,
}

pub(crate) fn default_confidence() -> f64 {
    0.8
}

#[derive(Deserialize)]
#[serde(untagged)]
pub(crate) enum ImportLine {
    Full(MemoryOut),
    Append(AppendInput),
}

#[derive(Serialize, Deserialize)]
pub(crate) struct MemoryOut {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) scope: String,
    pub(crate) subject: String,
    pub(crate) text: String,
    pub(crate) source: String,
    pub(crate) confidence: f64,
    #[serde(default)]
    pub(crate) sources: Vec<String>,
    #[serde(default)]
    pub(crate) merged: bool,
    #[serde(default)]
    pub(crate) created_at: u64,
    #[serde(default)]
    pub(crate) strategy: Option<String>,
    #[serde(default)]
    pub(crate) score: Option<f64>,
    #[serde(default)]
    pub(crate) valid_from: u64,
    #[serde(default)]
    pub(crate) valid_to: Option<u64>,
    #[serde(default)]
    pub(crate) supersedes: Vec<String>,
    #[serde(default)]
    pub(crate) conflict_of: Option<String>,
    #[serde(default)]
    pub(crate) trust: Option<String>,
    #[serde(default)]
    pub(crate) source_commit: Option<String>,
    #[serde(default)]
    pub(crate) source_hash: Option<String>,
    #[serde(default)]
    pub(crate) source_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) snippet: Option<String>,
}

impl MemoryOut {
    pub(crate) fn into_memory(self) -> crate::model::Memory {
        crate::model::Memory {
            id: self.id,
            kind: self.kind,
            scope: self.scope,
            subject: self.subject,
            text: self.text,
            source: self.source,
            confidence: self.confidence.clamp(0.0, 1.0),
            sources: self.sources,
            created_at: self.created_at,
            strategy: self.strategy,
            score: self.score,
            merged: self.merged,
            valid_from: if self.valid_from == 0 {
                self.created_at
            } else {
                self.valid_from
            },
            valid_to: self.valid_to,
            supersedes: self.supersedes,
            conflict_of: self.conflict_of,
            trust: self.trust.unwrap_or_else(|| "authored".to_string()),
            source_commit: self.source_commit,
            source_hash: self.source_hash,
            source_line: self.source_line,
        }
    }
}

impl From<&crate::model::Memory> for MemoryOut {
    fn from(memory: &crate::model::Memory) -> Self {
        Self {
            id: memory.id.clone(),
            kind: memory.kind.clone(),
            scope: memory.scope.clone(),
            subject: memory.subject.clone(),
            text: memory.text.clone(),
            source: memory.source.clone(),
            confidence: memory.confidence,
            sources: memory.sources.clone(),
            merged: memory.merged,
            created_at: memory.created_at,
            strategy: memory.strategy.clone(),
            score: memory.score,
            valid_from: memory.valid_from,
            valid_to: memory.valid_to,
            supersedes: memory.supersedes.clone(),
            conflict_of: memory.conflict_of.clone(),
            trust: Some(memory.trust.clone()),
            source_commit: memory.source_commit.clone(),
            source_hash: memory.source_hash.clone(),
            source_line: memory.source_line,
            snippet: None,
        }
    }
}

pub(crate) fn validate_trust(trust: &str) -> Result<()> {
    if matches!(trust, "authored" | "verified" | "imported" | "inferred") {
        Ok(())
    } else {
        bail!("invalid trust {trust:?}; expected authored/verified/imported/inferred")
    }
}

pub(crate) fn memory_from_input(input: &AppendInput) -> Result<crate::model::Memory> {
    memory_from_input_at(input, None)
}

pub(crate) fn memory_from_input_at(
    input: &AppendInput,
    root: Option<&std::path::Path>,
) -> Result<crate::model::Memory> {
    validate_input(input)?;
    let trust = input.trust.as_deref().unwrap_or("authored");
    validate_trust(trust)?;
    let mut memory = new_memory(
        &input.kind,
        &input.scope,
        &input.subject,
        &input.text,
        &input.source,
        input.confidence.clamp(0.0, 1.0),
        Vec::new(),
        None,
        None,
    );
    memory.valid_from = if input.valid_from == 0 {
        crate::model::now_ms()
    } else {
        input.valid_from
    };
    memory.valid_to = input.valid_to;
    memory.supersedes = input.supersedes.clone();
    memory.conflict_of = input.conflict_of.clone();
    memory.trust = trust.to_string();
    memory.source_commit = input.source_commit.clone();
    let anchor = match root {
        Some(root) => source_anchor_at(&input.source, root),
        None => source_anchor(&input.source),
    };
    if let Some((line, hash)) = anchor {
        memory.source_hash = Some(hash);
        memory.source_line = (line > 0).then_some(line);
    }
    Ok(memory)
}

pub(crate) fn validate_memory(kind: &str, scope: &str, subject: &str, text: &str) -> Result<()> {
    if subject.trim().is_empty() || text.trim().is_empty() {
        bail!("subject and text must not be empty");
    }
    if scope.trim().is_empty() {
        bail!("scope must not be empty");
    }
    if !KINDS.contains(&kind) {
        bail!("invalid kind {:?}", kind);
    }
    Ok(())
}

pub(crate) fn validate_input(input: &AppendInput) -> Result<()> {
    validate_memory(&input.kind, &input.scope, &input.subject, &input.text)
}

pub(crate) fn put_text(out: &mut Vec<u8>, id: u8, value: &str) {
    out.push(id);
    out.push(1);
    out.extend_from_slice(&(value.len() as u32).to_le_bytes());
    out.extend_from_slice(value.as_bytes());
}
pub(crate) fn put_u64(out: &mut Vec<u8>, id: u8, value: u64) {
    out.push(id);
    out.push(2);
    out.extend_from_slice(&8u32.to_le_bytes());
    out.extend_from_slice(&value.to_le_bytes());
}

pub(crate) const KINDS: &[&str] = &[
    "fact",
    "decision",
    "preference",
    "constraint",
    "episode",
    "procedure",
    "frame",
    "feedback",
];

pub(crate) const F_TEXT: u8 = 1;
pub(crate) const F_U64: u8 = 2;
pub(crate) const F_F64: u8 = 3;
pub(crate) const F_TEXTS: u8 = 4;

/// A durable memory record returned by the library API.
#[derive(Clone, Debug)]
pub struct Memory {
    pub id: String,
    pub kind: String,
    pub scope: String,
    pub subject: String,
    pub text: String,
    pub source: String,
    pub confidence: f64,
    pub sources: Vec<String>,
    pub created_at: u64,
    pub strategy: Option<String>,
    pub score: Option<f64>,
    pub merged: bool,
    pub valid_from: u64,
    pub valid_to: Option<u64>,
    pub supersedes: Vec<String>,
    pub conflict_of: Option<String>,
    pub trust: String,
    pub source_commit: Option<String>,
    pub source_hash: Option<String>,
    pub source_line: Option<u32>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn new_memory(
    kind: &str,
    scope: &str,
    subject: &str,
    text: &str,
    source: &str,
    confidence: f64,
    sources: Vec<String>,
    strategy: Option<String>,
    score: Option<f64>,
) -> Memory {
    Memory {
        id: new_id(),
        kind: kind.to_string(),
        scope: scope.to_string(),
        subject: subject.to_string(),
        text: text.to_string(),
        source: source.to_string(),
        confidence,
        sources,
        created_at: now_ms(),
        strategy,
        score,
        merged: false,
        valid_from: now_ms(),
        valid_to: None,
        supersedes: Vec::new(),
        conflict_of: None,
        trust: "authored".to_string(),
        source_commit: None,
        source_hash: None,
        source_line: None,
    }
}

pub(crate) fn build_merged(
    canonical: &Memory,
    sources: &[String],
    text: &str,
    strategy: &str,
    score: f64,
) -> Memory {
    let mut memory = new_memory(
        &canonical.kind,
        &canonical.scope,
        &canonical.subject,
        text,
        &canonical.source,
        canonical.confidence,
        sources.to_vec(),
        Some(strategy.to_string()),
        Some(score),
    );
    memory.merged = true;
    memory.valid_from = now_ms();
    memory.valid_to = None;
    memory.supersedes = Vec::new();
    memory.conflict_of = None;
    memory.trust = canonical.trust.clone();
    memory.source_commit = canonical.source_commit.clone();
    memory.source_hash = canonical.source_hash.clone();
    memory.source_line = canonical.source_line;
    memory
}

const COMPACT_MARKER: u8 = 0xF1;

fn put_varint(out: &mut Vec<u8>, mut value: usize) {
    while value >= 0x80 {
        out.push((value as u8 & 0x7F) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn take_varint(data: &[u8], cursor: &mut usize) -> Result<usize> {
    let mut value = 0usize;
    let mut shift = 0u32;
    loop {
        let byte = *data
            .get(*cursor)
            .ok_or_else(|| anyhow!("truncated varint"))?;
        *cursor += 1;
        value |= ((byte & 0x7F) as usize) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
        if shift >= usize::BITS {
            bail!("varint overflow");
        }
    }
}

fn compact_field(id: u8, mask: &mut u32, out: &mut Vec<u8>, value: &[u8]) {
    *mask |= 1u32 << (id - 1);
    out.extend_from_slice(value);
}

fn compact_text(id: u8, mask: &mut u32, out: &mut Vec<u8>, value: &str) {
    let mut field = Vec::with_capacity(value.len() + 3);
    put_varint(&mut field, value.len());
    field.extend_from_slice(value.as_bytes());
    compact_field(id, mask, out, &field);
}

fn compact_u64(id: u8, mask: &mut u32, out: &mut Vec<u8>, value: u64) {
    let mut field = Vec::with_capacity(10);
    put_varint(&mut field, value as usize);
    compact_field(id, mask, out, &field);
}

fn compact_f64(id: u8, mask: &mut u32, out: &mut Vec<u8>, value: f64) {
    compact_field(id, mask, out, &value.to_le_bytes());
}

fn compact_texts<S: AsRef<str>>(id: u8, mask: &mut u32, out: &mut Vec<u8>, values: &[S]) {
    let mut field = Vec::new();
    put_varint(&mut field, values.len());
    for value in values {
        let value = value.as_ref();
        put_varint(&mut field, value.len());
        field.extend_from_slice(value.as_bytes());
    }
    compact_field(id, mask, out, &field);
}

/// Schema-aware compact payload. Field order is fixed by schema `0xF1`;
/// a 19-bit presence map removes per-field IDs/types, and defaults are omitted.
pub(crate) fn memory_payload(memory: &Memory) -> Vec<u8> {
    let mut mask = 0u32;
    let mut body = Vec::new();
    compact_text(1, &mut mask, &mut body, &memory.id);
    compact_text(2, &mut mask, &mut body, &memory.kind);
    compact_text(3, &mut mask, &mut body, &memory.scope);
    compact_text(4, &mut mask, &mut body, &memory.subject);
    compact_text(5, &mut mask, &mut body, &memory.text);
    if !memory.source.is_empty() {
        compact_text(6, &mut mask, &mut body, &memory.source);
    }
    if memory.confidence != 0.9 {
        compact_f64(7, &mut mask, &mut body, memory.confidence);
    }
    if !memory.sources.is_empty() {
        compact_texts(8, &mut mask, &mut body, &memory.sources);
    }
    compact_u64(9, &mut mask, &mut body, memory.created_at);
    if let Some(strategy) = &memory.strategy {
        compact_text(10, &mut mask, &mut body, strategy);
    }
    if let Some(score) = memory.score {
        compact_f64(11, &mut mask, &mut body, score);
    }
    if memory.valid_from != memory.created_at {
        compact_u64(12, &mut mask, &mut body, memory.valid_from);
    }
    if let Some(valid_to) = memory.valid_to {
        compact_u64(13, &mut mask, &mut body, valid_to);
    }
    if !memory.supersedes.is_empty() {
        compact_texts(14, &mut mask, &mut body, &memory.supersedes);
    }
    if let Some(conflict_of) = &memory.conflict_of {
        compact_text(15, &mut mask, &mut body, conflict_of);
    }
    if memory.trust != "authored" {
        compact_text(16, &mut mask, &mut body, &memory.trust);
    }
    if let Some(source_commit) = &memory.source_commit {
        compact_text(17, &mut mask, &mut body, source_commit);
    }
    if let Some(source_hash) = &memory.source_hash {
        compact_text(18, &mut mask, &mut body, source_hash);
    }
    if let Some(source_line) = memory.source_line {
        compact_u64(19, &mut mask, &mut body, u64::from(source_line));
    }
    let mut out = Vec::with_capacity(5 + body.len());
    out.push(COMPACT_MARKER);
    out.extend_from_slice(&mask.to_le_bytes());
    out.extend_from_slice(&body);
    out
}

fn take_compact_text(data: &[u8], cursor: &mut usize) -> Result<String> {
    let len = take_varint(data, cursor)?;
    let end = cursor
        .checked_add(len)
        .context("compact text length overflow")?;
    if end > data.len() {
        bail!("truncated compact text");
    }
    let value = String::from_utf8(data[*cursor..end].to_vec())?;
    *cursor = end;
    Ok(value)
}

fn take_compact_u64(data: &[u8], cursor: &mut usize) -> Result<u64> {
    Ok(take_varint(data, cursor)? as u64)
}

fn take_compact_texts(data: &[u8], cursor: &mut usize) -> Result<Vec<String>> {
    let count = take_varint(data, cursor)?;
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        values.push(take_compact_text(data, cursor)?);
    }
    Ok(values)
}

fn decode_compact_memory(payload: &[u8]) -> Result<Memory> {
    if payload.len() < 5 || payload[0] != COMPACT_MARKER {
        bail!("bad compact memory payload");
    }
    let mask = u32::from_le_bytes(payload[1..5].try_into().unwrap());
    let mut cursor = 5usize;
    let mut fields: HashMap<u8, Field> = HashMap::new();
    for id in 1..=19u8 {
        if mask & (1u32 << (id - 1)) == 0 {
            continue;
        }
        let field = match id {
            7 | 11 => {
                if payload.len() - cursor < 8 {
                    bail!("truncated compact f64");
                }
                let value = f64::from_le_bytes(payload[cursor..cursor + 8].try_into().unwrap());
                cursor += 8;
                Field::F64(value)
            }
            8 | 14 => Field::Texts(take_compact_texts(payload, &mut cursor)?),
            9 | 12 | 13 | 19 => Field::U64(take_compact_u64(payload, &mut cursor)?),
            _ => Field::Text(take_compact_text(payload, &mut cursor)?),
        };
        fields.insert(id, field);
    }
    if cursor != payload.len() {
        bail!("trailing compact payload bytes");
    }
    let created_at = get_u64(&fields, 9).unwrap_or(0);
    let valid_from = get_u64(&fields, 12).unwrap_or(created_at);
    Ok(Memory {
        id: get_text(&fields, 1)
            .ok_or_else(|| anyhow!("memory missing id"))?
            .to_string(),
        kind: get_text(&fields, 2)
            .ok_or_else(|| anyhow!("memory missing kind"))?
            .to_string(),
        scope: get_text(&fields, 3)
            .ok_or_else(|| anyhow!("memory missing scope"))?
            .to_string(),
        subject: get_text(&fields, 4)
            .ok_or_else(|| anyhow!("memory missing subject"))?
            .to_string(),
        text: get_text(&fields, 5)
            .ok_or_else(|| anyhow!("memory missing text"))?
            .to_string(),
        source: get_text(&fields, 6).unwrap_or("").to_string(),
        confidence: get_f64(&fields, 7).unwrap_or(0.9),
        sources: get_texts(&fields, 8),
        created_at,
        strategy: get_text(&fields, 10).map(ToString::to_string),
        score: get_f64(&fields, 11),
        merged: false,
        valid_from,
        valid_to: get_u64(&fields, 13),
        supersedes: get_texts(&fields, 14),
        conflict_of: get_text(&fields, 15).map(ToString::to_string),
        trust: get_text(&fields, 16).unwrap_or("authored").to_string(),
        source_commit: get_text(&fields, 17).map(ToString::to_string),
        source_hash: get_text(&fields, 18).map(ToString::to_string),
        source_line: get_u64(&fields, 19).map(|line| line as u32),
    })
}

pub(crate) fn decode_memory(payload: &[u8]) -> Result<Memory> {
    if payload.first() == Some(&COMPACT_MARKER) {
        return decode_compact_memory(payload);
    }
    let fields = decode_fields(payload)?;
    let created_at = get_u64(&fields, 9).unwrap_or(0);
    let valid_from = get_u64(&fields, 12).unwrap_or(created_at);
    Ok(Memory {
        id: get_text(&fields, 1)
            .ok_or_else(|| anyhow!("memory missing id"))?
            .to_string(),
        kind: get_text(&fields, 2)
            .ok_or_else(|| anyhow!("memory missing kind"))?
            .to_string(),
        scope: get_text(&fields, 3)
            .ok_or_else(|| anyhow!("memory missing scope"))?
            .to_string(),
        subject: get_text(&fields, 4)
            .ok_or_else(|| anyhow!("memory missing subject"))?
            .to_string(),
        text: get_text(&fields, 5)
            .ok_or_else(|| anyhow!("memory missing text"))?
            .to_string(),
        source: get_text(&fields, 6).unwrap_or("").to_string(),
        confidence: get_f64(&fields, 7).unwrap_or(0.8),
        sources: get_texts(&fields, 8),
        created_at,
        strategy: get_text(&fields, 10).map(ToString::to_string),
        score: get_f64(&fields, 11),
        merged: false,
        valid_from,
        valid_to: get_u64(&fields, 13),
        supersedes: get_texts(&fields, 14),
        conflict_of: get_text(&fields, 15).map(ToString::to_string),
        trust: get_text(&fields, 16).unwrap_or("authored").to_string(),
        source_commit: get_text(&fields, 17).map(ToString::to_string),
        source_hash: get_text(&fields, 18).map(ToString::to_string),
        source_line: get_u64(&fields, 19).map(|line| line as u32),
    })
}

pub(crate) fn content_hash(memory: &Memory) -> String {
    let normalized = memory
        .text
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let input = format!(
        "{}\0{}\0{}\0{}",
        memory.kind, memory.scope, memory.subject, normalized
    );
    format!("{:032x}", xxh3_128(input.as_bytes()))
}

pub(crate) fn new_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let value = format!(
        "{nanos}:{}:{}",
        process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    format!("{:032x}", xxh3_128(value.as_bytes()))
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub(crate) fn kind_id(kind: &str) -> Option<u8> {
    KINDS.iter().position(|item| *item == kind).map(|i| i as u8)
}

pub(crate) fn preview(text: &str, max_chars: usize) -> String {
    let mut out = text.chars().take(max_chars).collect::<String>();
    if text.chars().count() > max_chars {
        out.push_str("...");
    }
    out
}

/// Conservative token estimate for recall budgets. CJK characters are counted
/// as one token; other non-space characters as a quarter token.
pub(crate) fn estimate_tokens(text: &str) -> usize {
    let mut tokens = 0.0f64;
    for character in text.chars() {
        if character.is_whitespace() {
            continue;
        }
        tokens += if is_cjk(character) { 1.0 } else { 0.25 };
    }
    tokens.ceil() as usize
}

/// Query-focused excerpt used by the compact recall projection. It keeps the
/// first matching query term in view and falls back to the text prefix.
pub(crate) fn memory_snippet(memory: &Memory, query: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    if memory.text.chars().count() <= max_chars {
        return memory.text.clone();
    }
    let lower = memory.text.to_lowercase();
    let mut matched: Option<usize> = None;
    for token in tokenize(query) {
        if token.chars().count() < 2 {
            continue;
        }
        if let Some(position) = lower.find(&token) {
            matched = Some(matched.map_or(position, |best| best.min(position)));
        }
    }
    let byte_start = matched.unwrap_or(0);
    let char_start = memory.text[..byte_start].chars().count();
    let start = char_start.saturating_sub(max_chars / 3);
    let mut snippet: String = memory.text.chars().skip(start).take(max_chars).collect();
    let total = memory.text.chars().count();
    if start > 0 {
        snippet.insert(0, '…');
    }
    if start + max_chars < total {
        snippet.push('…');
    }
    snippet
}

pub(crate) fn cmx_line(memory: &Memory) -> String {
    format!(
        "CM1{}	{}	{}	{}	{}	{}	{:.2}	{}	{}	{}	{}",
        if memory.merged { "M" } else { "F" },
        clean_field(&memory.id),
        clean_field(&memory.kind),
        clean_field(&memory.scope),
        clean_field(&memory.trust),
        memory.valid_from,
        memory.confidence,
        clean_field(&memory.subject),
        clean_field(&memory.text),
        clean_field(&memory.source),
        memory
            .sources
            .iter()
            .map(|id| clean_field(id))
            .collect::<Vec<_>>()
            .join(",")
    )
}

fn clean_field(value: &str) -> String {
    value.replace(['\t', '\n', '\r'], " ")
}

#[cfg(feature = "cli")]
pub(crate) fn parse_ids(raw: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    raw.split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .filter(|id| seen.insert((*id).to_string()))
        .map(ToString::to_string)
        .collect()
}

pub(crate) enum Field {
    Text(String),
    U64(u64),
    F64(f64),
    Texts(Vec<String>),
}

pub(crate) fn decode_fields(bytes: &[u8]) -> Result<HashMap<u8, Field>> {
    let mut fields = HashMap::new();
    let mut offset = 0usize;
    while offset < bytes.len() {
        if bytes.len() - offset < 6 {
            bail!("truncated field");
        }
        let id = bytes[offset];
        let kind = bytes[offset + 1];
        let len = u32::from_le_bytes(bytes[offset + 2..offset + 6].try_into().unwrap()) as usize;
        offset += 6;
        if bytes.len() - offset < len {
            bail!("truncated field value");
        }
        let raw = &bytes[offset..offset + len];
        offset += len;
        let field = match kind {
            F_TEXT => Field::Text(String::from_utf8(raw.to_vec())?),
            F_U64 if len == 8 => Field::U64(u64::from_le_bytes(raw.try_into().unwrap())),
            F_F64 if len == 8 => Field::F64(f64::from_le_bytes(raw.try_into().unwrap())),
            F_TEXTS => {
                if len < 4 {
                    bail!("bad text array");
                }
                let count = u32::from_le_bytes(raw[0..4].try_into().unwrap()) as usize;
                let mut cursor = 4usize;
                let mut values = Vec::with_capacity(count);
                for _ in 0..count {
                    if raw.len() - cursor < 4 {
                        bail!("bad text array length");
                    }
                    let size =
                        u32::from_le_bytes(raw[cursor..cursor + 4].try_into().unwrap()) as usize;
                    cursor += 4;
                    if raw.len() - cursor < size {
                        bail!("bad text array value");
                    }
                    values.push(String::from_utf8(raw[cursor..cursor + size].to_vec())?);
                    cursor += size;
                }
                Field::Texts(values)
            }
            _ => bail!("unknown field type {kind}"),
        };
        fields.insert(id, field);
    }
    Ok(fields)
}

pub(crate) fn get_text(fields: &HashMap<u8, Field>, id: u8) -> Option<&str> {
    match fields.get(&id) {
        Some(Field::Text(value)) => Some(value),
        _ => None,
    }
}

fn get_u64(fields: &HashMap<u8, Field>, id: u8) -> Option<u64> {
    match fields.get(&id) {
        Some(Field::U64(value)) => Some(*value),
        _ => None,
    }
}

fn get_f64(fields: &HashMap<u8, Field>, id: u8) -> Option<f64> {
    match fields.get(&id) {
        Some(Field::F64(value)) => Some(*value),
        _ => None,
    }
}

fn get_texts(fields: &HashMap<u8, Field>, id: u8) -> Vec<String> {
    match fields.get(&id) {
        Some(Field::Texts(values)) => values.clone(),
        _ => Vec::new(),
    }
}

const STOP_WORDS: &[&str] = &[
    "a",
    "an",
    "and",
    "are",
    "as",
    "at",
    "be",
    "by",
    "for",
    "from",
    "in",
    "is",
    "it",
    "of",
    "on",
    "or",
    "that",
    "the",
    "this",
    "to",
    "with",
    "how",
    "what",
    "which",
    "why",
    "when",
    "where",
    "do",
    "does",
    "did",
    "i",
    "we",
    "you",
    "can",
    "could",
    "should",
    "would",
    "recommend",
    "recommended",
    "setting",
    "configure",
];

pub(crate) fn tokenize(text: &str) -> Vec<String> {
    let text = split_camel_case(text);
    let mut tokens = Vec::new();
    let mut word = String::new();
    let mut cjk = String::new();
    for character in text.chars() {
        if is_cjk(character) {
            flush_word(&mut word, &mut tokens);
            cjk.push(character);
        } else if character.is_alphanumeric() {
            flush_cjk(&mut cjk, &mut tokens);
            word.push(character);
        } else {
            flush_word(&mut word, &mut tokens);
            flush_cjk(&mut cjk, &mut tokens);
        }
    }
    flush_word(&mut word, &mut tokens);
    flush_cjk(&mut cjk, &mut tokens);
    tokens
}

fn flush_word(word: &mut String, tokens: &mut Vec<String>) {
    if word.is_empty() {
        return;
    }
    let lowered = word.to_lowercase();
    word.clear();
    if !STOP_WORDS.contains(&lowered.as_str()) {
        tokens.push(stem(&lowered));
    }
}

fn flush_cjk(cjk: &mut String, tokens: &mut Vec<String>) {
    if cjk.is_empty() {
        return;
    }
    let characters: Vec<char> = cjk.chars().collect();
    if characters.len() == 1 {
        tokens.push(characters[0].to_string());
    } else {
        for window in characters.windows(2) {
            tokens.push(window.iter().collect());
        }
    }
    cjk.clear();
}

fn is_cjk(character: char) -> bool {
    matches!(
        character,
        '\u{3400}'..='\u{4DBF}' | '\u{4E00}'..='\u{9FFF}' | '\u{F900}'..='\u{FAFF}'
    )
}

fn stem(term: &str) -> String {
    if !term.is_ascii() {
        return term.to_owned();
    }
    let length = term.len();
    if length > 4 && term.ends_with("ies") {
        return format!("{}y", &term[..length - 3]);
    }
    if length > 4
        && ["ches", "shes", "sses", "xes", "zes"]
            .iter()
            .any(|suffix| term.ends_with(suffix))
    {
        return term[..length - 2].to_owned();
    }
    if length > 3 && term.ends_with('s') && !term.ends_with("ss") {
        return term[..length - 1].to_owned();
    }
    term.to_owned()
}

fn split_camel_case(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len() + 8);
    for (index, current) in chars.iter().enumerate() {
        let previous = index.checked_sub(1).and_then(|i| chars.get(i));
        let next = chars.get(index + 1);
        if let Some(previous) = previous {
            let lower_then_upper = (previous.is_ascii_lowercase() || previous.is_ascii_digit())
                && current.is_ascii_uppercase();
            let acronym_then_word = previous.is_ascii_uppercase()
                && current.is_ascii_uppercase()
                && next.is_some_and(|next| next.is_ascii_lowercase());
            if lower_then_upper || acronym_then_word {
                out.push(' ');
            }
        }
        out.push(*current);
    }
    out
}

#[cfg(feature = "cli")]
pub(crate) const MINHASH_K: usize = 128;

#[cfg(feature = "cli")]
pub(crate) fn minhash_signature(memory: &Memory) -> Option<Vec<u64>> {
    let tokens: HashSet<String> = tokenize(&format!("{} {}", memory.subject, memory.text))
        .into_iter()
        .collect();
    if tokens.is_empty() {
        return None;
    }
    let mut signature = vec![u64::MAX; MINHASH_K];
    for token in &tokens {
        for (seed, slot) in signature.iter_mut().enumerate() {
            let hash = xxh3_64_with_seed(token.as_bytes(), seed as u64 + 1);
            if hash < *slot {
                *slot = hash;
            }
        }
    }
    Some(signature)
}

#[cfg(feature = "cli")]
pub(crate) fn minhash_similarity(left: &Option<Vec<u64>>, right: &Option<Vec<u64>>) -> f64 {
    let (Some(left), Some(right)) = (left, right) else {
        return 0.0;
    };
    if left.len() != right.len() || left.is_empty() {
        return 0.0;
    }
    left.iter().zip(right).filter(|(a, b)| a == b).count() as f64 / left.len() as f64
}

#[cfg(feature = "cli")]
pub(crate) fn pair_similarity(
    left: &Memory,
    right: &Memory,
    left_signature: &Option<Vec<u64>>,
    right_signature: &Option<Vec<u64>>,
    threshold: f64,
) -> Option<(&'static str, f64)> {
    if left.confidence.min(right.confidence) < 0.3 {
        return None;
    }
    if content_hash(left) == content_hash(right) {
        return Some(("exact", 1.0));
    }
    let score = minhash_similarity(left_signature, right_signature);
    if score >= threshold {
        Some(("minhash", score))
    } else {
        None
    }
}

#[cfg(feature = "cli")]
pub(crate) struct Dsu {
    parent: Vec<usize>,
    rank: Vec<usize>,
}

#[cfg(feature = "cli")]
impl Dsu {
    pub(crate) fn new(size: usize) -> Self {
        Self {
            parent: (0..size).collect(),
            rank: vec![0; size],
        }
    }

    pub(crate) fn find(&mut self, value: usize) -> usize {
        if self.parent[value] != value {
            let root = self.find(self.parent[value]);
            self.parent[value] = root;
        }
        self.parent[value]
    }

    pub(crate) fn union(&mut self, left: usize, right: usize) {
        let left = self.find(left);
        let right = self.find(right);
        if left == right {
            return;
        }
        if self.rank[left] < self.rank[right] {
            self.parent[left] = right;
        } else if self.rank[left] > self.rank[right] {
            self.parent[right] = left;
        } else {
            self.parent[right] = left;
            self.rank[left] += 1;
        }
    }
}
