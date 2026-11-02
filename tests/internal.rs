//! White-box tests. The production modules are compiled as a private test
//! crate so internal invariants (headers, page repair, cache admission) can be
//! checked without putting test code in `src/`.
//!
//! Only the paths exercised below are used from this test crate; production
//! dead code is still rejected by `cargo clippy` on the real lib/bin targets.
#![allow(dead_code)]

#[path = "../src/cache.rs"]
mod cache;
#[path = "../src/cli.rs"]
mod cli;
#[path = "../src/install.rs"]
mod install;
#[path = "../src/model.rs"]
mod model;
#[path = "../src/store.rs"]
mod store;

use cache::*;
use cli::*;
use install::*;
use model::*;
use std::env;
use std::fs;
use std::fs::OpenOptions;
use std::io::{Seek, Write};
use std::path::{Path, PathBuf};
use store::*;

fn cached_block_tags(store_path: &Path, block_id: u32) -> Option<(u32, u32)> {
    cached_entry(store_path, block_id).map(|(_, scope, task)| (scope, task))
}

fn cached_entry(store_path: &Path, key: u32) -> Option<(u32, u32, u32)> {
    let (_, table_off, cap, _, _, _) = cache::layout().expect("cache layout");
    let data = fs::read(store_path.with_file_name("cm.cache")).expect("read cache");
    for index in 0..cap {
        let offset = table_off + index * 48;
        let id = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
        let len = u32::from_le_bytes(data[offset + 20..offset + 24].try_into().unwrap());
        if id == key && len > 0 {
            let scope = u32::from_le_bytes(data[offset + 24..offset + 28].try_into().unwrap());
            let task = u32::from_le_bytes(data[offset + 28..offset + 32].try_into().unwrap());
            return Some((id, scope, task));
        }
    }
    None
}

fn cached_keys(store_path: &Path) -> Vec<u32> {
    let (_, table_off, cap, _, _, _) = cache::layout().expect("cache layout");
    let data = fs::read(store_path.with_file_name("cm.cache")).expect("read cache");
    (0..cap)
        .filter_map(|index| {
            let offset = table_off + index * 48;
            let len = u32::from_le_bytes(data[offset + 20..offset + 24].try_into().unwrap());
            (len > 0).then(|| u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()))
        })
        .collect()
}

fn cached_slot(store_path: &Path, key: u32) -> Option<(usize, usize, usize)> {
    let (_, table_off, cap, data_off, _, _) = cache::layout().expect("cache layout");
    let data = fs::read(store_path.with_file_name("cm.cache")).expect("read cache");
    for index in 0..cap {
        let offset = table_off + index * 48;
        let id = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
        let len = u32::from_le_bytes(data[offset + 20..offset + 24].try_into().unwrap());
        if id == key && len > 0 {
            let slot = u32::from_le_bytes(data[offset + 16..offset + 20].try_into().unwrap());
            return Some((
                offset,
                data_off + slot as usize * cache::SLOT_SIZE,
                len as usize,
            ));
        }
    }
    None
}

#[cfg(test)]
mod model_tests {
    use super::*;

    #[test]
    fn memory_payload_roundtrip_preserves_provenance() {
        let mut memory = new_memory(
            "fact",
            "repo",
            "database",
            "PostgreSQL 16 is primary",
            "README.md",
            0.9,
            vec!["source-id".to_string()],
            Some("manual".to_string()),
            Some(0.8),
        );
        memory.valid_from = 1234;
        memory.valid_to = Some(5678);
        memory.supersedes = vec!["old-id".to_string()];
        memory.conflict_of = Some("conflict-id".to_string());
        memory.trust = "verified".to_string();
        memory.source_commit = Some("abc123".to_string());

        let encoded = memory_payload(&memory);
        let decoded = decode_memory(&encoded).expect("decode");
        assert_eq!(decoded.id, memory.id);
        assert_eq!(decoded.valid_from, 1234);
        assert_eq!(decoded.valid_to, Some(5678));
        assert_eq!(decoded.supersedes, vec!["old-id"]);
        assert_eq!(decoded.conflict_of.as_deref(), Some("conflict-id"));
        assert_eq!(decoded.trust, "verified");
        assert_eq!(decoded.source_commit.as_deref(), Some("abc123"));
    }

    #[test]
    fn tokenizer_handles_ascii_and_cjk() {
        let tokens = tokenize("topic 使用 memory 16");
        assert!(tokens.iter().any(|token| token == "topic"));
        assert!(tokens.iter().any(|token| token == "使用"));
        assert!(tokens.iter().any(|token| token == "memory"));
    }

    #[test]
    fn token_estimate_counts_cjk_character_wise() {
        assert_eq!(estimate_tokens("hello world"), 3);
        assert_eq!(estimate_tokens("使用记忆"), 4);
        assert_eq!(estimate_tokens("hello 使用"), 4);
    }
}

#[cfg(test)]
mod cache_tests {
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

