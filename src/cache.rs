use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use memmap2::{MmapMut, MmapOptions};
use xxhash_rust::xxh3::xxh3_64;

use crate::model::now_ms;

const CACHE_MAGIC: &[u8; 4] = b"CMLU";
const CACHE_VERSION: u8 = 1;
const CACHE_HEADER: usize = 64;
const MAX_CACHE_SIZE: usize = 256 * 1024 * 1024;
const MIN_CACHE_SIZE: usize = 8 * 1024 * 1024;
const HOT_BUDGET: usize = 8 * 1024 * 1024;
const SLOT_SIZE: usize = 64 * 1024;
const ENTRY_SIZE: usize = 32;
const MAX_ENTRIES: usize = 1024;

#[derive(Clone, Copy, Debug)]
pub(crate) struct CacheHeader {
    pub index_hash: u64,
    pub index_len: u64,
    pub table_off: u64,
    pub table_cap: u32,
    pub slot_size: u32,
    pub slot_count: u32,
    pub data_off: u64,
}

pub(crate) struct Cache {
    mmap: MmapMut,
    header: CacheHeader,
}

impl Cache {
    pub(crate) fn open_existing(
        store_path: &Path,
        index_hash: u64,
        index_len: usize,
    ) -> Option<Self> {
        let path = cache_path(store_path);
        let (size, _, table_cap, _, slot_count) = layout(index_len).ok()?;
        let mmap = open_mmap(&path).ok()?;
        let header = decode_header(&mmap)?;
        if header.index_hash == index_hash
            && header.index_len == index_len as u64
            && header.table_cap == table_cap as u32
            && header.slot_count == slot_count as u32
            && mmap.len() == size
        {
            Some(Self { mmap, header })
        } else {
            None
        }
    }

    pub(crate) fn create(store_path: &Path, index_hash: u64, index_raw: &[u8]) -> Result<Self> {
        let path = cache_path(store_path);
        let (size, table_off, table_cap, data_off, slot_count) = layout(index_raw.len())?;
        if path.exists() {
            fs::remove_file(&path).ok();
        }

        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("create cache {}", path.display()))?;
        file.set_len(size as u64)?;
        let header = CacheHeader {
            index_hash,
            index_len: index_raw.len() as u64,
            table_off: table_off as u64,
            table_cap: table_cap as u32,
            slot_size: SLOT_SIZE as u32,
            slot_count: slot_count as u32,
            data_off: data_off as u64,
        };
        let encoded = encode_header(&header);
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&encoded)?;
        file.seek(SeekFrom::Start(CACHE_HEADER as u64))?;
        file.write_all(index_raw)?;
        file.flush()?;
        drop(file);
        let mmap = open_mmap(&path)?;
        Ok(Self { mmap, header })
    }

    pub(crate) fn index(&self) -> &[u8] {
        &self.mmap[CACHE_HEADER..CACHE_HEADER + self.header.index_len as usize]
    }

    pub(crate) fn get_block<F>(&mut self, block_id: u32, decompress: F) -> Result<Vec<u8>>
    where
        F: FnOnce() -> Result<Vec<u8>>,
    {
        let table_start = self.header.table_off as usize;
        let cap = self.header.table_cap as usize;
        for i in 0..cap {
            let offset = table_start + i * ENTRY_SIZE;
            if read_u32(&self.mmap, offset) == block_id {
                let len = read_u32(&self.mmap, offset + 20) as usize;
                if len > 0 {
                    let slot = read_u32(&self.mmap, offset + 16) as usize;
                    let start = self.header.data_off as usize + slot * SLOT_SIZE;
                    let data = self.mmap[start..start + len].to_vec();
                    let hits = read_u32(&self.mmap, offset + 4).saturating_add(1);
                    write_u32(&mut self.mmap, offset + 4, hits);
                    write_u64(&mut self.mmap, offset + 8, now_ms());
                    return Ok(data);
                }
            }
        }

        let data = decompress()?;
        if data.len() > SLOT_SIZE {
            bail!("cache block too large: {}", data.len());
        }
        let mut victim = 0usize;
        let mut victim_hits = u32::MAX;
        let mut victim_time = u64::MAX;
        let mut found_empty = false;
        for i in 0..cap {
            let offset = table_start + i * ENTRY_SIZE;
            let len = read_u32(&self.mmap, offset + 20);
            if len == 0 {
                victim = i;
                found_empty = true;
                break;
            }
            let hits = read_u32(&self.mmap, offset + 4);
            let last = read_u64(&self.mmap, offset + 8);
            if hits < victim_hits || (hits == victim_hits && last < victim_time) {
                victim = i;
                victim_hits = hits;
                victim_time = last;
            }
        }
        let _ = found_empty;
        let slot = victim as u32;
        let offset = table_start + victim * ENTRY_SIZE;
        let start = self.header.data_off as usize + slot as usize * SLOT_SIZE;
        self.mmap[start..start + data.len()].copy_from_slice(&data);
        write_u32(&mut self.mmap, offset, block_id);
        write_u32(&mut self.mmap, offset + 4, 1);
        write_u64(&mut self.mmap, offset + 8, now_ms());
        write_u32(&mut self.mmap, offset + 16, slot);
        write_u32(&mut self.mmap, offset + 20, data.len() as u32);
        Ok(data)
    }
}

fn cache_path(store_path: &Path) -> PathBuf {
    store_path.with_file_name("cm.lru")
}

fn layout(index_len: usize) -> Result<(usize, usize, usize, usize, usize)> {
    let size = (index_len + HOT_BUDGET).clamp(MIN_CACHE_SIZE, MAX_CACHE_SIZE);
    let table_off = align8(CACHE_HEADER + index_len);
    let mut cap = MAX_ENTRIES;
    loop {
        let table_end = align8(table_off + cap * ENTRY_SIZE);
        if table_end + cap * SLOT_SIZE <= size {
            let slots = (size - table_end) / SLOT_SIZE;
            return Ok((size, table_off, cap, table_end, slots));
        }
        if cap <= 1 {
            bail!("index too large for cache");
        }
        cap /= 2;
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
    })
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

#[allow(dead_code)]
fn _keep_file(_: File) {}
