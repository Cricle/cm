use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use fs2::FileExt;
use memmap2::Mmap;
use xxhash_rust::xxh3::xxh3_64;

use crate::cache::Cache;
use crate::model::{
    content_hash, decode_fields, decode_memory, get_text, kind_id, memory_payload, now_ms,
    tokenize, Memory,
};

pub(crate) const HEADER_LEN: usize = 128;
pub(crate) const DUAL_HEADER_LEN: usize = HEADER_LEN * 2;
pub(crate) const HEADER_SLOT: usize = HEADER_LEN;
pub(crate) const HEADER_FLAG_DUAL: u32 = 1;
const HEADER_FLAG_PAGE_INDEX: u32 = 2;
const PAGE_SIZE: usize = 64 * 1024;
const PAGE_MAGIC: &[u8; 8] = b"CMPG1\0\0\0";
const PAGE_ENTRY: usize = 24;
const PAGE_TABLE_HEADER: usize = 24;
const INDEX_PAGE_CACHE_BYTES: usize = 32 * 1024 * 1024;
const INDEX_PAGE_CACHE_PAGES: usize = INDEX_PAGE_CACHE_BYTES / PAGE_SIZE;
pub(crate) const INDEX_MARKER: &[u8; 8] = b"CMIX1\0\0\0";
const INDEX_MARKER_LEN: usize = 16;
pub(crate) const BLOCK_ENTRY: usize = 24;
pub(crate) const DOC_SIZE: usize = 32;
pub(crate) const HASH_SLOT: usize = 16;
const HASH_SLOT8: usize = 8;
const INDEX_VERSION_HASH8: u8 = 2;
const INDEX_VERSION_DELTA: u8 = 4;
pub(crate) const INDEX_HEADER: usize = 128;
const MAGIC: &[u8; 4] = b"CM1\0";
const VERSION: u8 = 1;
const MAX_BODY: usize = 16 * 1024 * 1024;
const BLOCK_RAW: usize = 64 * 1024;
const ZSTD_LEVEL: i32 = 3;

pub(crate) const T_MEMORY: u8 = 2;
pub(crate) const T_MERGE: u8 = 3;
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
    pub generation: u64,
    pub flags: u32,
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
    pub mmap: Arc<Mmap>,
    pub header: Header,
    pub cache: Cache,
    index: Index,
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

#[derive(Clone)]
struct PageChunk {
    comp: Vec<u8>,
    raw_len: u32,
    hash: u64,
}

#[derive(Clone, Copy)]
struct PageEntry {
    offset: u64,
    comp_len: u32,
    raw_len: u32,
    hash: u64,
}

struct CachedPage {
    data: Arc<[u8]>,
    last_used: u64,
}

/// A small strict-LRU cache for decompressed index pages. The cap is a byte
/// budget (32 MiB by default) and also a page-count bound so a malformed page
/// table cannot grow the cache without limit.
struct PageCache {
    pages: HashMap<usize, CachedPage>,
    bytes: usize,
    tick: u64,
}

impl PageCache {
    fn new() -> Self {
        Self {
            pages: HashMap::new(),
            bytes: 0,
            tick: 0,
        }
    }

    fn get(&mut self, page_id: usize) -> Option<Arc<[u8]>> {
        self.tick = self.tick.wrapping_add(1);
        let tick = self.tick;
        let entry = self.pages.get_mut(&page_id)?;
        entry.last_used = tick;
        Some(Arc::clone(&entry.data))
    }

    fn insert(&mut self, page_id: usize, data: Arc<[u8]>) {
        if self.pages.contains_key(&page_id) {
            return;
        }
        while !self.pages.is_empty()
            && (self.pages.len() >= INDEX_PAGE_CACHE_PAGES
                || self.bytes.saturating_add(data.len()) > INDEX_PAGE_CACHE_BYTES)
        {
            let victim = self
                .pages
                .iter()
                .min_by_key(|(_, page)| page.last_used)
                .map(|(id, _)| *id);
            let Some(victim) = victim else {
                break;
            };
            if let Some(page) = self.pages.remove(&victim) {
                self.bytes = self.bytes.saturating_sub(page.data.len());
            }
        }
        self.tick = self.tick.wrapping_add(1);
        self.bytes = self.bytes.saturating_add(data.len());
        self.pages.insert(
            page_id,
            CachedPage {
                data,
                last_used: self.tick,
            },
        );
    }
}

struct PagedIndex {
    mmap: Arc<Mmap>,
    entries: Arc<Vec<PageEntry>>,
    cache: RefCell<PageCache>,
    decompressor: RefCell<zstd::bulk::Decompressor<'static>>,
    len: usize,
    hash: u64,
}

fn decompress_index_page(
    data: &[u8],
    page_id: usize,
    entry: PageEntry,
    decompressor: &mut zstd::bulk::Decompressor<'static>,
) -> Result<Vec<u8>> {
    let start = entry.offset as usize;
    let end = start
        .checked_add(entry.comp_len as usize)
        .context("index page offset overflow")?;
    let compressed = data
        .get(start..end)
        .ok_or_else(|| anyhow!("index page {page_id} out of bounds"))?;
    let raw = decompressor
        .decompress(compressed, entry.raw_len as usize)
        .context("decompress index page")?;
    if raw.len() != entry.raw_len as usize || xxh3_64(&raw) != entry.hash {
        bail!("index page checksum mismatch");
    }
    Ok(raw)
}

impl PagedIndex {
    fn load(&self, page_id: usize) -> Result<Arc<[u8]>> {
        if let Some(page) = self.cache.borrow_mut().get(page_id) {
            return Ok(page);
        }
        let entry = *self
            .entries
            .get(page_id)
            .ok_or_else(|| anyhow!("index page {page_id} out of range"))?;
        let raw = decompress_index_page(
            &self.mmap,
            page_id,
            entry,
            &mut self.decompressor.borrow_mut(),
        )?;
        let page: Arc<[u8]> = Arc::from(raw.into_boxed_slice());
        self.cache.borrow_mut().insert(page_id, Arc::clone(&page));
        Ok(page)
    }

    /// Read document metadata for many random doc ids. Pages are decompressed
    /// once into a reusable buffer and discarded; only the small `DocMeta`
    /// records are kept. This avoids allocating and caching dozens of MiB of
    /// whole 64 KiB pages for a single query.
    fn doc_meta_batch(&self, docs_off: usize, indices: &[u32]) -> Result<Vec<DocMeta>> {
        let mut groups: HashMap<usize, Vec<(usize, usize)>> = HashMap::new();
        for (result_idx, &idx) in indices.iter().enumerate() {
            let offset = docs_off + idx as usize * DOC_SIZE;
            groups
                .entry(offset / PAGE_SIZE)
                .or_default()
                .push((result_idx, offset % PAGE_SIZE));
        }
        let mut groups: Vec<(usize, Vec<(usize, usize)>)> = groups.into_iter().collect();
        groups.sort_by_key(|(page_id, _)| *page_id);
        let mut metas = vec![
            DocMeta {
                block_id: 0,
                offset_in_block: 0,
                record_len: 0,
                token_count: 0,
                scope_id: 0,
                kind: 0,
            };
            indices.len()
        ];
        let threads = std::thread::available_parallelism()
            .map(|value| value.get())
            .unwrap_or(1)
            .clamp(1, 16);
        if threads > 1 && groups.len() > 2 {
            let chunk = groups.len().div_ceil(threads);
            let mmap = Arc::clone(&self.mmap);
            let entries = Arc::clone(&self.entries);
            let decoded: Vec<(usize, DocMeta)> = std::thread::scope(|scope| -> Result<_> {
                let mut handles = Vec::new();
                for part in groups.chunks(chunk) {
                    let mmap = Arc::clone(&mmap);
                    let entries = Arc::clone(&entries);
                    handles.push(scope.spawn(move || -> Result<Vec<(usize, DocMeta)>> {
                        let mut decompressor = zstd::bulk::Decompressor::new()?;
                        let mut buffer = vec![0u8; PAGE_SIZE];
                        let mut out = Vec::new();
                        for (page_id, items) in part {
                            read_doc_meta_page(
                                &mmap,
                                &entries,
                                *page_id,
                                items,
                                &mut decompressor,
                                &mut buffer,
                                &mut out,
                            )?;
                        }
                        Ok(out)
                    }));
                }
                let mut decoded = Vec::new();
                for handle in handles {
                    let part = handle
                        .join()
                        .map_err(|_| anyhow!("index page worker panicked"))??;
                    decoded.extend(part);
                }
                Ok(decoded)
            })?;
            for (result_idx, meta) in decoded {
                metas[result_idx] = meta;
            }
        } else {
            let mut decompressor = zstd::bulk::Decompressor::new()?;
            let mut buffer = vec![0u8; PAGE_SIZE];
            let mut out = Vec::new();
            for (page_id, items) in &groups {
                read_doc_meta_page(
                    &self.mmap,
                    &self.entries,
                    *page_id,
                    items,
                    &mut decompressor,
                    &mut buffer,
                    &mut out,
                )?;
            }
            for (result_idx, meta) in out {
                metas[result_idx] = meta;
            }
        }
        Ok(metas)
    }
}

