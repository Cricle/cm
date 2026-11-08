use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use fs2::FileExt;
use memmap2::{MmapMut, MmapOptions};
use xxhash_rust::xxh3::{xxh3_64, xxh3_64_with_seed};

use crate::model::now_ms;

const CACHE_MAGIC: &[u8; 4] = b"CMLU";
const CACHE_VERSION: u8 = 1;
const CACHE_LAYOUT: u8 = 1;
const CACHE_HEADER: usize = 80;
const HEADER_HITS: usize = 56;
const HEADER_MISSES: usize = 60;
const HEADER_EVICTIONS: usize = 64;
const HEADER_HASH: usize = 68;
pub(crate) const HOT_BUDGET: usize = 8 * 1024 * 1024;
pub(crate) const SLOT_SIZE: usize = 64 * 1024;
const ENTRY_SIZE: usize = 48;
const ENTRY_HITS: usize = 4;
const ENTRY_LAST: usize = 8;
const ENTRY_SLOT: usize = 16;
const ENTRY_LEN: usize = 20;
const ENTRY_SCOPE: usize = 24;
const ENTRY_TASK: usize = 28;
const ENTRY_HASH: usize = 32;
const MAX_ENTRIES: usize = 1024;
const SKETCH_ROWS: usize = 4;
const SKETCH_COLS: usize = 1024;
const SKETCH_BYTES: usize = 4 + SKETCH_ROWS * SKETCH_COLS;

#[derive(Clone, Copy, Debug)]
pub(crate) struct CacheHeader {
    pub index_hash: u64,
    pub index_len: u64,
    pub table_off: u64,
    pub table_cap: u32,
    pub slot_size: u32,
    pub slot_count: u32,
    pub data_off: u64,
    pub sketch_off: u32,
    pub hits: u32,
    pub misses: u32,
    pub evictions: u32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct CacheStats {
    pub capacity: u32,
    pub entries: u32,
    pub data_entries: u32,
    pub index_entries: u32,
    pub hits: u32,
    pub misses: u32,
    pub evictions: u32,
    pub bytes: u64,
}

enum CacheData {
    Mmap(MmapMut),
    Memory(Vec<u8>),
}

impl CacheData {
    fn as_mut_slice(&mut self) -> &mut [u8] {
        match self {
            Self::Mmap(mmap) => mmap,
            Self::Memory(bytes) => bytes,
        }
    }

    fn as_slice(&self) -> &[u8] {
        match self {
            Self::Mmap(mmap) => mmap,
            Self::Memory(bytes) => bytes,
        }
    }
}

pub(crate) struct Cache {
    data: CacheData,
    header: CacheHeader,
    file: Option<File>,
    active_scope_tag: u32,
    active_task_tag: u32,
}

struct CacheGuard<'a> {
    file: &'a File,
}