    #[test]
    fn affinity_tags_follow_scope_and_task() {
        let dir = env::temp_dir().join(format!("cm-cache-affinity-{}", model::new_id()));
        fs::create_dir_all(&dir).expect("create temp dir");
        let store = dir.join("cm");
        let mut cache = Cache::create(&store, 7).expect("create cache");
        let scope = affinity_tag(Some("repo-a"));
        let task_a = affinity_tag(Some("task-a"));
        let task_b = affinity_tag(Some("task-b"));

        cache.set_affinity(Some("repo-a"), Some("task-a"));
        cache.get_block(1, || Ok(vec![1u8; 1024])).expect("block 1");
        assert_eq!(cached_block_tags(&store, 1), Some((scope, task_a)));

        cache.set_affinity(Some("repo-a"), Some("task-b"));
        cache.get_block(2, || Ok(vec![2u8; 1024])).expect("block 2");
        assert_eq!(cached_block_tags(&store, 2), Some((scope, task_b)));

        cache
            .get_block(1, || panic!("block 1 must remain cached"))
            .expect("hit block 1");
        assert_eq!(
            cached_block_tags(&store, 1),
            Some((scope, task_b)),
            "a hit refreshes the task tag"
        );

        cache.set_affinity(None, None);
        cache
            .get_block(1, || panic!("block 1 must remain cached"))
            .expect("global hit");
        assert_eq!(
            cached_block_tags(&store, 1),
            Some((scope, task_b)),
            "a global hit must not erase the working-set tag"
        );

        let task_c = affinity_tag(Some("task-c"));
        cache.set_affinity(None, Some("task-c"));
        cache
            .get_block(1, || panic!("block 1 must remain cached"))
            .expect("task-only hit");
        assert_eq!(
            cached_block_tags(&store, 1),
            Some((scope, task_c)),
            "a task-only hit must preserve the durable scope tag"
        );

        cache.set_affinity(Some("repo-a"), None);
        cache
            .get_block(1, || panic!("block 1 must remain cached"))
            .expect("scope-only hit");
        assert_eq!(
            cached_block_tags(&store, 1),
            Some((scope, task_c)),
            "a scope-only hit must preserve the task hint"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cache_rejects_a_foreign_index_hash() {
        let dir = env::temp_dir().join(format!("cm-cache-replace-{}", model::new_id()));
        fs::create_dir_all(&dir).expect("create temp dir");
        let store = dir.join("cm");
        drop(Cache::create(&store, 7).expect("first cache"));
        drop(Cache::create(&store, 8).expect("replacement cache"));
        assert!(Cache::open_existing(&store, 7).is_none());
        assert!(Cache::open_existing(&store, 8).is_some());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn oversized_block_bypasses_the_cache() {
        let dir = env::temp_dir().join(format!("cm-cache-large-{}", model::new_id()));
        fs::create_dir_all(&dir).expect("create temp dir");
        let store = dir.join("cm");
        let mut cache = Cache::create(&store, 7).expect("create cache");
        let block = cache
            .get_block(1, || Ok(vec![9u8; SLOT_SIZE + 1]))
            .expect("oversized read still succeeds");
        assert_eq!(block.len(), SLOT_SIZE + 1);
        assert!(cached_keys(&store).is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn affinity_protects_the_hot_block_across_task_switch() {
        let dir = env::temp_dir().join(format!("cm-cache-working-set-{}", model::new_id()));
        fs::create_dir_all(&dir).expect("create temp dir");
        let store = dir.join("cm");
        let mut cache = Cache::create(&store, 7).expect("create cache");
        let (_, _, cap, _, _, _) = layout().expect("layout");

        cache.set_affinity(Some("repo-a"), Some("task-a"));
        for id in 0..cap as u32 {
            cache
                .get_block(10_000 + id, || Ok(vec![id as u8; 512]))
                .expect("fill working set");
        }
        for _ in 0..8 {
            cache
                .get_block(10_000, || panic!("hot block must be cached"))
                .expect("hot hit");
        }

        cache.set_affinity(Some("repo-a"), Some("task-b"));
        for id in 0..8 {
            cache
                .get_block(20_000 + id, || Ok(vec![id as u8; 512]))
                .expect("switch task");
        }
        cache
            .get_block(10_000, || panic!("hot block was evicted by task switch"))
            .expect("hot block survived");
        fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod store_tests {
    use super::*;
    use crate::model::{build_merged, memory_payload, new_memory};
    use std::time::Duration;

    fn temp_store_path() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cm-store-test-{}", crate::model::new_id()));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir.join("cm")
    }

    fn empty_indexed_store() -> PathBuf {
        let path = temp_store_path();
        let header = Header {
            created_at: 0,
            block_table_off: HEADER_LEN as u64,
            block_count: 0,
            index_off: HEADER_LEN as u64,
            index_len: 0,
            index_raw_len: 0,
            index_hash: 0,
            data_start: HEADER_LEN as u64,
            tail_start: HEADER_LEN as u64,
            doc_count: 0,
            generation: 0,
            flags: 0,
        };
        fs::write(&path, encode_header(&header)).expect("write header");
        reindex_file(&path).expect("reindex empty store");
        path
    }

    fn cleanup(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::remove_dir_all(parent).ok();
        }
    }

    fn sample_record() -> Vec<u8> {
        let memory = new_memory(
            "fact",
            "test",
            "subject",
            "body",
            "audit",
            0.9,
            Vec::new(),
            None,
            None,
        );
        encode_record(T_MEMORY, 0, &memory_payload(&memory)).expect("encode record")
    }

    #[test]
    fn checked_reader_accepts_complete_record() {
        let record = sample_record();
        let records = read_records_from_checked(&record, 0).expect("complete record");
        assert_eq!(records.len(), 1);
    }

    #[test]
    fn checked_reader_rejects_truncated_record_and_trailing_bytes() {
        let record = sample_record();
        assert!(read_records_from_checked(&record[..record.len() - 1], 0).is_err());

        let mut padded = record;
        padded.extend_from_slice(b"junk");
        assert!(read_records_from_checked(&padded, 0).is_err());
    }

    #[test]
    fn checkpoint_waits_for_writer_lock() {
        let path = empty_indexed_store();
        let lock = acquire_store_lock(&path).expect("hold store lock");
        let checkpoint_path = path.clone();
        let handle = std::thread::spawn(move || checkpoint_file(&checkpoint_path, false));
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            !handle.is_finished(),
            "checkpoint must block while a writer holds the store lock"
        );
        drop(lock);
        let header = handle.join().expect("join checkpoint").expect("checkpoint");
        assert_eq!(header.doc_count, 0);
        cleanup(&path);
    }

    #[test]
    fn append_waits_for_writer_lock() {
        let path = empty_indexed_store();
        let before = fs::metadata(&path).expect("metadata").len();
        let lock = acquire_store_lock(&path).expect("hold store lock");
        let append_path = path.clone();
        let handle =
            std::thread::spawn(move || append_record(&append_path, T_MEMORY, b"payload", true));
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            !handle.is_finished(),
            "append must block while a writer holds the store lock"
        );
        drop(lock);
        handle.join().expect("join append").expect("append");
        let data = fs::read(&path).expect("read store");
        assert!(data.len() > before as usize);
        let header = read_header_file(&path).expect("header");
        let records = read_records_from(&data, header.tail_start as usize).expect("tail");
        assert_eq!(records.len(), 1);
        cleanup(&path);
    }

    #[cfg(windows)]
    #[test]
    fn append_does_not_block_concurrent_header_reads() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let path = empty_indexed_store();
        let stopping = Arc::new(AtomicBool::new(false));
        let started = Arc::new(AtomicBool::new(false));
        let reader_path = path.clone();
        let reader_stop = Arc::clone(&stopping);
        let reader_started = Arc::clone(&started);
        let reader = std::thread::spawn(move || {
            reader_started.store(true, Ordering::Release);
            let mut failures = Vec::new();
            while !reader_stop.load(Ordering::Acquire) {
                if let Err(error) = read_header_file(&reader_path) {
                    failures.push(format!("{error:#}"));
                    break;
                }
            }
            failures
        });
        while !started.load(Ordering::Acquire) {
            std::hint::spin_loop();
        }
        append_record(&path, T_MEMORY, &vec![7u8; 8 * 1024 * 1024], true).expect("append");
        stopping.store(true, Ordering::Release);
        let failures = reader.join().expect("join reader");
        assert!(
            failures.is_empty(),
            "header reads must not hit the writer's byte-range lock: {failures:?}"
        );
        cleanup(&path);
    }

    fn test_header(generation: u64, flags: u32) -> Header {
        Header {
            created_at: 1,
            block_table_off: DUAL_HEADER_LEN as u64,
            block_count: 0,
            index_off: DUAL_HEADER_LEN as u64,
            index_len: 1,
            index_raw_len: 1,
            index_hash: 0,
            data_start: DUAL_HEADER_LEN as u64,
            tail_start: DUAL_HEADER_LEN as u64,
            doc_count: 0,
            generation,
            flags,
        }
    }

    #[test]
    fn header_selection_uses_newest_valid_slot() {
        let mut data = vec![0u8; DUAL_HEADER_LEN];
        data[..HEADER_LEN].copy_from_slice(&encode_header(&test_header(3, HEADER_FLAG_DUAL)));
        data[HEADER_SLOT..].copy_from_slice(&encode_header(&test_header(4, HEADER_FLAG_DUAL)));
        let (header, slot) = read_current_header_with_slot(&data).expect("header B");
        assert_eq!((header.generation, slot), (4, HEADER_SLOT));

        data[..HEADER_LEN].copy_from_slice(&encode_header(&test_header(5, HEADER_FLAG_DUAL)));
        let (header, slot) = read_current_header_with_slot(&data).expect("header A");
        assert_eq!((header.generation, slot), (5, 0));

        data[HEADER_SLOT] = b'X';
        let (header, slot) = read_current_header_with_slot(&data).expect("corrupt B fallback");
        assert_eq!((header.generation, slot), (5, 0));

        data[..HEADER_LEN].copy_from_slice(&encode_header(&test_header(2, HEADER_FLAG_DUAL)));
        data[HEADER_SLOT..].copy_from_slice(&encode_header(&test_header(4, HEADER_FLAG_DUAL)));
        data[0] = b'X';
        let (header, slot) = read_current_header_with_slot(&data).expect("corrupt A fallback");
        assert_eq!((header.generation, slot), (4, HEADER_SLOT));

        data[HEADER_SLOT] = b'X';
        assert!(read_current_header_with_slot(&data).is_err());
    }

    #[test]
    fn doc_meta_batch_matches_individual_reads() {
        let path = empty_indexed_store();
        for index in 0..300 {
            let memory = new_memory(
                "fact",
                "test",
                &format!("subject {index}"),
                &format!("body {index}"),
                "audit",
                0.9,
                Vec::new(),
                None,
                None,
            );
            append_record(&path, T_MEMORY, &memory_payload(&memory), true).expect("append");
        }
        reindex_file(&path).expect("reindex");
        let store = Store::open(&path).expect("open");
        let indices: Vec<u32> = (0..300).collect();
        let batch = store.index().doc_meta_batch(&indices).expect("batch");
        assert_eq!(batch.len(), indices.len());
        for (idx, meta) in indices.iter().zip(batch) {
            let single = store.doc_meta(*idx).expect("single");
            assert_eq!(meta.block_id, single.block_id);
            assert_eq!(meta.offset_in_block, single.offset_in_block);
            assert_eq!(meta.record_len, single.record_len);
            assert_eq!(meta.token_count, single.token_count);
            assert_eq!(meta.scope_id, single.scope_id);
            assert_eq!(meta.kind, single.kind);
        }
        cleanup(&path);
    }

    #[test]
    fn reindex_preserves_merged_record_flag() {
        let path = empty_indexed_store();
        let first = new_memory(
            "fact",
            "test",
            "first",
            "first body",
            "audit",
            0.9,
            Vec::new(),
            None,
            None,
        );
        let second = new_memory(
            "fact",
            "test",
            "second",
            "second body",
            "audit",
            0.9,
            Vec::new(),
            None,
            None,
        );
        append_record(&path, T_MEMORY, &memory_payload(&first), true).expect("first");
        append_record(&path, T_MEMORY, &memory_payload(&second), true).expect("second");
        reindex_file(&path).expect("reindex sources");
        let merged = build_merged(
            &first,
            &[first.id.clone(), second.id.clone()],
            "merged body",
            "manual",
            1.0,
        );
        append_record(&path, T_MERGE, &memory_payload(&merged), true).expect("merge");
        reindex_file(&path).expect("reindex merge");

        let mut store = Store::open(&path).expect("open");
        let memories = store.scan_page(usize::MAX, 0).expect("scan");
        assert_eq!(memories.len(), 1);
        assert_eq!(memories[0].id, merged.id);
        assert!(memories[0].merged);
        cleanup(&path);
    }

    #[test]
    fn checkpoint_recovers_partial_index_append() {
        let path = empty_indexed_store();
        let memory = new_memory(
            "fact",
            "test",
            "tail",
            "tail body",
            "audit",
            0.9,
            Vec::new(),
            None,
            None,
        );
        let payload = memory_payload(&memory);
        append_record(&path, T_MEMORY, &payload, true).expect("append");
        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open partial marker");
        file.write_all(INDEX_MARKER).expect("write partial marker");
        file.sync_all().expect("sync partial marker");

        let header = checkpoint_file(&path, true).expect("checkpoint repairs partial marker");
        assert_eq!(header.doc_count, 0);
        let data = fs::read(&path).expect("read repaired");
        assert!(!data.windows(INDEX_MARKER.len()).any(|w| w == INDEX_MARKER));
        assert_eq!(verify_full(&path).expect("verify repaired"), 1);
        cleanup(&path);
    }

    #[test]
    fn records_appended_after_partial_index_are_readable_and_survive_checkpoint() {
        let path = empty_indexed_store();
        let first = new_memory(
            "fact",
            "test",
            "first",
            "first body",
            "audit",
            0.9,
            Vec::new(),
            None,
            None,
        );
        append_record(&path, T_MEMORY, &memory_payload(&first), true).expect("first");
        let header = read_header_file(&path).expect("header");
        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open partial marker");
        file.write_all(INDEX_MARKER).expect("write marker");
        file.write_all(&(1024u64).to_le_bytes())
            .expect("write partial index length");
        file.write_all(b"partial index bytes")
            .expect("write partial index");
        file.sync_all().expect("sync partial index");
        drop(file);
        let second = new_memory(
            "fact",
            "test",
            "second",
            "second body",
            "audit",
            0.9,
            Vec::new(),
            None,
            None,
        );
        append_record(&path, T_MEMORY, &memory_payload(&second), true).expect("second");

        let data = fs::read(&path).expect("read store");
        let tail = read_records_from(&data, header.tail_start as usize).expect("tail");
        assert_eq!(
            tail.len(),
            2,
            "records after a partial index must stay visible"
        );
        let store = Store::open(&path).expect("open store");
        assert_eq!(store.tail_view().expect("tail view").memories.len(), 2);

        let repaired = checkpoint_file(&path, true).expect("checkpoint repairs partial index");
        assert_eq!(repaired.doc_count, 0);
        assert_eq!(verify_full(&path).expect("verify repaired"), 2);
        let data = fs::read(&path).expect("read repaired");
        assert!(!data.windows(INDEX_MARKER.len()).any(|w| w == INDEX_MARKER));
        cleanup(&path);
    }

    #[test]
    fn records_after_short_partial_marker_prefix_are_recovered() {
        for prefix_len in [1usize, 4, 8, 12] {
            let path = empty_indexed_store();
            let header = read_header_file(&path).expect("header");
            let first = new_memory(
                "fact",
                "test",
                "first",
                "first body",
                "audit",
                0.9,
                Vec::new(),
                None,
                None,
            );
            append_record(&path, T_MEMORY, &memory_payload(&first), true).expect("first");
            let mut file = OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open partial marker");
            let mut partial = INDEX_MARKER[..prefix_len.min(INDEX_MARKER.len())].to_vec();
            partial.resize(prefix_len, 0);
            file.write_all(&partial).expect("write marker prefix");
            file.sync_all().expect("sync marker prefix");
            drop(file);
            let second = new_memory(
                "fact",
                "test",
                "second",
                "second body",
                "audit",
                0.9,
                Vec::new(),
                None,
                None,
            );
            append_record(&path, T_MEMORY, &memory_payload(&second), true).expect("second");

            let data = fs::read(&path).expect("read store");
            let tail = read_records_from(&data, header.tail_start as usize).expect("tail");
            assert_eq!(
                tail.len(),
                2,
                "marker prefix length {prefix_len} must not hide later records"
            );
            let repaired = checkpoint_file(&path, true).expect("checkpoint repairs marker prefix");
            assert_eq!(repaired.doc_count, 0);
            assert_eq!(verify_full(&path).expect("verify repaired"), 2);
            cleanup(&path);
        }
    }

    #[test]
    fn checkpoint_rejects_truncated_or_garbage_tail() {
        let path = empty_indexed_store();
        append_record(&path, T_MEMORY, b"payload", true).expect("append");
        let len = fs::metadata(&path).expect("metadata").len();
        OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open truncated")
            .set_len(len - 1)
            .expect("truncate");
        let before = fs::read(&path).expect("read before");
        let error = checkpoint_file(&path, false).expect_err("truncated tail must fail");
        assert!(format!("{error:#}").contains("truncated"), "{error:#}");
        assert_eq!(fs::read(&path).expect("read after"), before);
        assert!(!path.with_file_name("cm.tmp").exists());

        let garbage_path = empty_indexed_store();
        append_record(&garbage_path, T_MEMORY, b"payload", true).expect("append");
        let mut file = OpenOptions::new()
            .append(true)
            .open(&garbage_path)
            .expect("open for garbage");
        file.write_all(b"garbage!").expect("write garbage");
        file.sync_all().expect("sync garbage");
        let error = checkpoint_file(&garbage_path, false).expect_err("garbage tail must fail");
        assert!(format!("{error:#}").contains("offset"), "{error:#}");
        assert!(!garbage_path.with_file_name("cm.tmp").exists());
        cleanup(&path);
        cleanup(&garbage_path);
    }

    #[test]
    fn prefetch_tags_result_blocks_with_scope_and_task() {
        let path = empty_indexed_store();
        for index in 0..40 {
            let memory = new_memory(
                "fact",
                "repo",
                &format!("subject {index}"),
                &format!("body {index} {}", "x".repeat(3000)),
                "audit",
                0.9,
                Vec::new(),
                None,
                None,
            );
            append_record(&path, T_MEMORY, &memory_payload(&memory), true).expect("append");
        }
        reindex_file(&path).expect("reindex");

        let mut store = Store::open(&path).expect("open");
        let ids: Vec<String> = store
            .scan_page(usize::MAX, 0)
            .expect("scan")
            .into_iter()
            .map(|memory| memory.id)
            .collect();
        let indices: Vec<u32> = (0..store.doc_count()).collect();
        let blocks: Vec<u32> = store
            .index()
            .doc_meta_batch(&indices)
            .expect("metas")
            .into_iter()
            .map(|meta| meta.block_id)
            .collect();

        store.set_cache_affinity(Some("repo-x"), Some("task-x"));
        store.prefetch_memory_ids(&ids).expect("prefetch");
        let scope = affinity_tag(Some("repo-x"));
        let task_x = affinity_tag(Some("task-x"));
        for block in &blocks {
            assert_eq!(
                cached_block_tags(&path, *block),
                Some((scope, task_x)),
                "prefetched result block must carry the working-set tags"
            );
        }

        store.set_cache_affinity(Some("repo-x"), Some("task-y"));
        store
            .prefetch_memory_ids(&ids[..1])
            .expect("prefetch second task");
        let first_block = store.index().doc_meta_batch(&[0]).expect("first meta")[0].block_id;
        assert_eq!(
            cached_block_tags(&path, first_block),
            Some((scope, affinity_tag(Some("task-y")))),
            "a later task refreshes the task tag without changing scope"
        );
        cleanup(&path);
    }

    #[test]
    fn index_pages_are_persisted_in_cache_with_affinity() {
        let path = empty_indexed_store();
        for index in 0..100 {
            let memory = new_memory(
                "fact",
                "repo",
                &format!("subject {index}"),
                &format!("needle body {index}"),
                "audit",
                0.9,
                Vec::new(),
                None,
                None,
            );
            append_record(&path, T_MEMORY, &memory_payload(&memory), true).expect("append");
        }
        reindex_file(&path).expect("reindex");

        let mut store = Store::open(&path).expect("open");
        store.set_cache_affinity(Some("repo-index"), Some("task-index"));
        let hits = store
            .bm25("needle", None, None, None, None, 10)
            .expect("bm25");
        assert!(!hits.is_empty());

        let keys = cached_keys(&path);
        let index_key = keys
            .iter()
            .copied()
            .find(|key| key & 0x8000_0000 != 0)
            .expect("index page must enter the disk cache");
        let (_, scope, task) = cached_entry(&path, index_key).expect("cached index page");
        assert_eq!(scope, affinity_tag(Some("repo-index")));
        assert_eq!(task, affinity_tag(Some("task-index")));
        cleanup(&path);
    }

    #[test]
    fn corrupted_cache_slot_falls_back_to_store() {
        let path = empty_indexed_store();
        let memory = new_memory(
            "fact",
            "test",
            "cache checksum",
            "checksum falls back to the original block",
            "audit",
            0.9,
            Vec::new(),
            None,
            None,
        );
        append_record(&path, T_MEMORY, &memory_payload(&memory), true).expect("append");
        reindex_file(&path).expect("reindex");
        {
            let mut store = Store::open(&path).expect("open");
            assert_eq!(store.scan_page(usize::MAX, 0).expect("scan").len(), 1);
        }

        let (_, slot_offset, len) = cached_slot(&path, 0).expect("cached data block");
        let cache_path = path.with_file_name("cm.cache");
        let mut file = OpenOptions::new()
            .write(true)
            .open(&cache_path)
            .expect("open cache");
        file.seek(std::io::SeekFrom::Start(
            (slot_offset + 5.min(len - 1)) as u64,
        ))
        .expect("seek slot");
        file.write_all(&[0xFF]).expect("corrupt cache slot");
        file.sync_all().expect("sync corruption");

        let mut store = Store::open(&path).expect("open after corruption");
        let memories = store
            .scan_page(usize::MAX, 0)
            .expect("scan after corruption");
        assert_eq!(memories.len(), 1);
        assert_eq!(memories[0].text, memory.text);
        cleanup(&path);
    }
}

#[cfg(test)]
mod install_tests {
    use super::*;

    #[test]
    fn agent_paths_are_unique() {
        let mut ids = std::collections::HashSet::new();
        let mut paths = std::collections::HashSet::new();
        for spec in AGENTS {
            assert!(ids.insert(spec.id));
            assert!(paths.insert(spec.path));
        }
    }

    #[test]
    fn installs_and_removes_exe_in_temp_home() {
        let home = std::env::temp_dir().join(format!("cm-install-test-{}", crate::model::new_id()));
        fs::create_dir_all(&home).unwrap();
        let args = vec![
            "install".to_string(),
            "--home".to_string(),
            home.display().to_string(),
            "--agent".to_string(),
            "claude".to_string(),
            "--no-path".to_string(),
        ];
        cmd_install(&args, "test skill").unwrap();
        let exe = home
            .join(".local")
            .join("bin")
            .join(if cfg!(windows) { "cm.exe" } else { "cm" });
        assert!(exe.exists());
        let manifest = load_manifest(&home).unwrap();
        assert!(manifest.exe.is_some());
        cmd_uninstall(
            &[
                "uninstall".to_string(),
                "--home".to_string(),
                home.display().to_string(),
                "--all".to_string(),
            ],
            "test skill",
        )
        .unwrap();
        assert!(!exe.exists());
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn installs_and_removes_skill_in_temp_home() {
        let home = std::env::temp_dir().join(format!("cm-install-test-{}", crate::model::new_id()));
        fs::create_dir_all(&home).unwrap();
        let skill = "test skill";
        let args = vec![
            "install".to_string(),
            "--home".to_string(),
            home.display().to_string(),
            "--agent".to_string(),
            "claude".to_string(),
            "--no-exe".to_string(),
        ];
        cmd_install(&args, skill).unwrap();
        let path = skill_path(&home, AGENTS.iter().find(|s| s.id == "claude").unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), skill);
        cmd_uninstall(
            &[
                "uninstall".to_string(),
                "--home".to_string(),
                home.display().to_string(),
                "--agent".to_string(),
                "claude".to_string(),
            ],
            skill,
        )
        .unwrap();
        assert!(!path.exists());
        fs::remove_dir_all(&home).ok();
    }
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    fn temp_store_dir() -> PathBuf {
        let dir = env::temp_dir().join(format!("cm-doctor-test-{}", crate::model::new_id()));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn doctor_flags_truncated_tail_record() {
        let dir = temp_store_dir();
        let path = dir.join("cm");
        cmd_init(&path, true).expect("init");
        let input = AppendInput {
            kind: "fact".to_string(),
            scope: "test".to_string(),
            subject: "tail subject".to_string(),
            text: "tail body".to_string(),
            source: "audit".to_string(),
            confidence: 0.9,
            valid_from: 0,
            valid_to: None,
            supersedes: Vec::new(),
            conflict_of: None,
            trust: Some("verified".to_string()),
            source_commit: None,
        };
        cmd_append_input(&path, &input, true).expect("append");
        assert!(cmd_doctor(&path).expect("healthy doctor"));

        let len = fs::metadata(&path).expect("metadata").len();
        OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open for truncate")
            .set_len(len - 1)
            .expect("truncate");
        assert!(!cmd_doctor(&path).expect("doctor after truncate"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn doctor_flags_corrupt_index_with_warm_cache() {
        let dir = temp_store_dir();
        let path = dir.join("cm");
        cmd_init(&path, true).expect("init");
        assert!(cmd_doctor(&path).expect("healthy doctor"));

        let header = read_header_file(&path).expect("read header");
        assert!(header.index_len > 2);
        let mut bytes = fs::read(&path).expect("read store");
        bytes[header.index_off as usize + 1] ^= 0xFF;
        fs::write(&path, &bytes).expect("write corrupt store");

        assert!(!cmd_doctor(&path).expect("doctor after index corruption"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_get_and_forget_hide_tombstoned_index_records() {
        let dir = temp_store_dir();
        let path = dir.join("cm");
        cmd_init(&path, true).expect("init");
        let input = AppendInput {
            kind: "fact".to_string(),
            scope: "test".to_string(),
            subject: "forgotten subject".to_string(),
            text: "forgotten body".to_string(),
            source: "audit".to_string(),
            confidence: 0.9,
            valid_from: 0,
            valid_to: None,
            supersedes: Vec::new(),
            conflict_of: None,
            trust: Some("verified".to_string()),
            source_commit: None,
        };
        cmd_append_input(&path, &input, true).expect("append");
        cmd_reindex(&path).expect("reindex");
        let id = {
            let mut store = Store::open(&path).expect("open");
            store.scan_page(usize::MAX, 0).expect("scan")[0].id.clone()
        };
        cmd_forget(&path, &id).expect("forget");
        let mut store = Store::open(&path).expect("open after forget");
        assert!(store
            .scan_page(usize::MAX, 0)
            .expect("scan after forget")
            .is_empty());
        assert!(cmd_get(&path, &id).is_err());
        assert!(cmd_forget(&path, &id).is_err());
        fs::remove_dir_all(&dir).ok();
    }

    fn test_input(subject: &str, text: &str) -> AppendInput {
        AppendInput {
            kind: "fact".to_string(),
            scope: "test".to_string(),
            subject: subject.to_string(),
            text: text.to_string(),
            source: "audit".to_string(),
            confidence: 0.9,
            valid_from: 0,
            valid_to: None,
            supersedes: Vec::new(),
            conflict_of: None,
            trust: Some("verified".to_string()),
            source_commit: None,
        }
    }

    #[test]
    fn search_excludes_hidden_records_before_top_truncation() {
        let dir = temp_store_dir();
        let path = dir.join("cm");
        cmd_init(&path, true).expect("init");
        cmd_append_input(
            &path,
            &test_input("alpha alpha alpha", "alpha alpha alpha alpha"),
            true,
        )
        .expect("high score record");
        cmd_append_input(&path, &test_input("alpha", "alpha"), true).expect("low score record");
        cmd_reindex(&path).expect("reindex");
        let ids = {
            let mut store = Store::open(&path).expect("open");
            store
                .scan_page(usize::MAX, 0)
                .expect("scan")
                .into_iter()
                .map(|memory| memory.id)
                .collect::<Vec<_>>()
        };
        let before = search_out(&path, "alpha", 1, None, None, None, 0.0, None, false)
            .expect("search before forget");
        assert_eq!(before.candidates.len(), 1);
        let hidden_id = before.candidates[0].id.clone();
        let visible_id = ids
            .into_iter()
            .find(|id| id != &hidden_id)
            .expect("visible record");

        cmd_forget(&path, &hidden_id).expect("forget top hit");
        let after = search_out(&path, "alpha", 1, None, None, None, 0.0, None, false)
            .expect("search after forget");
        assert_eq!(
            after.candidates.len(),
            1,
            "a hidden top hit must not leave search empty"
        );
        assert_eq!(after.candidates[0].id, visible_id);
        fs::remove_dir_all(&dir).ok();
    }

    fn test_active_total(path: &Path) -> u64 {
        let mut store = Store::open(path).expect("open");
        let tail = store.tail_view().expect("tail");
        let mut active = store.active_count().expect("active count");
        for id in &tail.hidden {
            if let Some((idx, _)) = store.find_doc(id).expect("find hidden") {
                if store.is_active(idx).expect("active") {
                    active = active.saturating_sub(1);
                }
            }
        }
        active + tail.memories.len() as u64
    }

    #[test]
    fn checkpoint_masks_tombstoned_index_without_reindex() {
        let dir = temp_store_dir();
        let path = dir.join("cm");
        cmd_init(&path, true).expect("init");
        cmd_append_input(&path, &test_input("first subject", "first body"), true).expect("first");
        cmd_append_input(&path, &test_input("second subject", "second body"), true)
            .expect("second");
        cmd_reindex(&path).expect("reindex");
        let first_id = {
            let mut store = Store::open(&path).expect("open");
            store.scan_page(1, 0).expect("scan")[0].id.clone()
        };
        cmd_forget(&path, &first_id).expect("forget");
        cmd_append_input(&path, &test_input("tail subject", "tail body"), true).expect("tail");
        let before = {
            let mut store = Store::open(&path).expect("open");
            store
                .scan_page(usize::MAX, 0)
                .expect("before")
                .into_iter()
                .map(|memory| memory.id)
                .collect::<Vec<_>>()
        };
        cmd_checkpoint(&path, true).expect("checkpoint");
        let after = {
            let mut store = Store::open(&path).expect("open after checkpoint");
            store
                .scan_page(usize::MAX, 0)
                .expect("after")
                .into_iter()
                .map(|memory| memory.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(before, after);
        assert_eq!(test_active_total(&path), 2);
        cmd_verify(&path, true).expect("verify");
        assert!(cmd_doctor(&path).expect("doctor"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn remember_dedupes_tail_and_small_index() {
        let dir = temp_store_dir();
        let path = dir.join("cm");
        cmd_init(&path, true).expect("init");
        cmd_remember_input(
            &path,
            &test_input("dedup subject", "Dedup Body"),
            true,
            false,
        )
        .expect("first remember");
        let first_id = {
            let mut store = Store::open(&path).expect("open");
            store.scan_page(1, 0).expect("scan")[0].id.clone()
        };
        assert_eq!(test_active_total(&path), 1);
        cmd_remember_input(
            &path,
            &test_input("dedup subject", "dedup   body"),
            true,
            false,
        )
        .expect("tail duplicate");
        assert_eq!(test_active_total(&path), 1);
        cmd_reindex(&path).expect("reindex");
        cmd_remember_input(
            &path,
            &test_input("dedup subject", "dedup body"),
            true,
            false,
        )
        .expect("index duplicate");
        assert_eq!(test_active_total(&path), 1);
        let before_force = {
            let mut store = Store::open(&path).expect("open");
            store.scan_page(usize::MAX, 0).expect("scan").len()
        };
        cmd_remember_input(
            &path,
            &test_input("dedup subject", "dedup body"),
            true,
            true,
        )
        .expect("force duplicate");
        assert_eq!(test_active_total(&path), 2);
        assert_eq!(before_force, 1);
        assert_eq!(
            {
                let mut store = Store::open(&path).expect("open");
                store.scan_page(usize::MAX, 0).expect("scan")[0].id.clone()
            },
            first_id
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn doctor_conflict_warnings_are_capped() {
        let groups: Vec<ConflictGroupOut> = (0..DOCTOR_WARNING_CAP + 3)
            .map(|idx| ConflictGroupOut {
                scope: "test".to_string(),
                kind: "fact".to_string(),
                subject: format!("subject-{idx}"),
                records: Vec::new(),
            })
            .collect();
        let mut warnings = Vec::new();
        append_conflict_warnings(&mut warnings, &groups);
        assert_eq!(warnings.len(), DOCTOR_WARNING_CAP + 1);
        assert_eq!(
            warnings.last().map(String::as_str),
            Some("conflict: 3 more not shown")
        );
    }
}
