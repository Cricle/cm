use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{bail, Context, Result};
use memmap2::{MmapMut, MmapOptions};
use xxhash_rust::xxh3::{xxh3_64, xxh3_64_with_seed};

use crate::model::now_ms;

const CACHE_MAGIC: &[u8; 4] = b"CMLU";
const CACHE_VERSION: u8 = 1;
const CACHE_HEADER: usize = 64;
const HOT_BUDGET: usize = 8 * 1024 * 1024;
const SLOT_SIZE: usize = 64 * 1024;
const ENTRY_SIZE: usize = 32;
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
}

pub(crate) struct Cache {
    data: CacheData,
    header: CacheHeader,
}

impl Cache {
    /// The cache holds only decompressed data blocks. `index_len` is always
    /// zero in the header: the index itself lives in the store and is read
    /// lazily through `store::Index`.
    pub(crate) fn open_existing(store_path: &Path, index_hash: u64) -> Option<Self> {
        let path = cache_path(store_path);
        let (size, _, table_cap, _, slot_count, sketch_off) = layout().ok()?;
        let mmap = open_mmap(&path).ok()?;
        let header = decode_header(&mmap)?;
        if header.index_hash == index_hash
            && header.index_len == 0
            && header.table_cap == table_cap as u32
            && header.slot_count == slot_count as u32
            && header.sketch_off == sketch_off as u32
            && mmap.len() == size
        {
            Some(Self {
                data: CacheData::Mmap(mmap),
                header,
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
        let header = CacheHeader {
            index_hash,
            index_len: 0,
            table_off: table_off as u64,
            table_cap: table_cap as u32,
            slot_size: SLOT_SIZE as u32,
            slot_count: slot_count as u32,
            data_off: data_off as u64,
            sketch_off: sketch_off as u32,
        };
        let mmap = open_mmap(&path)?;
        if mmap.len() != size {
            bail!("cache size changed while opening {}", path.display());
        }
        Ok(Self {
            data: CacheData::Mmap(mmap),
            header,
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
        };
        let mut data = vec![0u8; size];
        data[..CACHE_HEADER].copy_from_slice(&encode_header(&header));
        Ok(Self {
            data: CacheData::Memory(data),
            header,
        })
    }

    pub(crate) fn get_block<F>(&mut self, block_id: u32, decompress: F) -> Result<Vec<u8>>
    where
        F: FnOnce() -> Result<Vec<u8>>,
    {
        let table_start = self.header.table_off as usize;
        let cap = self.header.table_cap as usize;
        let data_off = self.header.data_off as usize;
        let sketch_off = self.header.sketch_off as usize;
        let data = self.data.as_mut_slice();
        for i in 0..cap {
            let offset = table_start + i * ENTRY_SIZE;
            if read_u32(data, offset) == block_id {
                let len = read_u32(data, offset + 20) as usize;
                if len > 0 {
                    let slot = read_u32(data, offset + 16) as usize;
                    let start = data_off + slot * SLOT_SIZE;
                    let block = data[start..start + len].to_vec();
                    let hits = read_u32(data, offset + 4).saturating_add(1);
                    write_u32(data, offset + 4, hits);
                    write_u64(data, offset + 8, now_ms());
                    sketch_bump(data, sketch_off, block_id);
                    return Ok(block);
                }
            }
        }

        let block = decompress()?;
        if block.len() > SLOT_SIZE {
            bail!("cache block too large: {}", block.len());
        }
        sketch_bump(data, sketch_off, block_id);
        let mut victim = 0usize;
        let mut victim_hits = u32::MAX;
        let mut victim_time = u64::MAX;
        for i in 0..cap {
            let offset = table_start + i * ENTRY_SIZE;
            let len = read_u32(data, offset + 20);
            if len == 0 {
                victim = i;
                break;
            }
            let hits = read_u32(data, offset + 4);
            let last = read_u64(data, offset + 8);
            if hits < victim_hits || (hits == victim_hits && last < victim_time) {
                victim = i;
                victim_hits = hits;
                victim_time = last;
            }
        }
        let victim_offset = table_start + victim * ENTRY_SIZE;
        let victim_len = read_u32(data, victim_offset + 20);
        if victim_len > 0 {
            let victim_id = read_u32(data, victim_offset);
            let candidate_freq = sketch_estimate(data, sketch_off, block_id);
            let victim_freq = sketch_estimate(data, sketch_off, victim_id);
            if victim_freq >= candidate_freq {
                // TinyLFU admission: a one-off scan must not evict a
                // repeatedly reused block.
                return Ok(block);
            }
        }
        let slot = victim as u32;
        let start = data_off + slot as usize * SLOT_SIZE;
        data[start..start + block.len()].copy_from_slice(&block);
        write_u32(data, victim_offset, block_id);
        write_u32(data, victim_offset + 4, 1);
        write_u64(data, victim_offset + 8, now_ms());
        write_u32(data, victim_offset + 16, slot);
        write_u32(data, victim_offset + 20, block.len() as u32);
        Ok(block)
    }
}

pub(crate) fn cache_path(store_path: &Path) -> PathBuf {
    store_path.with_file_name("cm.cache")
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

fn layout() -> Result<(usize, usize, usize, usize, usize, usize)> {
    // The block cache is deliberately fixed at the hot budget; the index is
    // no longer copied here.
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

fn open_mmap(path: &Path) -> Result<MmapMut> {
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    unsafe { MmapOptions::new().map_mut(&file).context("mmap cache") }
}

fn encode_header(header: &CacheHeader) -> [u8; CACHE_HEADER] {
    let mut bytes = [0u8; CACHE_HEADER];
    bytes[0..4].copy_from_slice(CACHE_MAGIC);
    bytes[4] = CACHE_VERSION;
    bytes[6..8].copy_from_slice(&(CACHE_HEADER as u16).to_le_bytes());
    bytes[8..16].copy_from_slice(&header.index_hash.to_le_bytes());
    bytes[16..24].copy_from_slice(&header.index_len.to_le_bytes());
    bytes[24..32].copy_from_slice(&header.table_off.to_le_bytes());
    bytes[32..36].copy_from_slice(&header.table_cap.to_le_bytes());
    bytes[36..40].copy_from_slice(&header.slot_size.to_le_bytes());
    bytes[40..44].copy_from_slice(&header.slot_count.to_le_bytes());
    bytes[44..48].copy_from_slice(&header.sketch_off.to_le_bytes());
    bytes[48..56].copy_from_slice(&header.data_off.to_le_bytes());
    let hash = xxh3_64(&bytes[0..56]);
    bytes[56..64].copy_from_slice(&hash.to_le_bytes());
    bytes
}

fn decode_header(mmap: &[u8]) -> Option<CacheHeader> {
    if mmap.len() < CACHE_HEADER
        || &mmap[0..4] != CACHE_MAGIC
        || mmap[4] != CACHE_VERSION
        || xxh3_64(&mmap[0..56]) != u64::from_le_bytes(mmap[56..64].try_into().ok()?)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tinylfu_rejects_scan_pollution() {
        let dir = std::env::temp_dir().join(format!("cm-cache-test-{}", crate::model::new_id()));
        fs::create_dir_all(&dir).expect("create temp dir");
        let store = dir.join("cm");
        let (_, _, cap, _, _, _) = layout().expect("layout");
        let mut cache = Cache::create(&store, 7).expect("create cache");
        for id in 1..=cap as u32 {
            for _ in 0..2 {
                cache
                    .get_block(id, || Ok(vec![id as u8; 1024]))
                    .expect("fill hot");
            }
        }
        for id in 0..cap * 4 {
            cache
                .get_block(20_000 + id as u32, || Ok(vec![(id % 250) as u8; 1024]))
                .expect("scan");
        }
        let hot = cache
            .get_block(1, || panic!("hot block was evicted"))
            .expect("hot hit");
        assert_eq!(hot[0], 1);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn layout_uses_all_hot_slots() {
        let (size, _table_off, cap, data_off, slots, sketch_off) = layout().expect("layout");
        assert_eq!(size, HOT_BUDGET);
        assert_eq!(cap, slots);
        assert!(sketch_off < data_off);
        assert!(data_off + slots * SLOT_SIZE <= size);
    }

    #[test]
    fn cache_create_reopen_and_memory_fallback() {
        let dir = std::env::temp_dir().join(format!("cm-cache-test-{}", crate::model::new_id()));
        fs::create_dir_all(&dir).expect("create temp dir");
        let store = dir.join("cm");
        let mut cache = Cache::create(&store, 42).expect("create cache");
        let block = cache
            .get_block(7, || Ok(vec![7u8; 1024]))
            .expect("fill block");
        assert_eq!(block.len(), 1024);
        drop(cache);

        let mut reopened = Cache::open_existing(&store, 42).expect("reopen cache");
        let block = reopened
            .get_block(7, || panic!("block cache did not persist"))
            .expect("cached block");
        assert_eq!(block[0], 7);
        assert!(Cache::open_existing(&store, 43).is_none());
        drop(reopened);

        let mut memory = Cache::memory(42).expect("memory cache");
        let block = memory
            .get_block(9, || Ok(vec![9u8; 512]))
            .expect("memory block");
        assert_eq!(block[0], 9);
        fs::remove_dir_all(&dir).ok();
    }
}