#[allow(clippy::too_many_arguments)]
fn read_doc_meta_page(
    data: &[u8],
    entries: &[PageEntry],
    page_id: usize,
    items: &[(usize, usize)],
    decompressor: &mut zstd::bulk::Decompressor<'static>,
    buffer: &mut [u8],
    out: &mut Vec<(usize, DocMeta)>,
) -> Result<()> {
    let entry = *entries
        .get(page_id)
        .ok_or_else(|| anyhow!("index page {page_id} out of range"))?;
    let start = entry.offset as usize;
    let end = start
        .checked_add(entry.comp_len as usize)
        .context("index page offset overflow")?;
    let compressed = data
        .get(start..end)
        .ok_or_else(|| anyhow!("index page {page_id} out of bounds"))?;
    let written = decompressor
        .decompress_to_buffer(compressed, &mut buffer[..])
        .context("decompress index page")?;
    if written != entry.raw_len as usize || xxh3_64(&buffer[..written]) != entry.hash {
        bail!("index page checksum mismatch");
    }
    for &(result_idx, in_page) in items {
        let end = in_page
            .checked_add(DOC_SIZE)
            .ok_or_else(|| anyhow!("doc metadata offset overflow"))?;
        if end > written {
            bail!("doc metadata crosses index page boundary");
        }
        out.push((result_idx, parse_doc_meta(&buffer[in_page..end])));
    }
    Ok(())
}

enum IndexStorage {
    /// Legacy single-blob index, already decompressed into memory.
    Blob(Vec<u8>),
    /// Page-index: only the page table is loaded up front.
    Paged(PagedIndex),
}

/// Read-only view of the logical (uncompressed) index. Callers use
/// `read_u32`/`read_u64`/`read_bytes` and stay independent of whether the
/// store uses a page index or the legacy compressed single blob.
pub(crate) struct Index {
    data: IndexStorage,
}

impl Index {
    fn from_blob(raw: Vec<u8>) -> Self {
        Self {
            data: IndexStorage::Blob(raw),
        }
    }

    fn from_pages(mmap: Arc<Mmap>, header: &Header) -> Result<Self> {
        let start = header.index_off as usize;
        let end = start
            .checked_add(header.index_len as usize)
            .context("page table offset overflow")?;
        let table = mmap
            .get(start..end)
            .ok_or_else(|| anyhow!("page table out of bounds"))?;
        let entries = parse_page_table(table, header)?;
        let len = header.index_raw_len as usize;
        if entries.len() != len.div_ceil(PAGE_SIZE) {
            bail!("page index has wrong page count");
        }
        Ok(Self {
            data: IndexStorage::Paged(PagedIndex {
                mmap,
                entries: Arc::new(entries),
                cache: RefCell::new(PageCache::new()),
                decompressor: RefCell::new(zstd::bulk::Decompressor::new()?),
                len,
                hash: header.index_hash,
            }),
        })
    }

    pub(crate) fn len(&self) -> usize {
        match &self.data {
            IndexStorage::Blob(raw) => raw.len(),
            IndexStorage::Paged(paged) => paged.len,
        }
    }

    fn page(&self, page_id: usize) -> Result<Arc<[u8]>> {
        match &self.data {
            IndexStorage::Blob(_) => bail!("page access on single-blob index"),
            IndexStorage::Paged(paged) => paged.load(page_id),
        }
    }

    fn check_range(&self, offset: usize, len: usize) -> Result<usize> {
        let end = offset
            .checked_add(len)
            .ok_or_else(|| anyhow!("index range overflow"))?;
        if end > self.len() {
            bail!("index range {offset}..{end} out of bounds");
        }
        Ok(end)
    }

    fn read_array<const N: usize>(&self, offset: usize) -> Result<[u8; N]> {
        let end = self.check_range(offset, N)?;
        match &self.data {
            IndexStorage::Blob(raw) => {
                let mut out = [0u8; N];
                out.copy_from_slice(&raw[offset..end]);
                Ok(out)
            }
            IndexStorage::Paged(paged) => {
                let page_id = offset / PAGE_SIZE;
                let in_page = offset % PAGE_SIZE;
                let page = paged.load(page_id)?;
                if in_page + N <= page.len() {
                    let mut out = [0u8; N];
                    out.copy_from_slice(&page[in_page..in_page + N]);
                    Ok(out)
                } else {
                    let bytes = self.read_bytes(offset, N)?;
                    let mut out = [0u8; N];
                    out.copy_from_slice(&bytes);
                    Ok(out)
                }
            }
        }
    }

    pub(crate) fn read_u8(&self, offset: usize) -> Result<u8> {
        Ok(self.read_array::<1>(offset)?[0])
    }

    pub(crate) fn read_u16(&self, offset: usize) -> Result<u16> {
        Ok(u16::from_le_bytes(self.read_array::<2>(offset)?))
    }

    pub(crate) fn read_u32(&self, offset: usize) -> Result<u32> {
        Ok(u32::from_le_bytes(self.read_array::<4>(offset)?))
    }

    pub(crate) fn read_u64(&self, offset: usize) -> Result<u64> {
        Ok(u64::from_le_bytes(self.read_array::<8>(offset)?))
    }

    pub(crate) fn read_f64(&self, offset: usize) -> Result<f64> {
        Ok(f64::from_le_bytes(self.read_array::<8>(offset)?))
    }

    pub(crate) fn read_bytes(&self, offset: usize, len: usize) -> Result<Vec<u8>> {
        let end = self.check_range(offset, len)?;
        match &self.data {
            IndexStorage::Blob(raw) => Ok(raw[offset..end].to_vec()),
            IndexStorage::Paged(paged) => {
                let mut out = Vec::with_capacity(len);
                let mut pos = offset;
                while pos < end {
                    let page_id = pos / PAGE_SIZE;
                    let page = paged.load(page_id)?;
                    let in_page = pos % PAGE_SIZE;
                    if in_page >= page.len() {
                        bail!("index page {page_id} too short");
                    }
                    let take = (end - pos).min(page.len() - in_page);
                    out.extend_from_slice(&page[in_page..in_page + take]);
                    pos += take;
                }
                Ok(out)
            }
        }
    }

    /// Materialize the whole logical index. Only checkpoint/reindex paths use
    /// this; normal reads never copy the full index.
    pub(crate) fn to_vec(&self) -> Result<Vec<u8>> {
        match &self.data {
            IndexStorage::Blob(raw) => Ok(raw.clone()),
            IndexStorage::Paged(paged) => {
                let raw = self.read_bytes(0, paged.len)?;
                if xxh3_64(&raw) != paged.hash {
                    bail!("index hash mismatch");
                }
                Ok(raw)
            }
        }
    }

    pub(crate) fn cursor(&self, offset: usize) -> IndexCursor<'_> {
        IndexCursor::new(self, offset)
    }

    fn is_paged(&self) -> bool {
        matches!(self.data, IndexStorage::Paged(_))
    }

    pub(crate) fn doc_meta_batch(&self, indices: &[u32]) -> Result<Vec<DocMeta>> {
        let docs_off = self.read_u64(24)? as usize;
        match &self.data {
            IndexStorage::Blob(raw) => indices
                .iter()
                .map(|&idx| {
                    let offset = docs_off + idx as usize * DOC_SIZE;
                    let end = self.check_range(offset, DOC_SIZE)?;
                    Ok(parse_doc_meta(&raw[offset..end]))
                })
                .collect(),
            IndexStorage::Paged(paged) => {
                for &idx in indices {
                    let offset = docs_off + idx as usize * DOC_SIZE;
                    self.check_range(offset, DOC_SIZE)?;
                }
                paged.doc_meta_batch(docs_off, indices)
            }
        }
    }
}

pub(crate) struct IndexCursor<'a> {
    index: &'a Index,
    offset: usize,
    page_id: usize,
    page: Option<Arc<[u8]>>,
    page_end: usize,
}

impl<'a> IndexCursor<'a> {
    fn new(index: &'a Index, offset: usize) -> Self {
        Self {
            index,
            offset,
            page_id: usize::MAX,
            page: None,
            page_end: 0,
        }
    }

