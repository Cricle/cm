use std::collections::{HashMap, HashSet};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Result};
use xxhash_rust::xxh3::{xxh3_128, xxh3_64_with_seed};

pub(crate) const KINDS: &[&str] = &[
    "fact",
    "decision",
    "preference",
    "constraint",
    "episode",
    "procedure",
];

pub(crate) const F_TEXT: u8 = 1;
pub(crate) const F_U64: u8 = 2;
pub(crate) const F_F64: u8 = 3;
pub(crate) const F_TEXTS: u8 = 4;

#[derive(Clone, Debug)]
pub(crate) struct Memory {
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
    memory
}

pub(crate) fn memory_payload(memory: &Memory) -> Vec<u8> {
    let mut payload = Vec::new();
    put_text(&mut payload, 1, &memory.id);
    put_text(&mut payload, 2, &memory.kind);
    put_text(&mut payload, 3, &memory.scope);
    put_text(&mut payload, 4, &memory.subject);
    put_text(&mut payload, 5, &memory.text);
    put_text(&mut payload, 6, &memory.source);
    put_f64(&mut payload, 7, memory.confidence);
    put_texts(&mut payload, 8, &memory.sources);
    put_u64(&mut payload, 9, memory.created_at);
    if let Some(strategy) = &memory.strategy {
        put_text(&mut payload, 10, strategy);
    }
    if let Some(score) = memory.score {
        put_f64(&mut payload, 11, score);
    }
    payload
}

pub(crate) fn decode_memory(payload: &[u8]) -> Result<Memory> {
    let fields = decode_fields(payload)?;
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
        created_at: get_u64(&fields, 9).unwrap_or(0),
        strategy: get_text(&fields, 10).map(ToString::to_string),
        score: get_f64(&fields, 11),
        merged: false,
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

pub(crate) fn cmx_line(memory: &Memory) -> String {
    format!(
        "CM1{}\t{}\t{}\t{}\t{:.2}\t{}\t{}\t{}\t{}",
        if memory.merged { "M" } else { "F" },
        clean_field(&memory.id),
        clean_field(&memory.kind),
        clean_field(&memory.scope),
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

pub(crate) fn parse_ids(raw: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    raw.split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .filter(|id| seen.insert((*id).to_string()))
        .map(ToString::to_string)
        .collect()
}

fn put_text(out: &mut Vec<u8>, id: u8, value: &str) {
    out.push(id);
    out.push(F_TEXT);
    out.extend_from_slice(&(value.len() as u32).to_le_bytes());
    out.extend_from_slice(value.as_bytes());
}

fn put_u64(out: &mut Vec<u8>, id: u8, value: u64) {
    out.push(id);
    out.push(F_U64);
    out.extend_from_slice(&8u32.to_le_bytes());
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_f64(out: &mut Vec<u8>, id: u8, value: f64) {
    out.push(id);
    out.push(F_F64);
    out.extend_from_slice(&8u32.to_le_bytes());
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_texts<S: AsRef<str>>(out: &mut Vec<u8>, id: u8, values: &[S]) {
    let size = 4 + values
        .iter()
        .map(|value| 4 + value.as_ref().len())
        .sum::<usize>();
    out.push(id);
    out.push(F_TEXTS);
    out.extend_from_slice(&(size as u32).to_le_bytes());
    out.extend_from_slice(&(values.len() as u32).to_le_bytes());
    for value in values {
        let value = value.as_ref();
        out.extend_from_slice(&(value.len() as u32).to_le_bytes());
        out.extend_from_slice(value.as_bytes());
    }
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
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "in", "is", "it", "of", "on",
    "or", "that", "the", "this", "to", "with",
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

pub(crate) const MINHASH_K: usize = 128;

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

pub(crate) fn minhash_similarity(left: &Option<Vec<u64>>, right: &Option<Vec<u64>>) -> f64 {
    let (Some(left), Some(right)) = (left, right) else {
        return 0.0;
    };
    if left.len() != right.len() || left.is_empty() {
        return 0.0;
    }
    left.iter().zip(right).filter(|(a, b)| a == b).count() as f64 / left.len() as f64
}

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

pub(crate) struct Dsu {
    parent: Vec<usize>,
    rank: Vec<usize>,
}

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