impl Drop for CacheGuard<'_> {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn lock_file(file: &File) -> Result<CacheGuard<'_>> {
    let deadline = Instant::now() + Duration::from_millis(25);
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(CacheGuard { file }),
            Err(error) => {
                if Instant::now() >= deadline {
                    return Err(anyhow!("cache lock busy: {error}"));
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
}

impl Cache {
    /// The cache holds decompressed data blocks and hot index pages.
    /// `index_len` is always zero in the header: the index itself lives in the
    /// store and is read lazily through `store::Index`.
    pub(crate) fn open_existing(store_path: &Path, index_hash: u64) -> Option<Self> {
        let path = cache_path(store_path);
        let (size, table_off, table_cap, data_off, slot_count, sketch_off) = layout().ok()?;
        let (file, mmap) = open_cache(&path).ok()?;
        let header = decode_header(&mmap)?;
        if header.index_hash == index_hash
            && header.index_len == 0
            && header.table_off == table_off as u64
            && header.table_cap == table_cap as u32
            && header.slot_size == SLOT_SIZE as u32
            && header.slot_count == slot_count as u32
            && header.data_off == data_off as u64
            && header.sketch_off == sketch_off as u32
            && mmap.len() == size
        {
            Some(Self {
                data: CacheData::Mmap(mmap),
                header,
                file: Some(file),
                active_scope_tag: 0,
                active_task_tag: 0,
            })
        } else {
            None
        }
    }

    /// Build the disposable cache in a private temporary file and rename it
    /// into place. Existing readers keep their old mapping valid; a cache file
    /// is never truncated underneath a mapping. The header is written last so
    /// a partially-created file can never validate.
    pub(crate) fn create(store_path: &Path, index_hash: u64) -> Result<Self> {
        let path = cache_path(store_path);
        let (size, table_off, table_cap, data_off, slot_count, sketch_off) = layout()?;
        let tmp = temp_cache_path(&path);
        let _ = fs::remove_file(&tmp);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .with_context(|| format!("create cache {}", tmp.display()))?;
        let build = (|| -> Result<()> {
            file.set_len(size as u64)?;
            let header = CacheHeader {
                index_hash,
                index_len: 0,
                table_off: table_off as u64,
                table_cap: table_cap as u32,
                slot_size: SLOT_SIZE as u32,
                slot_count: slot_count as u32,
                data_off: data_off as u64,
                sketch_off: sketch_off as u32,
                hits: 0,
                misses: 0,
                evictions: 0,
            };
            let encoded = encode_header(&header);
            file.seek(SeekFrom::Start(0))?;
            file.write_all(&encoded)?;
            file.sync_all()?;
            Ok(())
        })();
        if let Err(error) = build {
            drop(file);
            let _ = fs::remove_file(&tmp);
            return Err(error);
        }
        drop(file);
        if let Err(error) = fs::rename(&tmp, &path) {
            let _ = fs::remove_file(&tmp);
            return Err(error).with_context(|| format!("install cache {}", path.display()));
        }
        let (file, mmap) = open_cache(&path)?;
        if mmap.len() != size {
            bail!("cache size changed while opening {}", path.display());
        }
        // Another process may have replaced the cache between rename and open.
        // Never trust a cache unless it belongs to this index and layout.
        let actual = decode_header(&mmap).ok_or_else(|| anyhow!("cache header invalid"))?;
        if actual.index_hash != index_hash
            || actual.index_len != 0
            || actual.table_off != table_off as u64
            || actual.table_cap != table_cap as u32
            || actual.slot_size != SLOT_SIZE as u32
            || actual.slot_count != slot_count as u32
            || actual.data_off != data_off as u64
            || actual.sketch_off != sketch_off as u32
        {
            bail!("cache was replaced while opening {}", path.display());
        }
        let header = CacheHeader {
            index_hash,
            index_len: 0,
            table_off: table_off as u64,
            table_cap: table_cap as u32,
            slot_size: SLOT_SIZE as u32,
            slot_count: slot_count as u32,
            data_off: data_off as u64,
            sketch_off: sketch_off as u32,
            hits: 0,
            misses: 0,
            evictions: 0,
        };
        Ok(Self {
            data: CacheData::Mmap(mmap),
            header,
            file: Some(file),
            active_scope_tag: 0,
            active_task_tag: 0,
        })
    }

    /// In-memory fallback used when the disposable cache cannot be created
    /// (read-only directory, concurrent replacement, full disk). Reads must
    /// not fail just because the cache is unavailable.
    pub(crate) fn memory(index_hash: u64) -> Result<Self> {
        let (size, table_off, table_cap, data_off, slot_count, sketch_off) = layout()?;
        let header = CacheHeader {
            index_hash,
            index_len: 0,
            table_off: table_off as u64,
            table_cap: table_cap as u32,
            slot_size: SLOT_SIZE as u32,
            slot_count: slot_count as u32,
            data_off: data_off as u64,
            sketch_off: sketch_off as u32,
            hits: 0,
            misses: 0,
            evictions: 0,
        };
        let mut data = vec![0u8; size];
        data[..CACHE_HEADER].copy_from_slice(&encode_header(&header));
        Ok(Self {
            data: CacheData::Memory(data),
            header,
            file: None,
            active_scope_tag: 0,
            active_task_tag: 0,
        })
    }

    /// Scope/task affinity for keys touched by the current query. Scope is
    /// the durable working set (one coding workspace); task is the ephemeral
    /// working set inside that workspace. Scope protects the project from
    /// foreign blocks; task is a tie-break and a prefetch tag so alternating
    /// tasks can share the cache instead of wiping each other.
    pub(crate) fn set_affinity(&mut self, scope: Option<&str>, task: Option<&str>) {
        self.active_scope_tag = affinity_tag(scope);
        self.active_task_tag = affinity_tag(task);
    }

    pub(crate) fn get_block<F>(&mut self, key: u32, decompress: F) -> Result<Vec<u8>>
    where
        F: FnOnce() -> Result<Vec<u8>>,
    {
        if let Some(block) = self.try_get(key)? {
            return Ok(block);
        }

        let block = decompress()?;
        if block.len() > SLOT_SIZE {
            // Valid records can exceed one cache slot. Reads must still work;
            // they simply bypass the cache instead of failing.
            return Ok(block);
        }

        let guard = if let Some(file) = &self.file {
            match lock_file(file) {
                Ok(guard) => Some(guard),
                // A cache that cannot be locked is still optional: return the
                // freshly decompressed block without mutating the file.
                Err(_) => return Ok(block),
            }
        } else {
            None
        };
        let table_start = self.header.table_off as usize;
        let cap = self.header.table_cap as usize;
        let data_off = self.header.data_off as usize;
        let sketch_off = self.header.sketch_off as usize;
        let active_scope_tag = self.active_scope_tag;
        let active_task_tag = self.active_task_tag;
        let affinity_enabled = active_scope_tag != 0 || active_task_tag != 0;
        let data = self.data.as_mut_slice();
        if let Some(existing) =
            lookup_locked(data, &self.header, active_scope_tag, active_task_tag, key)
        {
            return Ok(existing);
        }

        sketch_bump(data, sketch_off, key);
        let mut victim = 0usize;
        let mut victim_rank = u8::MAX;
        let mut victim_hits = u32::MAX;
        let mut victim_task_penalty = 0u8;
        let mut victim_time = u64::MAX;
        for i in 0..cap {
            let offset = table_start + i * ENTRY_SIZE;
            let len = read_u32(data, offset + ENTRY_LEN);
            if len == 0 {
                victim = i;
                break;
            }
            let entry_task = read_u32(data, offset + ENTRY_TASK);
            let rank = rank_for(
                active_scope_tag,
                active_task_tag,
                read_u32(data, offset + ENTRY_SCOPE),
                entry_task,
            );
            // Task is a tie-break, not a trump card: unrelated tasks must be
            // able to reuse the cache instead of wiping each other's pages.
            let task_penalty = u8::from(active_task_tag != 0 && entry_task != active_task_tag);
            let hits = read_u32(data, offset + ENTRY_HITS);
            let last = read_u64(data, offset + ENTRY_LAST);
            if rank < victim_rank
                || (rank == victim_rank
                    && (hits < victim_hits
                        || (hits == victim_hits
                            && (task_penalty > victim_task_penalty
                                || (task_penalty == victim_task_penalty && last < victim_time)))))
            {
                victim = i;
                victim_rank = rank;
                victim_hits = hits;
                victim_task_penalty = task_penalty;
                victim_time = last;
            }
        }
        let victim_offset = table_start + victim * ENTRY_SIZE;
        let victim_len = read_u32(data, victim_offset + ENTRY_LEN);
        if victim_len > 0 {
            let victim_id = read_u32(data, victim_offset);
            let candidate_freq = sketch_estimate(data, sketch_off, key);
            let victim_freq = sketch_estimate(data, sketch_off, victim_id);
            let same_priority = !affinity_enabled || active_scope_tag == 0 || victim_rank == 2;
            if same_priority && victim_freq >= candidate_freq {
                // TinyLFU admission within the active workspace: a one-off
                // scan must not evict a repeatedly reused block or index page.
                return Ok(block);
            }
        }
        let slot = victim as u32;
        let start = data_off + slot as usize * SLOT_SIZE;
        if victim_len > 0 {
            bump_stat(data, HEADER_EVICTIONS);
        }
        data[start..start + block.len()].copy_from_slice(&block);
        write_u32(data, victim_offset, key);
        write_u32(data, victim_offset + ENTRY_HITS, 1);
        write_u64(data, victim_offset + ENTRY_LAST, now_ms());
        write_u32(data, victim_offset + ENTRY_SLOT, slot);
        write_u32(data, victim_offset + ENTRY_LEN, block.len() as u32);
        if affinity_enabled {
            write_u32(data, victim_offset + ENTRY_SCOPE, active_scope_tag);
            write_u32(data, victim_offset + ENTRY_TASK, active_task_tag);
        } else {
            write_u32(data, victim_offset + ENTRY_SCOPE, 0);
            write_u32(data, victim_offset + ENTRY_TASK, 0);
        }
        write_u64(
            data,
            victim_offset + ENTRY_HASH,
            xxh3_64_with_seed(&block, key as u64),
        );
        drop(guard);
        Ok(block)
    }

    fn try_get(&mut self, key: u32) -> Result<Option<Vec<u8>>> {
        let guard = if let Some(file) = &self.file {
            match lock_file(file) {
                Ok(guard) => Some(guard),
                Err(_) => return Ok(None),
            }
        } else {
            None
        };
        let active_scope_tag = self.active_scope_tag;
        let active_task_tag = self.active_task_tag;
        let data = self.data.as_mut_slice();
        let block = lookup_locked(data, &self.header, active_scope_tag, active_task_tag, key);
        if block.is_some() {
            bump_stat(data, HEADER_HITS);
        } else {
            bump_stat(data, HEADER_MISSES);
        }
        drop(guard);
        Ok(block)
    }

    pub(crate) fn stats(&self) -> CacheStats {
        let data = self.data.as_slice();
        let table_start = self.header.table_off as usize;
        let cap = self.header.table_cap as usize;
        let mut entries = 0u32;
        let mut data_entries = 0u32;
        let mut index_entries = 0u32;
        for i in 0..cap {
            let offset = table_start + i * ENTRY_SIZE;
            if read_u32(data, offset + ENTRY_LEN) > 0 {
                entries += 1;
                if read_u32(data, offset) & 0x8000_0000 != 0 {
                    index_entries += 1;
                } else {
                    data_entries += 1;
                }
            }
        }
        CacheStats {
            capacity: cap as u32,
            entries,
            data_entries,
            index_entries,
            hits: read_u32(data, HEADER_HITS),
            misses: read_u32(data, HEADER_MISSES),
            evictions: read_u32(data, HEADER_EVICTIONS),
            bytes: data.len() as u64,
        }
    }
}

fn lookup_locked(
    data: &mut [u8],
    header: &CacheHeader,
    active_scope_tag: u32,
    active_task_tag: u32,
    key: u32,
) -> Option<Vec<u8>> {
    let table_start = header.table_off as usize;
    let cap = header.table_cap as usize;
    let data_off = header.data_off as usize;
    let sketch_off = header.sketch_off as usize;
    for i in 0..cap {
        let offset = table_start + i * ENTRY_SIZE;
        if read_u32(data, offset) != key {
            continue;
        }
        let len = read_u32(data, offset + ENTRY_LEN) as usize;
        if len == 0 || len > SLOT_SIZE {
            continue;
        }
        let slot = read_u32(data, offset + ENTRY_SLOT) as usize;
        let start = data_off + slot * SLOT_SIZE;
        if start.checked_add(len).is_none_or(|end| end > data.len()) {
            write_u32(data, offset + ENTRY_LEN, 0);
            continue;
        }
        let block = data[start..start + len].to_vec();
        if xxh3_64_with_seed(&block, key as u64) != read_u64(data, offset + ENTRY_HASH) {
            // Torn or stale cache slot: drop the entry and fall through to a
            // fresh decompression instead of returning bad bytes.
            write_u32(data, offset + ENTRY_LEN, 0);
            continue;
        }
        let hits = read_u32(data, offset + ENTRY_HITS).saturating_add(1);
        write_u32(data, offset + ENTRY_HITS, hits);
        write_u64(data, offset + ENTRY_LAST, now_ms());
        // A task-only query must not erase the durable scope tag; a scope-only
        // query likewise leaves the task hint intact.
        if active_scope_tag != 0 {
            write_u32(data, offset + ENTRY_SCOPE, active_scope_tag);
        }
        if active_task_tag != 0 {
            write_u32(data, offset + ENTRY_TASK, active_task_tag);
        }
        sketch_bump(data, sketch_off, key);
        return Some(block);
    }
    None
}

pub(crate) fn cache_path(store_path: &Path) -> PathBuf {
    let name = store_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "cm".to_string());
    store_path.with_file_name(format!("{name}.cache"))
}

fn temp_cache_path(path: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "cm.cache".to_string());
    path.with_file_name(format!(
        "{name}.tmp.{}.{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

pub(crate) fn layout() -> Result<(usize, usize, usize, usize, usize, usize)> {
    // The cache is deliberately fixed at the hot budget. It stores both
    // decompressed data blocks and hot 64 KiB index pages.
    let size = HOT_BUDGET;
    let table_off = align8(CACHE_HEADER);
    let available = size.saturating_sub(table_off);
    // Use the hot budget for slots, not for unused table entries. Reserve a
    // small TinyLFU frequency sketch before the block slots.
    let mut cap =
        (available.saturating_sub(SKETCH_BYTES) / (ENTRY_SIZE + SLOT_SIZE)).clamp(1, MAX_ENTRIES);
    loop {
        let table_end = align8(table_off + cap * ENTRY_SIZE);
        let sketch_off = align8(table_end);
        let data_off = align8(sketch_off + SKETCH_BYTES);
        let slots = size.saturating_sub(data_off) / SLOT_SIZE;
        if slots >= cap {
            return Ok((size, table_off, cap, data_off, slots, sketch_off));
        }
        if cap <= 1 {
            bail!("cache layout does not fit");
        }
        cap -= 1;
    }
}

fn open_cache(path: &Path) -> Result<(File, MmapMut)> {
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    let mmap = unsafe { MmapOptions::new().map_mut(&file).context("mmap cache")? };
    Ok((file, mmap))
}

fn encode_header(header: &CacheHeader) -> [u8; CACHE_HEADER] {
    let mut bytes = [0u8; CACHE_HEADER];
    bytes[0..4].copy_from_slice(CACHE_MAGIC);
    bytes[4] = CACHE_VERSION;
    bytes[5] = CACHE_LAYOUT;
    bytes[6..8].copy_from_slice(&(CACHE_HEADER as u16).to_le_bytes());
    bytes[8..16].copy_from_slice(&header.index_hash.to_le_bytes());
    bytes[16..24].copy_from_slice(&header.index_len.to_le_bytes());
    bytes[24..32].copy_from_slice(&header.table_off.to_le_bytes());
    bytes[32..36].copy_from_slice(&header.table_cap.to_le_bytes());
    bytes[36..40].copy_from_slice(&header.slot_size.to_le_bytes());
    bytes[40..44].copy_from_slice(&header.slot_count.to_le_bytes());
    bytes[44..48].copy_from_slice(&header.sketch_off.to_le_bytes());
    bytes[48..56].copy_from_slice(&header.data_off.to_le_bytes());
    bytes[HEADER_HITS..HEADER_HITS + 4].copy_from_slice(&header.hits.to_le_bytes());
    bytes[HEADER_MISSES..HEADER_MISSES + 4].copy_from_slice(&header.misses.to_le_bytes());
    bytes[HEADER_EVICTIONS..HEADER_EVICTIONS + 4].copy_from_slice(&header.evictions.to_le_bytes());
    // Counters are intentionally outside the checksum: they are advisory
    // statistics and must not invalidate the whole cache when torn.
    let hash = xxh3_64(&bytes[0..56]);
    bytes[HEADER_HASH..HEADER_HASH + 8].copy_from_slice(&hash.to_le_bytes());
    bytes
}

fn decode_header(mmap: &[u8]) -> Option<CacheHeader> {
    if mmap.len() < CACHE_HEADER
        || &mmap[0..4] != CACHE_MAGIC
        || mmap[4] != CACHE_VERSION
        || mmap[5] != CACHE_LAYOUT
        || xxh3_64(&mmap[0..56])
            != u64::from_le_bytes(mmap[HEADER_HASH..HEADER_HASH + 8].try_into().ok()?)
    {
        return None;
    }
    Some(CacheHeader {
        index_hash: u64::from_le_bytes(mmap[8..16].try_into().ok()?),
        index_len: u64::from_le_bytes(mmap[16..24].try_into().ok()?),
        table_off: u64::from_le_bytes(mmap[24..32].try_into().ok()?),
        table_cap: u32::from_le_bytes(mmap[32..36].try_into().ok()?),
        slot_size: u32::from_le_bytes(mmap[36..40].try_into().ok()?),
        slot_count: u32::from_le_bytes(mmap[40..44].try_into().ok()?),
        data_off: u64::from_le_bytes(mmap[48..56].try_into().ok()?),
        sketch_off: u32::from_le_bytes(mmap[44..48].try_into().ok()?),
        hits: u32::from_le_bytes(mmap[HEADER_HITS..HEADER_HITS + 4].try_into().ok()?),
        misses: u32::from_le_bytes(mmap[HEADER_MISSES..HEADER_MISSES + 4].try_into().ok()?),
        evictions: u32::from_le_bytes(
            mmap[HEADER_EVICTIONS..HEADER_EVICTIONS + 4]
                .try_into()
                .ok()?,
        ),
    })
}

fn sketch_index(block_id: u32, row: usize) -> usize {
    let hash = xxh3_64_with_seed(&block_id.to_le_bytes(), row as u64);
    (hash as usize) % SKETCH_COLS
}

fn sketch_bump(data: &mut [u8], sketch_off: usize, block_id: u32) {
    let accesses = read_u32(data, sketch_off).saturating_add(1);
    write_u32(data, sketch_off, accesses);
    for row in 0..SKETCH_ROWS {
        let offset = sketch_off + 4 + row * SKETCH_COLS + sketch_index(block_id, row);
        data[offset] = data[offset].saturating_add(1);
    }
    if accesses.is_multiple_of(8192) {
        for byte in &mut data[sketch_off + 4..sketch_off + 4 + SKETCH_ROWS * SKETCH_COLS] {
            *byte /= 2;
        }
    }
}

fn sketch_estimate(data: &[u8], sketch_off: usize, block_id: u32) -> u8 {
    let mut estimate = u8::MAX;
    for row in 0..SKETCH_ROWS {
        let offset = sketch_off + 4 + row * SKETCH_COLS + sketch_index(block_id, row);
        estimate = estimate.min(data[offset]);
    }
    estimate
}

fn align8(value: usize) -> usize {
    (value + 7) & !7
}

pub(crate) fn affinity_tag(value: Option<&str>) -> u32 {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return 0;
    };
    let tag = xxh3_64(value.as_bytes()) as u32;
    if tag == 0 {
        1
    } else {
        tag
    }
}

fn rank_for(active_scope: u32, active_task: u32, scope_tag: u32, task_tag: u32) -> u8 {
    if active_scope == 0 && active_task == 0 {
        return 1;
    }
    if (active_scope != 0 && scope_tag == active_scope)
        || (active_scope == 0 && active_task != 0 && task_tag == active_task)
    {
        2
    } else if scope_tag == 0 && task_tag == 0 {
        0
    } else {
        1
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
fn bump_stat(data: &mut [u8], offset: usize) {
    write_u32(data, offset, read_u32(data, offset).saturating_add(1));
}
fn write_u64(data: &mut [u8], offset: usize, value: u64) {
    data[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}