    pub(crate) fn byte(&mut self) -> Result<u8> {
        if self.offset >= self.index.len() {
            bail!("index cursor out of bounds");
        }
        if let IndexStorage::Blob(raw) = &self.index.data {
            let byte = raw[self.offset];
            self.offset += 1;
            return Ok(byte);
        }
        if self.page.is_none() || self.offset >= self.page_end {
            let page_id = self.offset / PAGE_SIZE;
            let page = self.index.page(page_id)?;
            self.page_id = page_id;
            self.page_end = (page_id + 1) * PAGE_SIZE;
            self.page = Some(page);
        }
        let page = self.page.as_ref().expect("page loaded");
        let in_page = self.offset - self.page_id * PAGE_SIZE;
        let byte = *page
            .get(in_page)
            .ok_or_else(|| anyhow!("index cursor beyond page"))?;
        self.offset += 1;
        Ok(byte)
    }

    pub(crate) fn varint(&mut self) -> Result<usize> {
        let mut value = 0usize;
        let mut shift = 0u32;
        loop {
            let byte = self.byte()?;
            value |= ((byte & 0x7F) as usize) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
            if shift >= usize::BITS {
                bail!("posting varint overflow");
            }
        }
    }
}

fn build_page_chunks(raw: &[u8]) -> Result<Vec<PageChunk>> {
    let mut chunks = Vec::with_capacity(raw.len().div_ceil(PAGE_SIZE));
    for chunk in raw.chunks(PAGE_SIZE) {
        let comp = zstd::bulk::compress(chunk, ZSTD_LEVEL)?;
        chunks.push(PageChunk {
            comp,
            raw_len: chunk.len() as u32,
            hash: xxh3_64(chunk),
        });
    }
    Ok(chunks)
}

