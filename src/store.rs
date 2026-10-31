use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use memmap2::Mmap;
use xxhash_rust::xxh3::xxh3_64;

use crate::cache::Cache;
use crate::model::{decode_fields, decode_memory, get_text, kind_id, now_ms, tokenize, Memory};

pub(crate) const HEADER_LEN: usize = 128;
pub(crate) const BLOCK_ENTRY: usize = 24;
pub(crate) const DOC_SIZE: usize = 32;
pub(crate) const HASH_SLOT: usize = 16;
pub(crate) const INDEX_HEADER: usize = 128;
const MAGIC: &[u8; 4] = b"CM1\0";
const VERSION: u8 = 1;
const MAX_BODY: usize = 16 * 1024 * 1024;
const BLOCK_RAW: usize = 64 * 1024;
const ZSTD_LEVEL: i32 = 3;

const T_MEMORY: u8 = 2;
const T_MERGE: u8 = 3;
const T_TOMBSTONE: u8 = 4;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Header {
    pub created_at: u64,
    pub block_table_off: u64,
    pub block_count: u64,
    pub index_off: u64,
    pub index_len: u64,
    pub index_raw_len: u64,
    pub index_hash: u64,
    pub data_start: u64,
    pub tail_start: u64,
    pub doc_count: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct Record {
    pub rec_type: u8,
    pub raw: Vec<u8>,
}

impl Record {
    pub(crate) fn payload(&self) -> Result<&[u8]> {
        if self.raw.len() < 18 {
            bail!("short record");
        }
        let body_len = u32::from_le_bytes(self.raw[0..4].try_into().unwrap()) as usize;
        if self.raw.len() < 4 + body_len + 8 {
            bail!("short record body");
        }
        Ok(&self.raw[14..4 + body_len])
    }
}

pub(crate) struct Store {
    pub mmap: Mmap,
    pub header: Header,
    pub cache: Cache,
}

pub(crate) struct TailView {
    pub memories: Vec<Memory>,
    pub hidden: HashSet<String>,
}

#[derive(Clone)]
struct Block {
    comp: Vec<u8>,
    raw_len: u32,
    hash: u64,
}

impl Store {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        let mmap = unsafe { Mmap::map(&file)? };
        let header = decode_header(&mmap)?;
        if header.index_len == 0 {
            bail!("no index; run cm reindex");
        }
        let index_start = header.index_off as usize;
        let index_end = index_start + header.index_len as usize;
        if index_end > mmap.len() {
            bail!("index out of bounds");
        }
        let cache =
            match Cache::open_existing(path, header.index_hash, header.index_raw_len as usize) {
                Some(cache) => cache,
                None => {
                    let index_raw = zstd::bulk::decompress(
                        &mmap[index_start..index_end],
                        header.index_raw_len as usize,
                    )
                    .context("decompress index")?;
                    if xxh3_64(&index_raw) != header.index_hash {
                        bail!("index hash mismatch");
                    }
                    Cache::create(path, header.index_hash, &index_raw)?
                }
            };
        Ok(Self {
            mmap,
            header,
            cache,
        })
    }

    pub(crate) fn index(&self) -> &[u8] {
        self.cache.index()
    }

    pub(crate) fn doc_meta(&self, idx: u32) -> Result<DocMeta> {
        let ib = self.index();
        let docs_off = read_u64(ib, 24) as usize;
        let offset = docs_off + idx as usize * DOC_SIZE;
        Ok(DocMeta {
            block_id: read_u32(ib, offset),
            offset_in_block: read_u32(ib, offset + 4),
            record_len: read_u32(ib, offset + 8),
        })
    }

    pub(crate) fn doc_memory(&mut self, meta: &DocMeta) -> Result<Memory> {
        let data: &[u8] = &self.mmap;
        let header = self.header;
        let cache = &mut self.cache;
        let block = cache.get_block(meta.block_id, || {
            decompress_block(data, &header, meta.block_id)
        })?;
        let start = meta.offset_in_block as usize;
        let end = start + meta.record_len as usize;
        if end > block.len() {
            bail!("doc record out of block");
        }
        parse_memory_record(&block[start..end])
    }

    pub(crate) fn is_active(&self, idx: u32) -> Result<bool> {
        let ib = self.index();
        let active_off = read_u64(ib, 48) as usize;
        Ok(ib[active_off + idx as usize / 8] & (1 << (idx % 8)) != 0)
    }

    pub(crate) fn active_count(&self) -> u64 {
        let ib = self.index();
        let active_off = read_u64(ib, 48) as usize;
        let active_len = (self.doc_count() as usize).div_ceil(8);
        ib[active_off..active_off + active_len]
            .iter()
            .map(|byte| byte.count_ones() as u64)
            .sum()
    }

    pub(crate) fn doc_count(&self) -> u32 {
        let ib = self.index();
        read_u32(ib, 8)
    }

    pub(crate) fn find_doc(&mut self, id: &str) -> Result<Option<(u32, Memory)>> {
        let (hash_cap, hash_off) = {
            let ib = self.index();
            (read_u32(ib, 20), read_u64(ib, 32) as usize)
        };
        let hash = xxh3_64(id.as_bytes());
        let mut slot = (hash as usize) & (hash_cap as usize - 1);
        for _ in 0..hash_cap {
            let (entry_hash, doc_idx) = {
                let ib = self.index();
                let offset = hash_off + slot * HASH_SLOT;
                (read_u64(ib, offset), read_u32(ib, offset + 8))
            };
            if doc_idx == u32::MAX {
                return Ok(None);
            }
            if entry_hash == hash {
                let meta = self.doc_meta(doc_idx)?;
                if self.is_active(doc_idx)? {
                    let memory = self.doc_memory(&meta)?;
                    if memory.id == id {
                        return Ok(Some((doc_idx, memory)));
                    }
                }
            }
            slot = (slot + 1) & (hash_cap as usize - 1);
        }
        Ok(None)
    }

    pub(crate) fn scope_id(&self, scope: &str) -> Option<u32> {
        let ib = self.index();
        let mut offset = read_u64(ib, 56) as usize;
        let count = read_u32(ib, offset) as usize;
        offset += 4;
        for id in 0..count {
            let len = read_u32(ib, offset) as usize;
            offset += 4;
            if &ib[offset..offset + len] == scope.as_bytes() {
                return Some(id as u32);
            }
            offset += len;
        }
        None
    }

    fn token_lookup(&self, token: &str) -> Option<(u32, u32)> {
        let ib = self.index();
        let token_count = read_u32(ib, 16) as usize;
        let offsets_off = read_u64(ib, 64) as usize;
        let tokens_off = read_u64(ib, 72) as usize;
        let hash = xxh3_64(token.as_bytes());
        let mut low = 0usize;
        let mut high = token_count;
        while low < high {
            let mid = (low + high) / 2;
            let rel = read_u64(ib, offsets_off + mid * 8) as usize;
            let offset = tokens_off + rel;
            let entry_hash = read_u64(ib, offset);
            let len = u16::from_le_bytes(ib[offset + 8..offset + 10].try_into().unwrap()) as usize;
            let entry_token = &ib[offset + 10..offset + 10 + len];
            let cmp = entry_hash
                .cmp(&hash)
                .then_with(|| entry_token.cmp(token.as_bytes()));
            if cmp == std::cmp::Ordering::Equal {
                return Some((
                    read_u32(ib, offset + 10 + len),
                    read_u32(ib, offset + 14 + len),
                ));
            }
            if cmp == std::cmp::Ordering::Less {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        None
    }

    fn posting_tf(&self, start: u32, count: u32, doc_idx: u32) -> Option<u32> {
        let ib = self.index();
        let postings_off = read_u64(ib, 80) as usize;
        let mut low = start as usize;
        let mut high = start as usize + count as usize;
        while low < high {
            let mid = (low + high) / 2;
            let current = read_u32(ib, postings_off + mid * 8);
            if current < doc_idx {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        if low < start as usize + count as usize && read_u32(ib, postings_off + low * 8) == doc_idx
        {
            Some(read_u32(ib, postings_off + low * 8 + 4))
        } else {
            None
        }
    }

    fn bm25_all_terms(
        &self,
        terms: &[String],
        scope_id: Option<u32>,
        kind: Option<u8>,
        exclude: Option<u32>,
        limit: usize,
    ) -> Result<Vec<(u32, f64)>> {
        let ib = self.index();
        let doc_count = read_u32(ib, 8) as f64;
        let avgdl = read_f64(ib, 96);
        let mut scores = vec![0.0f64; doc_count as usize];
        let mut touched = Vec::new();
        let postings_off = read_u64(ib, 80) as usize;
        let docs_off = read_u64(ib, 24) as usize;
        let active_off = read_u64(ib, 48) as usize;
        for term in terms {
            let Some((start, count)) = self.token_lookup(term) else {
                continue;
            };
            let df = count as f64;
            let idf = (1.0 + (doc_count - df + 0.5) / (df + 0.5)).ln();
            for p in 0..count as usize {
                let offset = postings_off + (start as usize + p) * 8;
                let doc_idx = read_u32(ib, offset);
                if exclude == Some(doc_idx)
                    || ib[active_off + doc_idx as usize / 8] & (1 << (doc_idx % 8)) == 0
                {
                    continue;
                }
                let meta_offset = docs_off + doc_idx as usize * DOC_SIZE;
                let scope = read_u32(ib, meta_offset + 16);
                let kind_value = ib[meta_offset + 28];
                if scope_id.is_some_and(|value| scope != value)
                    || kind.is_some_and(|value| kind_value != value)
                {
                    continue;
                }
                let token_count = read_u32(ib, docs_off + doc_idx as usize * DOC_SIZE + 12) as f64;
                let norm = 1.2 * (1.0 - 0.75 + 0.75 * token_count / avgdl.max(1.0));
                let tf = read_u32(ib, offset + 4) as f64;
                let score = idf * (tf * 2.2) / (tf + norm);
                let idx = doc_idx as usize;
                if scores[idx] == 0.0 {
                    touched.push(doc_idx);
                }
                scores[idx] += score;
            }
        }
        touched.sort_by(|a, b| {
            scores[*b as usize]
                .partial_cmp(&scores[*a as usize])
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        touched.truncate(limit);
        Ok(touched
            .into_iter()
            .map(|doc| (doc, scores[doc as usize]))
            .collect())
    }

    pub(crate) fn bm25(
        &self,
        query: &str,
        scope_id: Option<u32>,
        kind: Option<u8>,
        exclude: Option<u32>,
        limit: usize,
    ) -> Result<Vec<(u32, f64)>> {
        let ib = self.index();
        let doc_count = read_u32(ib, 8) as f64;
        let avgdl = read_f64(ib, 96);
        let mut seen = HashSet::new();
        let terms: Vec<String> = tokenize(query)
            .into_iter()
            .filter(|term| seen.insert(term.clone()))
            .take(64)
            .collect();
        let mut term_info = Vec::new();
        for term in &terms {
            if let Some((start, count)) = self.token_lookup(term) {
                let high_df_cutoff = if doc_count >= 10_000.0 {
                    doc_count / 10.0
                } else {
                    f64::INFINITY
                };
                if count as f64 <= high_df_cutoff {
                    let idf = (1.0 + (doc_count - count as f64 + 0.5) / (count as f64 + 0.5)).ln();
                    term_info.push((start, count, idf));
                }
            }
        }
        if term_info.is_empty() {
            for term in &terms {
                if let Some((start, count)) = self.token_lookup(term) {
                    let idf = (1.0 + (doc_count - count as f64 + 0.5) / (count as f64 + 0.5)).ln();
                    term_info.push((start, count, idf));
                }
            }
        }
        if term_info.is_empty() {
            return Ok(Vec::new());
        }
        if self.doc_count() <= 10_000 {
            return self.bm25_all_terms(&terms, scope_id, kind, exclude, limit);
        }
        term_info.sort_by_key(|(_, count, _)| *count);
        let (anchor_start, anchor_count, _) = term_info[0];
        let max_candidates = 50_000usize;
        let postings_off = read_u64(ib, 80) as usize;
        let active_off = read_u64(ib, 48) as usize;
        let docs_off = read_u64(ib, 24) as usize;
        let mut results = Vec::new();
        for p in 0..(anchor_count as usize).min(max_candidates) {
            let offset = postings_off + (anchor_start as usize + p) * 8;
            let doc_idx = read_u32(ib, offset);
            if exclude == Some(doc_idx)
                || ib[active_off + doc_idx as usize / 8] & (1 << (doc_idx % 8)) == 0
            {
                continue;
            }
            let meta_offset = docs_off + doc_idx as usize * DOC_SIZE;
            let scope = read_u32(ib, meta_offset + 16);
            let kind_value = ib[meta_offset + 28];
            if scope_id.is_some_and(|value| scope != value)
                || kind.is_some_and(|value| kind_value != value)
            {
                continue;
            }
            let token_count = read_u32(ib, docs_off + doc_idx as usize * DOC_SIZE + 12) as f64;
            let norm = 1.2 * (1.0 - 0.75 + 0.75 * token_count / avgdl.max(1.0));
            let mut score = 0.0;
            for (index, (start, count, idf)) in term_info.iter().enumerate() {
                let tf = if index == 0 {
                    read_u32(ib, offset + 4) as f64
                } else {
                    match self.posting_tf(*start, *count, doc_idx) {
                        Some(tf) => tf as f64,
                        None => continue,
                    }
                };
                score += idf * (tf * 2.2) / (tf + norm);
            }
            if score > 0.0 {
                results.push((doc_idx, score));
            }
        }
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(limit);
        Ok(results)
    }

    pub(crate) fn tail_view(&self) -> Result<TailView> {
        let records = read_records_from(&self.mmap, self.header.tail_start as usize)?;
        let active = active_docs(&records)?;
        let mut hidden = HashSet::new();
        for record in &records {
            if record.rec_type == T_TOMBSTONE {
                let fields = decode_fields(record.payload()?)?;
                if let Some(target) = get_text(&fields, 1) {
                    hidden.insert(target.to_string());
                }
            }
        }
        for record in &records {
            if record.rec_type == T_MERGE {
                if let Ok(memory) = decode_memory(record.payload()?) {
                    for source in &memory.sources {
                        hidden.insert(source.clone());
                    }
                }
            }
        }
        Ok(TailView {
            memories: active.into_iter().map(|doc| doc.memory).collect(),
            hidden,
        })
    }

    pub(crate) fn scan_page(&mut self, limit: usize, offset: usize) -> Result<Vec<Memory>> {
        let mut skipped = offset;
        let mut out = Vec::new();
        let doc_count = self.doc_count();
        for idx in 0..doc_count {
            if !self.is_active(idx)? {
                continue;
            }
            let meta = self.doc_meta(idx)?;
            let memory = self.doc_memory(&meta)?;
            if skipped > 0 {
                skipped -= 1;
                continue;
            }
            out.push(memory);
            if out.len() >= limit {
                return Ok(out);
            }
        }
        let tail = self.tail_view()?;
        let mut tail_memories = tail.memories;
        tail_memories.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        for memory in tail_memories {
            if skipped > 0 {
                skipped -= 1;
                continue;
            }
            out.push(memory);
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }
}

pub(crate) struct DocMeta {
    pub block_id: u32,
    pub offset_in_block: u32,
    pub record_len: u32,
}

impl TailView {
    pub(crate) fn get(&self, id: &str) -> Option<&Memory> {
        self.memories.iter().find(|memory| memory.id == id)
    }

    pub(crate) fn bm25(
        &self,
        query: &str,
        scope: Option<&str>,
        kind: Option<&str>,
        exclude: Option<&str>,
        limit: usize,
    ) -> Vec<(f64, Memory)> {
        let terms: Vec<String> = tokenize(query);
        if terms.is_empty() || self.memories.is_empty() {
            return Vec::new();
        }
        let docs: Vec<(Memory, Vec<String>)> = self
            .memories
            .iter()
            .filter(|memory| {
                scope.is_none_or(|value| memory.scope == value)
                    && kind.is_none_or(|value| memory.kind == value)
            })
            .map(|memory| {
                (
                    memory.clone(),
                    tokenize(&format!("{} {}", memory.subject, memory.text)),
                )
            })
            .collect();
        let avgdl = docs
            .iter()
            .map(|(_, tokens)| tokens.len() as f64)
            .sum::<f64>()
            / docs.len().max(1) as f64;
        let n = docs.len() as f64;
        let mut df: HashMap<&str, f64> = HashMap::new();
        for term in &terms {
            let count = docs
                .iter()
                .filter(|(_, tokens)| tokens.contains(term))
                .count() as f64;
            df.insert(term.as_str(), count);
        }
        let mut scored = Vec::new();
        for (memory, tokens) in &docs {
            if exclude == Some(memory.id.as_str()) {
                continue;
            }
            let mut score = 0.0;
            for term in &terms {
                let tf = tokens.iter().filter(|token| *token == term).count() as f64;
                if tf == 0.0 {
                    continue;
                }
                let df = df.get(term.as_str()).copied().unwrap_or(0.0);
                let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
                let norm = 1.2 * (1.0 - 0.75 + 0.75 * tokens.len() as f64 / avgdl.max(1.0));
                score += idf * (tf * 2.2) / (tf + norm);
            }
            if score > 0.0 {
                scored.push((score, memory.clone()));
            }
        }
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);
        scored
    }
}

fn decompress_block(data: &[u8], header: &Header, block_id: u32) -> Result<Vec<u8>> {
    let offset = header.block_table_off as usize + block_id as usize * BLOCK_ENTRY;
    let comp_len = read_u32(data, offset + 8) as usize;
    let raw_len = read_u32(data, offset + 12) as usize;
    let hash = read_u64(data, offset + 16);
    let start = read_u64(data, offset) as usize;
    let raw = zstd::bulk::decompress(&data[start + 16..start + 16 + comp_len], raw_len)?;
    if raw.len() != raw_len || xxh3_64(&raw) != hash {
        bail!("block checksum mismatch");
    }
    Ok(raw)
}

fn parse_memory_record(raw: &[u8]) -> Result<Memory> {
    if raw.len() < 18 {
        bail!("short record");
    }
    let body_len = u32::from_le_bytes(raw[0..4].try_into().unwrap()) as usize;
    if raw.len() < 4 + body_len + 8 {
        bail!("short record body");
    }
    let payload = &raw[14..4 + body_len];
    decode_memory(payload)
}

pub(crate) fn encode_record(rec_type: u8, flags: u8, payload: &[u8]) -> Result<Vec<u8>> {
    let mut body = Vec::with_capacity(10 + payload.len());
    body.push(rec_type);
    body.push(flags);
    body.extend_from_slice(&now_ms().to_le_bytes());
    body.extend_from_slice(payload);
    if body.len() > MAX_BODY {
        bail!("record too large");
    }
    let len = (body.len() as u32).to_le_bytes();
    let mut hash_input = Vec::with_capacity(4 + body.len());
    hash_input.extend_from_slice(&len);
    hash_input.extend_from_slice(&body);
    let mut out = Vec::with_capacity(4 + body.len() + 8);
    out.extend_from_slice(&len);
    out.extend_from_slice(&body);
    out.extend_from_slice(&xxh3_64(&hash_input).to_le_bytes());
    Ok(out)
}

pub(crate) fn read_header_file(path: &Path) -> Result<Header> {
    let mut file = File::open(path)?;
    let mut bytes = [0u8; HEADER_LEN];
    std::io::Read::read_exact(&mut file, &mut bytes)?;
    decode_header(&bytes)
}

pub(crate) fn append_record(path: &Path, rec_type: u8, payload: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().append(true).open(path)?;
    file.write_all(&encode_record(rec_type, 0, payload)?)?;
    file.flush()?;
    Ok(())
}

pub(crate) fn read_records_from(data: &[u8], start: usize) -> Result<Vec<Record>> {
    let mut offset = start;
    let mut records = Vec::new();
    while offset < data.len() {
        if data.len() - offset < 4 {
            break;
        }
        let body_len = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
        if !(10..=MAX_BODY).contains(&body_len) || data.len() - offset < 4 + body_len + 8 {
            break;
        }
        let body_end = offset + 4 + body_len;
        let stored = u64::from_le_bytes(data[body_end..body_end + 8].try_into().unwrap());
        if stored != xxh3_64(&data[offset..body_end]) {
            bail!("XXH3 mismatch at offset {offset}");
        }
        records.push(Record {
            rec_type: data[offset + 4],
            raw: data[offset..body_end + 8].to_vec(),
        });
        offset = body_end + 8;
    }
    Ok(records)
}

pub(crate) fn read_all_records(data: &[u8], header: &Header) -> Result<Vec<Record>> {
    let mut records = Vec::new();
    for block_id in 0..header.block_count as u32 {
        let raw = decompress_block(data, header, block_id)?;
        records.extend(read_records_from(&raw, 0)?);
    }
    records.extend(read_records_from(data, header.tail_start as usize)?);
    Ok(records)
}

struct ActiveDoc<'a> {
    memory: Memory,
    record: &'a Record,
}

fn active_docs(records: &[Record]) -> Result<Vec<ActiveDoc<'_>>> {
    let mut memories = Vec::new();
    let mut merges = Vec::new();
    let mut tombstones = HashSet::new();
    for record in records {
        match record.rec_type {
            T_MEMORY => memories.push((decode_memory(record.payload()?)?, record)),
            T_MERGE => {
                let mut memory = decode_memory(record.payload()?)?;
                memory.merged = true;
                merges.push((memory, record));
            }
            T_TOMBSTONE => {
                let fields = decode_fields(record.payload()?)?;
                if let Some(target) = get_text(&fields, 1) {
                    tombstones.insert(target.to_string());
                }
            }
            _ => {}
        }
    }
    let mut hidden = tombstones.clone();
    let mut valid_merges = Vec::new();
    for (memory, record) in merges {
        let invalid = tombstones.contains(&memory.id)
            || memory
                .sources
                .iter()
                .any(|source| tombstones.contains(source));
        hidden.extend(memory.sources.iter().cloned());
        if !invalid {
            valid_merges.push((memory, record));
        }
    }
    let mut active = Vec::new();
    for (memory, record) in memories {
        if !hidden.contains(&memory.id) {
            active.push(ActiveDoc { memory, record });
        }
    }
    for (memory, record) in valid_merges {
        if !hidden.contains(&memory.id) {
            active.push(ActiveDoc { memory, record });
        }
    }
    active.sort_by(|a, b| {
        a.memory
            .created_at
            .cmp(&b.memory.created_at)
            .then_with(|| a.memory.id.cmp(&b.memory.id))
    });
    Ok(active)
}

pub(crate) fn reindex_file(path: &Path) -> Result<Header> {
    let file = File::open(path)?;
    let mmap = unsafe { Mmap::map(&file)? };
    let old_header = decode_header(&mmap)?;
    let created_at = old_header.created_at;
    let records = read_all_records(&mmap, &old_header)?;
    let active = active_docs(&records)?;
    let (index_raw, blocks, doc_count) = build_index(active)?;
    let index_comp = zstd::bulk::compress(&index_raw, ZSTD_LEVEL)?;
    let index_hash = xxh3_64(&index_raw);
    let block_count = blocks.len();
    let block_table_off = HEADER_LEN;
    let index_off = align8(block_table_off + block_count * BLOCK_ENTRY);
    let data_start = index_off + index_comp.len();
    let mut block_offsets = Vec::with_capacity(block_count);
    let mut cursor = data_start;
    for block in &blocks {
        block_offsets.push(cursor);
        cursor += 16 + block.comp.len();
    }
    let header = Header {
        created_at,
        block_table_off: block_table_off as u64,
        block_count: block_count as u64,
        index_off: index_off as u64,
        index_len: index_comp.len() as u64,
        index_raw_len: index_raw.len() as u64,
        index_hash,
        data_start: data_start as u64,
        tail_start: cursor as u64,
        doc_count: doc_count as u64,
    };
    let tmp = temp_path(path);
    if tmp.exists() {
        fs::remove_file(&tmp).ok();
    }
    let mut out = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
    out.write_all(&encode_header(&header))?;
    for (block, offset) in blocks.iter().zip(block_offsets.iter()) {
        let mut entry = [0u8; BLOCK_ENTRY];
        write_u64(&mut entry, 0, *offset as u64);
        write_u32(&mut entry, 8, block.comp.len() as u32);
        write_u32(&mut entry, 12, block.raw_len);
        write_u64(&mut entry, 16, block.hash);
        out.write_all(&entry)?;
    }
    out.write_all(&index_comp)?;
    for block in &blocks {
        out.write_all(&(block.comp.len() as u32).to_le_bytes())?;
        out.write_all(&block.raw_len.to_le_bytes())?;
        out.write_all(&block.hash.to_le_bytes())?;
        out.write_all(&block.comp)?;
    }
    out.sync_all()?;
    drop(out);
    fs::rename(&tmp, path)?;
    let cache_path = path.with_file_name("cm.lru");
    if cache_path.exists() {
        fs::remove_file(&cache_path).ok();
    }
    Ok(header)
}

fn build_index(active: Vec<ActiveDoc<'_>>) -> Result<(Vec<u8>, Vec<Block>, u32)> {
    let mut blocks = Vec::new();
    let mut current = Vec::new();
    let mut docs = Vec::with_capacity(active.len());
    for doc in active {
        let raw = &doc.record.raw;
        let created_at = doc.memory.created_at;
        if !current.is_empty() && current.len() + raw.len() > BLOCK_RAW {
            let block = seal_block(&current)?;
            blocks.push(block);
            current.clear();
        }
        let block_id = blocks.len() as u32;
        let offset = current.len() as u32;
        current.extend_from_slice(raw);
        docs.push(IndexDoc {
            memory: doc.memory,
            block_id,
            offset_in_block: offset,
            record_len: raw.len() as u32,
            scope_id: 0,
            kind_id: 0,
            token_count: 0,
            created_at,
        });
    }
    if !current.is_empty() {
        blocks.push(seal_block(&current)?);
    }

    let mut scopes: Vec<String> = Vec::new();
    let mut scope_map: HashMap<String, u32> = HashMap::new();
    let mut token_entries: Vec<TokenEntry> = Vec::new();
    let mut token_index: HashMap<u64, Vec<usize>> = HashMap::new();
    for (doc_idx, doc) in docs.iter_mut().enumerate() {
        let scope = doc.memory.scope.clone();
        doc.scope_id = *scope_map.entry(scope.clone()).or_insert_with(|| {
            scopes.push(scope);
            (scopes.len() - 1) as u32
        });
        doc.kind_id = kind_id(&doc.memory.kind).unwrap_or(0);
        let mut tf: HashMap<String, u32> = HashMap::new();
        for token in tokenize(&format!("{} {}", doc.memory.subject, doc.memory.text)) {
            *tf.entry(token).or_default() += 1;
        }
        doc.token_count = tf.values().sum::<u32>();
        for (token, count) in tf {
            let hash = xxh3_64(token.as_bytes());
            let entry_idx = {
                let list = token_index.entry(hash).or_default();
                if let Some(pos) = list
                    .iter()
                    .position(|&idx| token_entries[idx].token == token)
                {
                    list[pos]
                } else {
                    let idx = token_entries.len();
                    token_entries.push(TokenEntry {
                        hash,
                        token,
                        postings: Vec::new(),
                    });
                    list.push(idx);
                    idx
                }
            };
            token_entries[entry_idx]
                .postings
                .push((doc_idx as u32, count));
        }
    }

    token_entries.sort_by(|a, b| a.hash.cmp(&b.hash).then_with(|| a.token.cmp(&b.token)));
    let token_count = token_entries.len();
    let mut token_offsets = Vec::with_capacity(token_count + 1);
    let mut tokens_data = Vec::new();
    let mut postings = Vec::new();
    for entry in &token_entries {
        token_offsets.push(tokens_data.len() as u64);
        tokens_data.extend_from_slice(&entry.hash.to_le_bytes());
        tokens_data.extend_from_slice(&(entry.token.len() as u16).to_le_bytes());
        tokens_data.extend_from_slice(entry.token.as_bytes());
        tokens_data.extend_from_slice(&(postings.len() as u32).to_le_bytes());
        tokens_data.extend_from_slice(&(entry.postings.len() as u32).to_le_bytes());
        postings.extend(entry.postings.iter().copied());
    }
    token_offsets.push(tokens_data.len() as u64);

    let mut scope_data = Vec::new();
    scope_data.extend_from_slice(&(scopes.len() as u32).to_le_bytes());
    for scope in &scopes {
        scope_data.extend_from_slice(&(scope.len() as u32).to_le_bytes());
        scope_data.extend_from_slice(scope.as_bytes());
    }

    let n = docs.len();
    let hash_cap = (n * 2).max(1024).next_power_of_two();
    let docs_off = INDEX_HEADER;
    let hash_off = align8(docs_off + DOC_SIZE * n);
    let active_off = align8(hash_off + HASH_SLOT * hash_cap);
    let active_len = n.div_ceil(8);
    let scopes_off = align8(active_off + active_len);
    let token_offsets_off = align8(scopes_off + scope_data.len());
    let tokens_off = align8(token_offsets_off + 8 * (token_count + 1));
    let postings_off = align8(tokens_off + tokens_data.len());
    let index_len = align8(postings_off + 8 * postings.len());
    let avgdl = if n == 0 {
        0.0
    } else {
        docs.iter().map(|doc| doc.token_count as f64).sum::<f64>() / n as f64
    };

    let mut index = vec![0u8; index_len];
    write_u32(&mut index, 0, 0x31584449);
    index[4] = 1;
    index[6..8].copy_from_slice(&(INDEX_HEADER as u16).to_le_bytes());
    write_u32(&mut index, 8, n as u32);
    write_u32(&mut index, 12, scopes.len() as u32);
    write_u32(&mut index, 16, token_count as u32);
    write_u32(&mut index, 20, hash_cap as u32);
    write_u64(&mut index, 24, docs_off as u64);
    write_u64(&mut index, 32, hash_off as u64);
    write_u64(&mut index, 48, active_off as u64);
    write_u64(&mut index, 56, scopes_off as u64);
    write_u64(&mut index, 64, token_offsets_off as u64);
    write_u64(&mut index, 72, tokens_off as u64);
    write_u64(&mut index, 80, postings_off as u64);
    write_u64(&mut index, 88, postings.len() as u64);
    write_f64(&mut index, 96, avgdl);

    for (i, doc) in docs.iter().enumerate() {
        let offset = docs_off + i * DOC_SIZE;
        write_u32(&mut index, offset, doc.block_id);
        write_u32(&mut index, offset + 4, doc.offset_in_block);
        write_u32(&mut index, offset + 8, doc.record_len);
        write_u32(&mut index, offset + 12, doc.token_count);
        write_u32(&mut index, offset + 16, doc.scope_id);
        write_u64(&mut index, offset + 20, doc.created_at);
        index[offset + 28] = doc.kind_id;
        index[offset + 29] = if doc.memory.merged { 1 } else { 0 };
    }

    let mut hash_table = vec![0xFFu8; HASH_SLOT * hash_cap];
    for (i, doc) in docs.iter().enumerate() {
        let hash = xxh3_64(doc.memory.id.as_bytes());
        let mut slot = (hash as usize) & (hash_cap - 1);
        loop {
            let doc_idx = read_u32(&hash_table, slot * HASH_SLOT + 8);
            if doc_idx == u32::MAX {
                write_u64(&mut hash_table, slot * HASH_SLOT, hash);
                write_u32(&mut hash_table, slot * HASH_SLOT + 8, i as u32);
                break;
            }
            slot = (slot + 1) & (hash_cap - 1);
        }
    }
    index[hash_off..hash_off + hash_table.len()].copy_from_slice(&hash_table);
    for i in 0..n {
        index[active_off + i / 8] = if i / 8 == active_len.saturating_sub(1) && n % 8 != 0 {
            (1u8 << (n % 8)) - 1
        } else {
            0xFF
        };
    }
    index[scopes_off..scopes_off + scope_data.len()].copy_from_slice(&scope_data);
    for (i, offset) in token_offsets.iter().enumerate() {
        write_u64(&mut index, token_offsets_off + i * 8, *offset);
    }
    index[tokens_off..tokens_off + tokens_data.len()].copy_from_slice(&tokens_data);
    for (i, (doc, tf)) in postings.iter().enumerate() {
        write_u32(&mut index, postings_off + i * 8, *doc);
        write_u32(&mut index, postings_off + i * 8 + 4, *tf);
    }
    Ok((index, blocks, n as u32))
}

struct IndexDoc {
    memory: Memory,
    block_id: u32,
    offset_in_block: u32,
    record_len: u32,
    scope_id: u32,
    kind_id: u8,
    token_count: u32,
    created_at: u64,
}

struct TokenEntry {
    hash: u64,
    token: String,
    postings: Vec<(u32, u32)>,
}

fn seal_block(raw: &[u8]) -> Result<Block> {
    let comp = zstd::bulk::compress(raw, ZSTD_LEVEL)?;
    Ok(Block {
        comp,
        raw_len: raw.len() as u32,
        hash: xxh3_64(raw),
    })
}

fn temp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "cm".to_string());
    path.with_file_name(format!("{name}.tmp"))
}

pub(crate) fn encode_header(header: &Header) -> [u8; HEADER_LEN] {
    let mut bytes = [0u8; HEADER_LEN];
    bytes[0..4].copy_from_slice(MAGIC);
    bytes[4] = VERSION;
    bytes[6..8].copy_from_slice(&(HEADER_LEN as u16).to_le_bytes());
    bytes[8..16].copy_from_slice(&header.created_at.to_le_bytes());
    bytes[16..24].copy_from_slice(&header.block_table_off.to_le_bytes());
    bytes[24..32].copy_from_slice(&header.block_count.to_le_bytes());
    bytes[32..40].copy_from_slice(&header.index_off.to_le_bytes());
    bytes[40..48].copy_from_slice(&header.index_len.to_le_bytes());
    bytes[48..56].copy_from_slice(&header.index_raw_len.to_le_bytes());
    bytes[56..64].copy_from_slice(&header.index_hash.to_le_bytes());
    bytes[64..72].copy_from_slice(&header.data_start.to_le_bytes());
    bytes[72..80].copy_from_slice(&header.tail_start.to_le_bytes());
    bytes[80..88].copy_from_slice(&header.doc_count.to_le_bytes());
    let hash = xxh3_64(&bytes[0..120]);
    bytes[120..128].copy_from_slice(&hash.to_le_bytes());
    bytes
}

fn decode_header(data: &[u8]) -> Result<Header> {
    if data.len() < HEADER_LEN || &data[0..4] != MAGIC || data[4] != VERSION {
        bail!("bad cm header");
    }
    if xxh3_64(&data[0..120]) != u64::from_le_bytes(data[120..128].try_into().unwrap()) {
        bail!("header XXH3 mismatch");
    }
    Ok(Header {
        created_at: u64::from_le_bytes(data[8..16].try_into().unwrap()),
        block_table_off: u64::from_le_bytes(data[16..24].try_into().unwrap()),
        block_count: u64::from_le_bytes(data[24..32].try_into().unwrap()),
        index_off: u64::from_le_bytes(data[32..40].try_into().unwrap()),
        index_len: u64::from_le_bytes(data[40..48].try_into().unwrap()),
        index_raw_len: u64::from_le_bytes(data[48..56].try_into().unwrap()),
        index_hash: u64::from_le_bytes(data[56..64].try_into().unwrap()),
        data_start: u64::from_le_bytes(data[64..72].try_into().unwrap()),
        tail_start: u64::from_le_bytes(data[72..80].try_into().unwrap()),
        doc_count: u64::from_le_bytes(data[80..88].try_into().unwrap()),
    })
}

fn read_u32(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
}
fn read_u64(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap())
}
fn read_f64(data: &[u8], offset: usize) -> f64 {
    f64::from_le_bytes(data[offset..offset + 8].try_into().unwrap())
}
fn write_u32(data: &mut [u8], offset: usize, value: u32) {
    data[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
fn write_u64(data: &mut [u8], offset: usize, value: u64) {
    data[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}
fn write_f64(data: &mut [u8], offset: usize, value: f64) {
    data[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}
fn align8(value: usize) -> usize {
    (value + 7) & !7
}