fn encode_page_table(entries: &[PageEntry], page_size: usize, raw_len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(PAGE_TABLE_HEADER + entries.len() * PAGE_ENTRY + 8);
    out.extend_from_slice(PAGE_MAGIC);
    out.extend_from_slice(&(page_size as u32).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    out.extend_from_slice(&(raw_len as u64).to_le_bytes());
    for entry in entries {
        out.extend_from_slice(&entry.offset.to_le_bytes());
        out.extend_from_slice(&entry.comp_len.to_le_bytes());
        out.extend_from_slice(&entry.raw_len.to_le_bytes());
        out.extend_from_slice(&entry.hash.to_le_bytes());
    }
    let checksum = xxh3_64(&out);
    out.extend_from_slice(&checksum.to_le_bytes());
    out
}

fn parse_page_table(data: &[u8], header: &Header) -> Result<Vec<PageEntry>> {
    if data.len() < PAGE_TABLE_HEADER + 8 || &data[..8] != PAGE_MAGIC {
        bail!("bad page index table");
    }
    let page_size = read_u32(data, 8) as usize;
    let count = read_u32(data, 12) as usize;
    let raw_len = read_u64(data, 16) as usize;
    if page_size != PAGE_SIZE || raw_len != header.index_raw_len as usize {
        bail!("page index table header mismatch");
    }
    let expected = PAGE_TABLE_HEADER
        .checked_add(
            count
                .checked_mul(PAGE_ENTRY)
                .context("page table size overflow")?,
        )
        .and_then(|value| value.checked_add(8))
        .context("page table size overflow")?;
    if expected != data.len() {
        bail!("page index table length mismatch");
    }
    let stored = read_u64(data, expected - 8);
    if xxh3_64(&data[..expected - 8]) != stored {
        bail!("page index table checksum mismatch");
    }
    let mut entries = Vec::with_capacity(count);
    let mut total = 0usize;
    for i in 0..count {
        let offset = PAGE_TABLE_HEADER + i * PAGE_ENTRY;
        let entry = PageEntry {
            offset: read_u64(data, offset),
            comp_len: read_u32(data, offset + 8),
            raw_len: read_u32(data, offset + 12),
            hash: read_u64(data, offset + 16),
        };
        if entry.raw_len == 0 || entry.raw_len as usize > PAGE_SIZE {
            bail!("bad page raw length");
        }
        total = total
            .checked_add(entry.raw_len as usize)
            .context("page raw length overflow")?;
        entries.push(entry);
    }
    if total != raw_len {
        bail!("page raw length total mismatch");
    }
    Ok(entries)
}

fn read_index_pages(mmap: &[u8], header: &Header) -> Result<Vec<u8>> {
    let start = header.index_off as usize;
    let end = start
        .checked_add(header.index_len as usize)
        .context("page table offset overflow")?;
    if end > mmap.len() {
        bail!("page table out of bounds");
    }
    let entries = parse_page_table(&mmap[start..end], header)?;
    let mut raw = Vec::with_capacity(header.index_raw_len as usize);
    for entry in entries {
        let page_start = entry.offset as usize;
        let page_end = page_start
            .checked_add(entry.comp_len as usize)
            .context("page offset overflow")?;
        if page_end > mmap.len() {
            bail!("page out of bounds");
        }
        let page = zstd::bulk::decompress(&mmap[page_start..page_end], entry.raw_len as usize)
            .context("decompress index page")?;
        if page.len() != entry.raw_len as usize || xxh3_64(&page) != entry.hash {
            bail!("index page checksum mismatch");
        }
        raw.extend_from_slice(&page);
    }
    if raw.len() != header.index_raw_len as usize || xxh3_64(&raw) != header.index_hash {
        bail!("index hash mismatch");
    }
    Ok(raw)
}

fn read_index_raw(mmap: &[u8], header: &Header) -> Result<Vec<u8>> {
    if header.flags & HEADER_FLAG_PAGE_INDEX != 0 {
        return read_index_pages(mmap, header);
    }
    let start = header.index_off as usize;
    let end = start
        .checked_add(header.index_len as usize)
        .context("index offset overflow")?;
    if end > mmap.len() {
        bail!("index out of bounds");
    }
    let raw = zstd::bulk::decompress(&mmap[start..end], header.index_raw_len as usize)
        .context("decompress index")?;
    if raw.len() != header.index_raw_len as usize || xxh3_64(&raw) != header.index_hash {
        bail!("index hash mismatch");
    }
    Ok(raw)
}

#[allow(clippy::type_complexity)]
fn page_layout(
    raw: &[u8],
    page_start: usize,
) -> Result<(Vec<PageChunk>, Vec<PageEntry>, usize, Vec<u8>)> {
    let chunks = build_page_chunks(raw)?;
    let mut entries = Vec::with_capacity(chunks.len());
    let mut cursor = page_start;
    for chunk in &chunks {
        entries.push(PageEntry {
            offset: cursor as u64,
            comp_len: chunk.comp.len() as u32,
            raw_len: chunk.raw_len,
            hash: chunk.hash,
        });
        cursor = cursor
            .checked_add(chunk.comp.len())
            .context("page offset overflow")?;
    }
    let table = encode_page_table(&entries, PAGE_SIZE, raw.len());
    Ok((chunks, entries, cursor, table))
}

impl Store {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        let mmap = Arc::new(unsafe { Mmap::map(&file)? });
        let header = read_current_header(&mmap[..])?;
        if header.index_len == 0 {
            bail!("no index; run cm reindex");
        }
        // Page-index stores load only the page table here. The legacy
        // single-blob path decompresses the index once, as before.
        let index = if header.flags & HEADER_FLAG_PAGE_INDEX != 0 {
            Index::from_pages(Arc::clone(&mmap), &header)?
        } else {
            Index::from_blob(read_index_raw(&mmap[..], &header)?)
        };
        let cache = match Cache::open_existing(path, header.index_hash) {
            Some(cache) => cache,
            None => match Cache::create(path, header.index_hash) {
                Ok(cache) => cache,
                // The cache is disposable: a read must still work when a
                // concurrent writer replaces it or the directory is
                // read-only. Retry the open once, then fall back to an
                // in-process cache.
                Err(_) => match Cache::open_existing(path, header.index_hash) {
                    Some(cache) => cache,
                    None => Cache::memory(header.index_hash)?,
                },
            },
        };
        Ok(Self {
            mmap,
            header,
            cache,
            index,
        })
    }

    pub(crate) fn index(&self) -> &Index {
        &self.index
    }

    /// Set the working-set affinity for blocks touched by this query.
    pub(crate) fn set_cache_affinity(&mut self, scope: Option<&str>, task: Option<&str>) {
        self.cache.set_affinity(scope, task);
    }

    /// Warm the blocks belonging to selected memory ids. This makes the
    /// query's result set part of the active scope/task working set.
    pub(crate) fn prefetch_memory_ids(&mut self, ids: &[String]) -> Result<()> {
        let mut blocks = Vec::new();
        for id in ids {
            if let Some((idx, _)) = self.find_doc(id)? {
                blocks.push(self.doc_meta(idx)?.block_id);
            }
        }
        blocks.sort_unstable();
        blocks.dedup();
        for block_id in blocks {
            let data = std::sync::Arc::clone(&self.mmap);
            let header = self.header;
            self.cache
                .get_block(block_id, || decompress_block(&data, &header, block_id))?;
        }
        Ok(())
    }

    pub(crate) fn doc_meta(&self, idx: u32) -> Result<DocMeta> {
        let ib = self.index();
        let docs_off = ib.read_u64(24)? as usize;
        let offset = docs_off + idx as usize * DOC_SIZE;
        let raw = ib.read_array::<DOC_SIZE>(offset)?;
        Ok(parse_doc_meta(&raw))
    }

    pub(crate) fn doc_memory(&mut self, meta: &DocMeta) -> Result<Memory> {
        let data: &[u8] = &self.mmap[..];
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
        let active_off = ib.read_u64(48)? as usize;
        Ok(ib.read_u8(active_off + idx as usize / 8)? & (1 << (idx % 8)) != 0)
    }

    pub(crate) fn active_bits(&self) -> Result<Vec<u8>> {
        let ib = self.index();
        let active_off = ib.read_u64(48)? as usize;
        let active_len = (self.doc_count() as usize).div_ceil(8);
        ib.read_bytes(active_off, active_len)
    }

    pub(crate) fn active_count(&self) -> Result<u64> {
        Ok(self
            .active_bits()?
            .iter()
            .map(|byte| byte.count_ones() as u64)
            .sum())
    }

    pub(crate) fn doc_count(&self) -> u32 {
        self.header.doc_count as u32
    }

    pub(crate) fn find_doc(&mut self, id: &str) -> Result<Option<(u32, Memory)>> {
        let (hash_cap, hash_off, version) = {
            let ib = self.index();
            (ib.read_u32(20)?, ib.read_u64(32)? as usize, ib.read_u8(4)?)
        };
        let hash8 = version >= INDEX_VERSION_HASH8;
        let hash = xxh3_64(id.as_bytes());
        let hash_slot = if hash8 { HASH_SLOT8 } else { HASH_SLOT };
        let mut slot = (hash as usize) & (hash_cap as usize - 1);
        for _ in 0..hash_cap {
            let (entry_hash, doc_idx) = {
                let ib = self.index();
                let offset = hash_off + slot * hash_slot;
                if hash8 {
                    (ib.read_u32(offset)? as u64, ib.read_u32(offset + 4)?)
                } else {
                    (ib.read_u64(offset)?, ib.read_u32(offset + 8)?)
                }
            };
            if doc_idx == u32::MAX {
                return Ok(None);
            }
            let hash_matches = if hash8 {
                entry_hash == (hash & u32::MAX as u64)
            } else {
                entry_hash == hash
            };
            if hash_matches {
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

    pub(crate) fn scope_id(&self, scope: &str) -> Result<Option<u32>> {
        let ib = self.index();
        let mut offset = ib.read_u64(56)? as usize;
        let count = ib.read_u32(offset)? as usize;
        offset += 4;
        for id in 0..count {
            let len = ib.read_u32(offset)? as usize;
            offset += 4;
            if ib.read_bytes(offset, len)? == scope.as_bytes() {
                return Ok(Some(id as u32));
            }
            offset += len;
        }
        Ok(None)
    }

    /// Returns `(posting_start, posting_count, posting_end)` where start/end
    /// are byte offsets into the postings region. The end offset comes from
    /// the next token entry, so callers can read an exact slice.
    fn token_lookup(&self, token: &str) -> Result<Option<(u32, u32, u32)>> {
        let ib = self.index();
        let token_count = ib.read_u32(16)? as usize;
        let offsets_off = ib.read_u64(64)? as usize;
        let tokens_off = ib.read_u64(72)? as usize;
        let postings_len = ib.read_u64(88)? as u32;
        let hash = xxh3_64(token.as_bytes());
        let mut low = 0usize;
        let mut high = token_count;
        while low < high {
            let mid = (low + high) / 2;
            let rel = ib.read_u64(offsets_off + mid * 8)? as usize;
            let offset = tokens_off + rel;
            let entry_hash = ib.read_u64(offset)?;
            let len = ib.read_u16(offset + 8)? as usize;
            let entry_token = ib.read_bytes(offset + 10, len)?;
            let cmp = entry_hash
                .cmp(&hash)
                .then_with(|| entry_token.as_slice().cmp(token.as_bytes()));
            if cmp == std::cmp::Ordering::Equal {
                let start = ib.read_u32(offset + 10 + len)?;
                let count = ib.read_u32(offset + 14 + len)?;
                let end = if mid + 1 < token_count {
                    let next_rel = ib.read_u64(offsets_off + (mid + 1) * 8)? as usize;
                    let next_offset = tokens_off + next_rel;
                    let next_len = ib.read_u16(next_offset + 8)? as usize;
                    ib.read_u32(next_offset + 10 + next_len)?
                } else {
                    postings_len
                };
                return Ok(Some((start, count, end)));
            }
            if cmp == std::cmp::Ordering::Less {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        Ok(None)
    }

    fn decode_postings(&self, start: u32, count: u32, end: u32) -> Result<Vec<(u32, u32)>> {
        let ib = self.index();
        let postings_off = ib.read_u64(80)? as usize;
        let mut postings = Vec::with_capacity(count as usize);
        if count == 0 {
            return Ok(postings);
        }
        if ib.read_u8(4)? >= INDEX_VERSION_DELTA {
            if ib.is_paged() {
                // The varint cursor is convenient for the single-blob path,
                // but per-byte page checks are expensive. For page indexes
                // read the exact posting extent and decode it as a slice.
                let begin = postings_off + start as usize;
                let len = end.saturating_sub(start) as usize;
                let bytes = ib.read_bytes(begin, len)?;
                let mut cursor = 0usize;
                let mut previous = 0u32;
                for _ in 0..count {
                    let delta = take_varint_slice(&bytes, &mut cursor)? as u32;
                    let doc_idx = previous
                        .checked_add(delta)
                        .context("posting delta overflow")?;
                    let tf = take_varint_slice(&bytes, &mut cursor)? as u32;
                    postings.push((doc_idx, tf));
                    previous = doc_idx;
                }
            } else {
                let mut cursor = ib.cursor(postings_off + start as usize);
                let mut previous = 0u32;
                for _ in 0..count {
                    let delta = cursor.varint()? as u32;
                    let doc_idx = previous
                        .checked_add(delta)
                        .context("posting delta overflow")?;
                    let tf = cursor.varint()? as u32;
                    postings.push((doc_idx, tf));
                    previous = doc_idx;
                }
            }
        } else {
            for i in 0..count {
                let offset = postings_off + (start as usize + i as usize) * POSTING_STRIDE;
                postings.push((ib.read_u32(offset)?, read_posting_tf_index(ib, offset)?));
            }
        }
        Ok(postings)
    }

    fn bm25_all_terms(
        &self,
        terms: &[String],
        scope_id: Option<u32>,
        kind: Option<u8>,
        exclude: Option<u32>,
        hidden_docs: Option<&HashSet<u32>>,
        limit: usize,
    ) -> Result<Vec<(u32, f64)>> {
        let ib = self.index();
        let doc_count = self.doc_count() as f64;
        let avgdl = ib.read_f64(96)?;
        let mut scores = vec![0.0f64; doc_count as usize];
        let mut touched = Vec::new();
        let docs_off = ib.read_u64(24)? as usize;
        let active = self.active_bits()?;
        for term in terms {
            let Some((start, count, end)) = self.token_lookup(term)? else {
                continue;
            };
            let df = count as f64;
            let idf = (1.0 + (doc_count - df + 0.5) / (df + 0.5)).ln();
            for (doc_idx, tf) in self.decode_postings(start, count, end)? {
                if exclude == Some(doc_idx)
                    || hidden_docs.is_some_and(|hidden| hidden.contains(&doc_idx))
                    || active[doc_idx as usize / 8] & (1 << (doc_idx % 8)) == 0
                {
                    continue;
                }
                let meta_offset = docs_off + doc_idx as usize * DOC_SIZE;
                let scope = ib.read_u32(meta_offset + 16)?;
                let kind_value = ib.read_u8(meta_offset + 28)?;
                if scope_id.is_some_and(|value| scope != value)
                    || kind.is_some_and(|value| kind_value != value)
                {
                    continue;
                }
                let token_count = ib.read_u32(meta_offset + 12)? as f64;
                let norm = 1.2 * (1.0 - 0.75 + 0.75 * token_count / avgdl.max(1.0));
                let tf = tf as f64;
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
        hidden_docs: Option<&HashSet<u32>>,
        limit: usize,
    ) -> Result<Vec<(u32, f64)>> {
        let ib = self.index();
        let doc_count = self.doc_count() as f64;
        let avgdl = ib.read_f64(96)?;
        let mut seen = HashSet::new();
        let terms: Vec<String> = tokenize(query)
            .into_iter()
            .filter(|term| seen.insert(term.clone()))
            .take(64)
            .collect();
        let mut term_info = Vec::new();
        for term in &terms {
            if let Some((start, count, end)) = self.token_lookup(term)? {
                let high_df_cutoff = if doc_count >= 10_000.0 {
                    doc_count / 10.0
                } else {
                    f64::INFINITY
                };
                if count as f64 <= high_df_cutoff {
                    let idf = (1.0 + (doc_count - count as f64 + 0.5) / (count as f64 + 0.5)).ln();
                    term_info.push((start, count, end, idf));
                }
            }
        }
        if term_info.is_empty() {
            for term in &terms {
                if let Some((start, count, end)) = self.token_lookup(term)? {
                    let idf = (1.0 + (doc_count - count as f64 + 0.5) / (count as f64 + 0.5)).ln();
                    term_info.push((start, count, end, idf));
                }
            }
        }
        if term_info.is_empty() {
            return Ok(Vec::new());
        }
        if self.doc_count() <= 10_000 {
            return self.bm25_all_terms(&terms, scope_id, kind, exclude, hidden_docs, limit);
        }
        term_info.sort_by_key(|(_, count, _, _)| *count);
        let (anchor_start, anchor_count, anchor_end, _) = term_info[0];
        let max_candidates = 50_000usize;
        let active = self.active_bits()?;
        let anchor_postings = self.decode_postings(anchor_start, anchor_count, anchor_end)?;
        let mut extra_maps: Vec<Option<HashMap<u32, u32>>> = Vec::with_capacity(term_info.len());
        for (index, (start, count, end, _)) in term_info.iter().enumerate() {
            if index == 0 {
                extra_maps.push(None);
            } else {
                extra_maps.push(Some(
                    self.decode_postings(*start, *count, *end)?
                        .into_iter()
                        .collect::<HashMap<_, _>>(),
                ));
            }
        }
        let candidates: Vec<(u32, u32)> = anchor_postings
            .into_iter()
            .take(max_candidates)
            .filter(|(doc_idx, _)| {
                exclude != Some(*doc_idx)
                    && !hidden_docs.is_some_and(|hidden| hidden.contains(doc_idx))
                    && active[*doc_idx as usize / 8] & (1 << (*doc_idx % 8)) != 0
            })
            .collect();
        let candidate_docs: Vec<u32> = candidates.iter().map(|(doc_idx, _)| *doc_idx).collect();
        let metas = self.index().doc_meta_batch(&candidate_docs)?;
        let mut results = Vec::new();
        for ((doc_idx, anchor_tf), meta) in candidates.into_iter().zip(metas) {
            if scope_id.is_some_and(|value| meta.scope_id != value)
                || kind.is_some_and(|value| meta.kind != value)
            {
                continue;
            }
            let token_count = meta.token_count as f64;
            let norm = 1.2 * (1.0 - 0.75 + 0.75 * token_count / avgdl.max(1.0));
            let mut score = 0.0;
            for (index, (_, _, _, idf)) in term_info.iter().enumerate() {
                let tf = if index == 0 {
                    anchor_tf as f64
                } else {
                    match extra_maps[index].as_ref().and_then(|map| map.get(&doc_idx)) {
                        Some(tf) => *tf as f64,
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
            if record.rec_type == T_MEMORY || record.rec_type == T_MERGE {
                if let Ok(memory) = decode_memory(record.payload()?) {
                    for source in &memory.sources {
                        hidden.insert(source.clone());
                    }
                    for superseded in &memory.supersedes {
                        hidden.insert(superseded.clone());
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
        let tail = self.tail_view()?;
        let mut skipped = offset;
        let mut out = Vec::new();
        let doc_count = self.doc_count();
        let active = self.active_bits()?;
        for idx in 0..doc_count {
            if active[idx as usize / 8] & (1 << (idx % 8)) == 0 {
                continue;
            }
            let meta = self.doc_meta(idx)?;
            let memory = self.doc_memory(&meta)?;
            if tail.hidden.contains(&memory.id) {
                continue;
            }
            if skipped > 0 {
                skipped -= 1;
                continue;
            }
            out.push(memory);
            if out.len() >= limit {
                return Ok(out);
            }
        }
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

#[derive(Clone, Copy)]
pub(crate) struct DocMeta {
    pub block_id: u32,
    pub offset_in_block: u32,
    pub record_len: u32,
    pub token_count: u32,
    pub scope_id: u32,
    pub kind: u8,
}

fn parse_doc_meta(raw: &[u8]) -> DocMeta {
    DocMeta {
        block_id: u32::from_le_bytes(raw[0..4].try_into().unwrap()),
        offset_in_block: u32::from_le_bytes(raw[4..8].try_into().unwrap()),
        record_len: u32::from_le_bytes(raw[8..12].try_into().unwrap()),
        token_count: u32::from_le_bytes(raw[12..16].try_into().unwrap()),
        scope_id: u32::from_le_bytes(raw[16..20].try_into().unwrap()),
        kind: raw[28],
    }
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
    let mut memory = decode_memory(payload)?;
    // The payload format has no merged bit; the record type is the source of
    // truth. Index blocks keep the original T_MERGE record, so preserve that
    // flag when reading a merged doc back out of a block (e.g. after reindex).
    memory.merged = raw[4] == T_MERGE;
    Ok(memory)
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

/// Exact content duplicate check for the uncompressed tail plus a bounded
/// exact scan for small indexes. Large indexes keep the cheap tail-only check;
/// this avoids decoding a million records on every write.
pub(crate) fn find_tail_duplicate(path: &Path, target: &Memory) -> Result<Option<String>> {
    let file = File::open(path)?;
    let mmap = unsafe { Mmap::map(&file)? };
    let header = read_current_header(&mmap)?;
    let records = read_records_from(&mmap, header.tail_start as usize)?;
    let target_hash = content_hash(target);
    for doc in active_docs(&records)? {
        if content_hash(&doc.memory) == target_hash {
            return Ok(Some(doc.memory.id));
        }
    }
    if header.doc_count > 0 && header.doc_count <= 10_000 {
        let mut store = Store::open(path)?;
        let tail = store.tail_view()?;
        for idx in 0..store.doc_count() {
            if !store.is_active(idx)? {
                continue;
            }
            let meta = store.doc_meta(idx)?;
            let memory = store.doc_memory(&meta)?;
            if !tail.hidden.contains(&memory.id) && content_hash(&memory) == target_hash {
                return Ok(Some(memory.id));
            }
        }
    }
    Ok(None)
}

pub(crate) fn read_header_file(path: &Path) -> Result<Header> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut bytes = [0u8; DUAL_HEADER_LEN];
    std::io::Read::read_exact(&mut file, &mut bytes[..HEADER_LEN])
        .with_context(|| format!("read header {}", path.display()))?;
    let extra = std::io::Read::read(&mut file, &mut bytes[HEADER_LEN..]).unwrap_or(0);
    read_current_header(&bytes[..HEADER_LEN + extra])
}

pub(crate) fn append_record(
    path: &Path,
    rec_type: u8,
    payload: &[u8],
    durable: bool,
) -> Result<()> {
    // The lock file, not the store inode, is the stable mutex: checkpoint and
    // reindex replace the store by rename, so a lock held on the old cm file
    // would not exclude writers that open the new path. Do not also take a
    // byte-range lock on the store itself: on Windows that makes concurrent
    // lock-free header reads fail with ERROR_LOCK_VIOLATION.
    let _store_lock = acquire_store_lock(path)?;
    let mut file = OpenOptions::new().read(true).append(true).open(path)?;
    file.write_all(&encode_record(rec_type, 0, payload)?)?;
    if durable {
        file.sync_all()?;
    } else {
        file.flush()?;
    }
    Ok(())
}

pub(crate) fn read_records_from(data: &[u8], start: usize) -> Result<Vec<Record>> {
    Ok(read_records_from_impl(data, start, false)?.0)
}

/// Like `read_records_from`, but rejects a truncated record or trailing bytes
/// instead of silently ignoring them. Used by `verify_full` so `cm doctor`
/// cannot report a healthy file after tail data loss or garbage was appended.
pub(crate) fn read_records_from_checked(data: &[u8], start: usize) -> Result<Vec<Record>> {
    let (records, offset) = read_records_from_impl(data, start, true)?;
    if offset != data.len() {
        bail!(
            "record data ends at offset {offset}, file has {} bytes",
            data.len()
        );
    }
    Ok(records)
}

/// Parse a contiguous run of records that must end exactly at EOF. Used only
/// to recover records appended after a checkpoint was killed midway through
/// appending an index generation: the partial index bytes are unreachable, but
/// later records are still valid and must not be silently dropped.
fn parse_record_chain_to_eof(data: &[u8], mut offset: usize) -> Option<Vec<Record>> {
    let mut records = Vec::new();
    while offset < data.len() {
        if data.len() - offset < 4 {
            return None;
        }
        let body_len = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
        if !(10..=MAX_BODY).contains(&body_len) || data.len() - offset < 4 + body_len + 8 {
            return None;
        }
        let body_end = offset + 4 + body_len;
        let stored = u64::from_le_bytes(data[body_end..body_end + 8].try_into().unwrap());
        if stored != xxh3_64(&data[offset..body_end]) {
            return None;
        }
        records.push(Record {
            rec_type: data[offset + 4],
            raw: data[offset..body_end + 8].to_vec(),
        });
        offset = body_end + 8;
    }
    (!records.is_empty()).then_some(records)
}

fn recover_records_after_partial_index(data: &[u8], search_from: usize) -> Option<Vec<Record>> {
    // Scan forward from just after the partial marker. The uncompressed bytes
    // before the first later record are compressed index data, so a false
    // positive would need a valid body length and a matching XXH3 checksum.
    (search_from..data.len()).find_map(|offset| parse_record_chain_to_eof(data, offset))
}

/// A kill while writing the marker can leave fewer than eight bytes of it
/// before a later record. Only treat the bytes as a partial marker when they
/// match a non-empty prefix and a valid record chain starts after them.
fn recover_after_partial_marker_prefix(data: &[u8], offset: usize) -> Option<Vec<Record>> {
    let prefix_len = data[offset..]
        .iter()
        .zip(INDEX_MARKER.iter())
        .take_while(|(actual, expected)| actual == expected)
        .count();
    if prefix_len == 0 {
        return None;
    }
    recover_records_after_partial_index(data, offset + 1)
}

fn read_records_from_impl(data: &[u8], start: usize, strict: bool) -> Result<(Vec<Record>, usize)> {
    if start > data.len() {
        bail!("record start {start} beyond end of data ({})", data.len());
    }
    let mut offset = start;
    let mut records = Vec::new();
    while offset < data.len() {
        let remaining = data.len() - offset;
        if remaining < INDEX_MARKER_LEN {
            let marker_prefix = if remaining >= 8 {
                &data[offset..offset + 8] == INDEX_MARKER
            } else {
                INDEX_MARKER.starts_with(&data[offset..])
            };
            if marker_prefix {
                if strict {
                    bail!("truncated index generation at offset {offset}");
                }
                if let Some(recovered) = recover_records_after_partial_index(data, offset + 1) {
                    records.extend(recovered);
                    return Ok((records, data.len()));
                }
                break;
            }
        }
        if remaining >= INDEX_MARKER_LEN && &data[offset..offset + 8] == INDEX_MARKER {
            let len =
                u64::from_le_bytes(data[offset + 8..offset + 16].try_into().unwrap()) as usize;
            let end = offset
                .checked_add(INDEX_MARKER_LEN)
                .and_then(|value| value.checked_add(len))
                .context("index generation length overflow")?;
            if end > data.len() {
                if strict {
                    bail!("truncated index generation at offset {offset}");
                }
                if let Some(recovered) = recover_records_after_partial_index(data, offset + 1) {
                    records.extend(recovered);
                    return Ok((records, data.len()));
                }
                break;
            }
            offset = end;
            continue;
        }
        if remaining < 4 {
            if strict {
                bail!("truncated record header at offset {offset}");
            }
            if let Some(recovered) = recover_after_partial_marker_prefix(data, offset) {
                records.extend(recovered);
                return Ok((records, data.len()));
            }
            break;
        }
        let body_len = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
        if !(10..=MAX_BODY).contains(&body_len) || remaining < 4 + body_len + 8 {
            if strict {
                bail!("truncated or invalid record at offset {offset}");
            }
            if let Some(recovered) = recover_after_partial_marker_prefix(data, offset) {
                records.extend(recovered);
                return Ok((records, data.len()));
            }
            break;
        }
        let body_end = offset + 4 + body_len;
        let stored = u64::from_le_bytes(data[body_end..body_end + 8].try_into().unwrap());
        if stored != xxh3_64(&data[offset..body_end]) {
            if !strict {
                if let Some(recovered) = recover_after_partial_marker_prefix(data, offset) {
                    records.extend(recovered);
                    return Ok((records, data.len()));
                }
            }
            bail!("XXH3 mismatch at offset {offset}");
        }
        records.push(Record {
            rec_type: data[offset + 4],
            raw: data[offset..body_end + 8].to_vec(),
        });
        offset = body_end + 8;
    }
    Ok((records, offset))
}

/// Return the offset of a trailing, incomplete index generation if the tail
/// ends with one. A process killed while `checkpoint --in-place` appends the
/// marker/length/compressed index can leave such bytes behind; the currently
/// committed header never references them, so they are safe to discard.
fn partial_index_tail_start(data: &[u8], start: usize) -> Result<Option<usize>> {
    if start > data.len() {
        bail!("record start {start} beyond end of data ({})", data.len());
    }
    let mut offset = start;
    while offset < data.len() {
        let remaining = data.len() - offset;
        if remaining >= INDEX_MARKER_LEN && &data[offset..offset + 8] == INDEX_MARKER {
            let len =
                u64::from_le_bytes(data[offset + 8..offset + 16].try_into().unwrap()) as usize;
            let end = offset
                .checked_add(INDEX_MARKER_LEN)
                .and_then(|value| value.checked_add(len))
                .context("index generation length overflow")?;
            if end > data.len() {
                return Ok(Some(offset));
            }
            offset = end;
            continue;
        }
        if remaining < INDEX_MARKER_LEN {
            if remaining >= 8 && &data[offset..offset + 8] == INDEX_MARKER {
                return Ok(Some(offset));
            }
            if remaining < 8 && INDEX_MARKER.starts_with(&data[offset..]) {
                return Ok(Some(offset));
            }
        }
        if remaining < 4 {
            return Ok(recover_after_partial_marker_prefix(data, offset).map(|_| offset));
        }
        let body_len = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
        if !(10..=MAX_BODY).contains(&body_len) || remaining < 4 + body_len + 8 {
            return Ok(recover_after_partial_marker_prefix(data, offset).map(|_| offset));
        }
        let body_end = offset + 4 + body_len;
        let stored = u64::from_le_bytes(data[body_end..body_end + 8].try_into().unwrap());
        if stored != xxh3_64(&data[offset..body_end]) {
            return Ok(recover_after_partial_marker_prefix(data, offset).map(|_| offset));
        }
        offset = body_end + 8;
    }
    Ok(None)
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
    for (memory, _) in &memories {
        for superseded in &memory.supersedes {
            hidden.insert(superseded.clone());
        }
    }
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

pub(crate) fn verify_full(path: &Path) -> Result<u64> {
    let file = File::open(path)?;
    let mmap = unsafe { Mmap::map(&file)? };
    let header = read_current_header(&mmap)?;
    if header.index_len > 0 {
        let _ = read_index_raw(&mmap, &header)?;
    }
    let mut count = 0u64;
    for block_id in 0..header.block_count as u32 {
        let raw = decompress_block(&mmap, &header, block_id)?;
        count += read_records_from_checked(&raw, 0)?.len() as u64;
    }
    count += read_records_from_checked(&mmap, header.tail_start as usize)?.len() as u64;
    Ok(count)
}

pub(crate) fn reindex_file(path: &Path) -> Result<Header> {
    let _store_lock = acquire_store_lock(path)?;
    let file = File::open(path)?;
    let mmap = unsafe { Mmap::map(&file)? };
    let old_header = read_current_header(&mmap)?;
    let created_at = old_header.created_at;
    let records = read_all_records(&mmap, &old_header)?;
    let active = active_docs(&records)?;
    let (index_raw, blocks, doc_count) = build_index(active, true)?;
    let index_hash = xxh3_64(&index_raw);
    let block_count = blocks.len();
    let block_table_off = DUAL_HEADER_LEN;
    let page_start = align8(block_table_off + block_count * BLOCK_ENTRY);
    let (pages, _entries, index_off, page_table) = page_layout(&index_raw, page_start)?;
    let data_start = index_off + page_table.len();
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
        index_len: page_table.len() as u64,
        index_raw_len: index_raw.len() as u64,
        index_hash,
        data_start: data_start as u64,
        tail_start: cursor as u64,
        doc_count: doc_count as u64,
        generation: 1,
        flags: HEADER_FLAG_DUAL | HEADER_FLAG_PAGE_INDEX,
    };
    let tmp = temp_path(path);
    if tmp.exists() {
        fs::remove_file(&tmp).ok();
    }
    let mut out = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
    out.write_all(&encode_header(&header))?;
    out.write_all(&encode_header(&header))?;
    for (block, offset) in blocks.iter().zip(block_offsets.iter()) {
        let mut entry = [0u8; BLOCK_ENTRY];
        write_u64(&mut entry, 0, *offset as u64);
        write_u32(&mut entry, 8, block.comp.len() as u32);
        write_u32(&mut entry, 12, block.raw_len);
        write_u64(&mut entry, 16, block.hash);
        out.write_all(&entry)?;
    }
    for page in &pages {
        out.write_all(&page.comp)?;
    }
    out.write_all(&page_table)?;
    for block in &blocks {
        out.write_all(&(block.comp.len() as u32).to_le_bytes())?;
        out.write_all(&block.raw_len.to_le_bytes())?;
        out.write_all(&block.hash.to_le_bytes())?;
        out.write_all(&block.comp)?;
    }
    out.sync_all()?;
    drop(out);
    fs::rename(&tmp, path)?;
    // The cache is keyed by index hash and replaced atomically on the next
    // open. Do not delete it: a reader may be mapping it right now, and a
    // missing/recreated cache must never turn a read command into an error.
    Ok(header)
}

fn mask_index_hidden(
    store: &mut Store,
    index_raw: &mut [u8],
    hidden: &HashSet<String>,
) -> Result<bool> {
    let doc_count = store.doc_count();
    let active_off = read_u64(index_raw, 48) as usize;
    let scopes_off = read_u64(index_raw, 56) as usize;
    let active_len = (doc_count as usize).div_ceil(8);
    if active_off < INDEX_HEADER
        || scopes_off > index_raw.len()
        || active_off
            .checked_add(active_len)
            .is_none_or(|end| end > scopes_off)
    {
        bail!("index active bitset out of bounds");
    }
    let mut changed = false;
    for id in hidden {
        if let Some((idx, _)) = store.find_doc(id)? {
            let byte = index_raw[active_off + idx as usize / 8];
            let bit = 1u8 << (idx % 8);
            if byte & bit != 0 {
                index_raw[active_off + idx as usize / 8] = byte & !bit;
                changed = true;
            }
        }
    }
    Ok(changed)
}

fn migrate_checkpoint(
    path: &Path,
    store: &Store,
    old_header: Header,
    index_raw: Vec<u8>,
) -> Result<Header> {
    let index_hash = xxh3_64(&index_raw);

    struct OldBlock {
        offset: usize,
        comp_len: usize,
    }
    let table_off = old_header.block_table_off as usize;
    let block_count = old_header.block_count as usize;
    let table_end = table_off
        .checked_add(
            block_count
                .checked_mul(BLOCK_ENTRY)
                .context("block table size overflow")?,
        )
        .context("block table offset overflow")?;
    if table_end > store.mmap.len() {
        bail!("block table out of bounds");
    }
    let mut old_blocks = Vec::with_capacity(block_count);
    for block_id in 0..block_count {
        let entry = table_off
            .checked_add(block_id * BLOCK_ENTRY)
            .context("block table offset overflow")?;
        let offset = read_u64(&store.mmap, entry) as usize;
        let comp_len = read_u32(&store.mmap, entry + 8) as usize;
        let end = offset
            .checked_add(16)
            .and_then(|value| value.checked_add(comp_len))
            .context("block bounds overflow")?;
        if end > store.mmap.len() {
            bail!("block {block_id} out of bounds");
        }
        old_blocks.push(OldBlock { offset, comp_len });
    }
    let tail_start = old_header.tail_start as usize;
    if tail_start > store.mmap.len() {
        bail!("tail out of bounds");
    }

    let block_table_off = DUAL_HEADER_LEN;
    let page_start = align8(block_table_off + block_count * BLOCK_ENTRY);
    let (pages, _entries, index_off, page_table) = page_layout(&index_raw, page_start)?;
    let data_start = index_off + page_table.len();
    let mut block_offsets = Vec::with_capacity(block_count);
    let mut cursor = data_start;
    for block in &old_blocks {
        block_offsets.push(cursor);
        cursor += 16 + block.comp_len;
    }
    let header = Header {
        created_at: old_header.created_at,
        block_table_off: block_table_off as u64,
        block_count: block_count as u64,
        index_off: index_off as u64,
        index_len: page_table.len() as u64,
        index_raw_len: index_raw.len() as u64,
        index_hash,
        data_start: data_start as u64,
        tail_start: cursor as u64,
        doc_count: old_header.doc_count,
        generation: 1,
        flags: HEADER_FLAG_DUAL | HEADER_FLAG_PAGE_INDEX,
    };
    let tmp = temp_path(path);
    if tmp.exists() {
        fs::remove_file(&tmp).ok();
    }
    let mut out = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
    out.write_all(&encode_header(&header))?;
    out.write_all(&encode_header(&header))?;
    for (block, offset) in old_blocks.iter().zip(block_offsets.iter()) {
        let mut entry = [0u8; BLOCK_ENTRY];
        write_u64(&mut entry, 0, *offset as u64);
        write_u32(&mut entry, 8, block.comp_len as u32);
        write_u32(&mut entry, 12, read_u32(&store.mmap, block.offset + 4));
        write_u64(&mut entry, 16, read_u64(&store.mmap, block.offset + 8));
        out.write_all(&entry)?;
    }
    for page in &pages {
        out.write_all(&page.comp)?;
    }
    out.write_all(&page_table)?;
    for block in &old_blocks {
        out.write_all(&store.mmap[block.offset..block.offset + 16 + block.comp_len])?;
    }
    for record in read_records_from(&store.mmap, tail_start)? {
        out.write_all(&record.raw)?;
    }
    out.sync_all()?;
    drop(out);
    fs::rename(&tmp, path)?;
    // The cache is keyed by index hash and replaced atomically on the next
    // open. Do not delete it: a reader may be mapping it right now, and a
    // missing/recreated cache must never turn a read command into an error.
    Ok(header)
}

/// Copy-on-write checkpoint. New indexes are appended after the current
/// tail, then an inactive 128-byte header slot is committed. Data blocks and
/// previous index generations are never rewritten. Old one-header files are
/// migrated once to the dual-header layout by copying immutable blocks.
pub(crate) fn checkpoint_file(path: &Path, in_place: bool) -> Result<Header> {
    let _store_lock = acquire_store_lock(path)?;
    let mut store = Store::open(path)?;
    let (current, slot) = read_current_header_with_slot(&store.mmap)?;
    let tail_start = current.tail_start as usize;
    if tail_start > store.mmap.len() {
        bail!("tail out of bounds");
    }
    // A crash during an earlier in-place append can leave a partial index
    // generation at EOF. It is unreachable (no committed header points at it),
    // so validate the records before it and heal the file with a full rewrite
    // instead of refusing every future checkpoint until a manual reindex.
    let partial_index = match read_records_from_checked(&store.mmap, tail_start) {
        Ok(_) => None,
        Err(error) => {
            match partial_index_tail_start(&store.mmap, tail_start).context("validate tail")? {
                Some(offset) => Some(offset),
                None => return Err(error).context("validate tail"),
            }
        }
    };
    let tail = store.tail_view()?;
    let mut index_raw = store.index().to_vec()?;
    let changed = mask_index_hidden(&mut store, &mut index_raw, &tail.hidden)?;

    if !in_place || current.flags & HEADER_FLAG_DUAL == 0 || partial_index.is_some() {
        return migrate_checkpoint(path, &store, current, index_raw);
    }
    if !changed {
        return Ok(current);
    }
    if current.flags & HEADER_FLAG_PAGE_INDEX == 0 {
        return migrate_checkpoint(path, &store, current, index_raw);
    }

    let table_start = current.index_off as usize;
    let table_end = table_start
        .checked_add(current.index_len as usize)
        .context("page table offset overflow")?;
    if table_end > store.mmap.len() {
        bail!("page table out of bounds");
    }
    let old_entries = parse_page_table(&store.mmap[table_start..table_end], &current)?;
    let page_count = index_raw.len().div_ceil(PAGE_SIZE);
    if old_entries.len() != page_count {
        return migrate_checkpoint(path, &store, current, index_raw);
    }
    let index_hash = xxh3_64(&index_raw);
    let marker_offset = store.mmap.len();
    let mut cursor = marker_offset
        .checked_add(INDEX_MARKER_LEN)
        .context("page marker offset overflow")?;
    let mut dirty = Vec::new();
    let mut entries = Vec::with_capacity(page_count);
    for (i, raw_page) in index_raw.chunks(PAGE_SIZE).enumerate() {
        let old = old_entries[i];
        let hash = xxh3_64(raw_page);
        if old.raw_len == raw_page.len() as u32 && old.hash == hash {
            entries.push(old);
            continue;
        }
        let comp = zstd::bulk::compress(raw_page, ZSTD_LEVEL)?;
        entries.push(PageEntry {
            offset: cursor as u64,
            comp_len: comp.len() as u32,
            raw_len: raw_page.len() as u32,
            hash,
        });
        cursor = cursor
            .checked_add(comp.len())
            .context("page offset overflow")?;
        dirty.push(comp);
    }
    let page_table = encode_page_table(&entries, PAGE_SIZE, index_raw.len());
    let index_off = cursor;
    let payload_len = index_off
        .checked_sub(marker_offset + INDEX_MARKER_LEN)
        .and_then(|value| value.checked_add(page_table.len()))
        .context("page generation length overflow")?;
    let mut out = OpenOptions::new().append(true).open(path)?;
    out.write_all(INDEX_MARKER)?;
    out.write_all(&(payload_len as u64).to_le_bytes())?;
    for comp in &dirty {
        out.write_all(comp)?;
    }
    out.write_all(&page_table)?;
    out.sync_all()?;
    drop(out);

    let mut next = current;
    next.index_off = index_off as u64;
    next.index_len = page_table.len() as u64;
    next.index_raw_len = index_raw.len() as u64;
    next.index_hash = index_hash;
    next.generation = current.generation.saturating_add(1);
    next.flags |= HEADER_FLAG_DUAL | HEADER_FLAG_PAGE_INDEX;

    let other = if slot == 0 { HEADER_SLOT } else { 0 };
    let mut header_file = OpenOptions::new().write(true).open(path)?;
    header_file.seek(SeekFrom::Start(other as u64))?;
    header_file.write_all(&encode_header(&next))?;
    header_file.sync_all()?;
    drop(header_file);
    drop(store);

    let cache_path = crate::cache::cache_path(path);
    if cache_path.exists() {
        fs::remove_file(&cache_path).ok();
    }
    Ok(next)
}

fn build_index(active: Vec<ActiveDoc<'_>>, reencode: bool) -> Result<(Vec<u8>, Vec<Block>, u32)> {
    let mut blocks = Vec::new();
    let mut current = Vec::new();
    let mut docs = Vec::with_capacity(active.len());
    for doc in active {
        let encoded = if reencode {
            Some(encode_record(
                doc.record.rec_type,
                0,
                &memory_payload(&doc.memory),
            )?)
        } else {
            None
        };
        let raw = encoded.as_deref().unwrap_or(&doc.record.raw);
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

    let n = docs.len();
    let index = build_index_bytes(docs)?;
    Ok((index, blocks, n as u32))
}

fn build_index_bytes(mut docs: Vec<IndexDoc>) -> Result<Vec<u8>> {
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
        let mut previous = 0u32;
        for (doc, tf) in &entry.postings {
            put_varint(&mut postings, (*doc - previous) as usize);
            previous = *doc;
            put_varint(&mut postings, *tf as usize);
        }
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
    let hash_slot = HASH_SLOT8;
    let docs_off = INDEX_HEADER;
    let hash_off = align8(docs_off + DOC_SIZE * n);
    let active_off = align8(hash_off + hash_slot * hash_cap);
    let active_len = n.div_ceil(8);
    let scopes_off = align8(active_off + active_len);
    let token_offsets_off = align8(scopes_off + scope_data.len());
    let tokens_off = align8(token_offsets_off + 8 * (token_count + 1));
    let postings_off = align8(tokens_off + tokens_data.len());
    let index_len = align8(postings_off + postings.len());
    let avgdl = if n == 0 {
        0.0
    } else {
        docs.iter().map(|doc| doc.token_count as f64).sum::<f64>() / n as f64
    };

    let mut index = vec![0u8; index_len];
    write_u32(&mut index, 0, 0x31584449);
    index[4] = INDEX_VERSION_DELTA;
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

    let mut hash_table = vec![0xFFu8; hash_slot * hash_cap];
    for (i, doc) in docs.iter().enumerate() {
        let hash = xxh3_64(doc.memory.id.as_bytes());
        let mut slot = (hash as usize) & (hash_cap - 1);
        loop {
            let entry = slot * hash_slot;
            let doc_idx = read_u32(&hash_table, entry + 4);
            if doc_idx == u32::MAX {
                write_u32(&mut hash_table, entry, hash as u32);
                write_u32(&mut hash_table, entry + 4, i as u32);
                break;
            }
            slot = (slot + 1) & (hash_cap - 1);
        }
    }
    index[hash_off..hash_off + hash_table.len()].copy_from_slice(&hash_table);
    for i in 0..n {
        index[active_off + i / 8] = if i / 8 == active_len.saturating_sub(1) && !n.is_multiple_of(8)
        {
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
    index[postings_off..postings_off + postings.len()].copy_from_slice(&postings);
    Ok(index)
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

fn lock_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "cm".to_string());
    path.with_file_name(format!("{name}.lock"))
}

/// Cross-process mutex for writers that can replace `path` by rename.
/// Readers stay lock-free: rename is atomic, so they observe either version.
pub(crate) fn acquire_store_lock(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path(path))
        .with_context(|| format!("open lock {}", lock_path(path).display()))?;
    file.lock_exclusive()
        .with_context(|| format!("lock {}", lock_path(path).display()))?;
    Ok(file)
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
    bytes[88..96].copy_from_slice(&header.generation.to_le_bytes());
    bytes[96..100].copy_from_slice(&header.flags.to_le_bytes());
    let hash = xxh3_64(&bytes[0..120]);
    bytes[120..128].copy_from_slice(&hash.to_le_bytes());
    bytes
}

fn decode_header_at(data: &[u8], offset: usize) -> Result<Header> {
    let end = offset
        .checked_add(HEADER_LEN)
        .context("header offset overflow")?;
    if end > data.len() || &data[offset..offset + 4] != MAGIC || data[offset + 4] != VERSION {
        bail!("bad cm header");
    }
    let bytes = &data[offset..end];
    if xxh3_64(&bytes[0..120]) != u64::from_le_bytes(bytes[120..128].try_into().unwrap()) {
        bail!("header XXH3 mismatch");
    }
    Ok(Header {
        created_at: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
        block_table_off: u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
        block_count: u64::from_le_bytes(bytes[24..32].try_into().unwrap()),
        index_off: u64::from_le_bytes(bytes[32..40].try_into().unwrap()),
        index_len: u64::from_le_bytes(bytes[40..48].try_into().unwrap()),
        index_raw_len: u64::from_le_bytes(bytes[48..56].try_into().unwrap()),
        index_hash: u64::from_le_bytes(bytes[56..64].try_into().unwrap()),
        data_start: u64::from_le_bytes(bytes[64..72].try_into().unwrap()),
        tail_start: u64::from_le_bytes(bytes[72..80].try_into().unwrap()),
        doc_count: u64::from_le_bytes(bytes[80..88].try_into().unwrap()),
        generation: u64::from_le_bytes(bytes[88..96].try_into().unwrap()),
        flags: u32::from_le_bytes(bytes[96..100].try_into().unwrap()),
    })
}

pub(crate) fn read_current_header_with_slot(data: &[u8]) -> Result<(Header, usize)> {
    let first = decode_header_at(data, 0);
    if let Ok(header) = first {
        if header.flags & HEADER_FLAG_DUAL == 0 {
            return Ok((header, 0));
        }
        if let Ok(second) = decode_header_at(data, HEADER_SLOT) {
            if second.generation > header.generation {
                return Ok((second, HEADER_SLOT));
            }
        }
        return Ok((header, 0));
    }
    let header = decode_header_at(data, HEADER_SLOT)?;
    Ok((header, HEADER_SLOT))
}

pub(crate) fn read_current_header(data: &[u8]) -> Result<Header> {
    read_current_header_with_slot(data).map(|(header, _)| header)
}

fn put_varint(out: &mut Vec<u8>, mut value: usize) {
    while value >= 0x80 {
        out.push((value as u8 & 0x7F) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

const POSTING_STRIDE: usize = 8;

fn read_posting_tf_index(index: &Index, offset: usize) -> Result<u32> {
    index.read_u32(offset + 4)
}

fn take_varint_slice(data: &[u8], cursor: &mut usize) -> Result<usize> {
    let mut value = 0usize;
    let mut shift = 0u32;
    loop {
        let byte = *data
            .get(*cursor)
            .ok_or_else(|| anyhow!("truncated posting varint"))?;
        *cursor += 1;
        value |= ((byte & 0x7F) as usize) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
        if shift >= usize::BITS {
            bail!("posting varint overflow");
        }
    }
}

fn read_u32(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
}
fn read_u64(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap())
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
