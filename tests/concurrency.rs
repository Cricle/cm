use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_dir(name: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("cm-concurrency-{name}-{stamp}"));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn assert_ok(output: &Output) {
    assert!(
        output.status.success(),
        "cm failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn search(file: &Path, query: &str, task: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cm"))
        .args([
            "search",
            "--file",
            file.to_str().unwrap(),
            "--query",
            query,
            "--scope",
            "repo",
            "--top",
            "20",
            "--task",
            task,
        ])
        .stdout(Stdio::null())
        .output()
        .expect("run concurrent search")
}

#[test]
fn concurrent_searches_keep_cache_consistent() {
    let dir = temp_dir("search");
    let file = dir.join("cm");
    let file_arg = file.to_str().unwrap();
    assert_ok(
        &Command::new(env!("CARGO_BIN_EXE_cm"))
            .args(["init", "--file", file_arg, "--empty"])
            .output()
            .expect("init"),
    );

    let mut import = Command::new(env!("CARGO_BIN_EXE_cm"))
        .args(["import", "--file", file_arg])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn import");
    {
        let stdin = import.stdin.as_mut().expect("import stdin");
        for index in 0..240 {
            let (subject, text) = if index % 2 == 0 {
                ("parser", "parser tokenizer grammar lexer syntax")
            } else {
                ("database", "database transaction index page lock")
            };
            writeln!(
                stdin,
                r#"{{"kind":"fact","scope":"repo","subject":"{subject} {index}","text":"{text} {index}","source":"bench","confidence":0.9,"trust":"authored"}}"#
            )
            .expect("write import line");
        }
    }
    assert_ok(&import.wait_with_output().expect("wait import"));

    let mut workers = Vec::new();
    for worker in 0..8 {
        let file = file.clone();
        workers.push(std::thread::spawn(move || {
            for round in 0..10 {
                let query = if (worker + round) % 2 == 0 {
                    "parser tokenizer"
                } else {
                    "database transaction"
                };
                let task = format!("task-{}", (worker + round) % 3);
                assert_ok(&search(&file, query, &task));
            }
        }));
    }
    for worker in workers {
        worker.join().expect("concurrent worker");
    }

    assert_ok(
        &Command::new(env!("CARGO_BIN_EXE_cm"))
            .args(["doctor", "--strict", "--file", file_arg])
            .output()
            .expect("doctor"),
    );
    let result = Command::new(env!("CARGO_BIN_EXE_cm"))
        .args([
            "search",
            "--file",
            file_arg,
            "--query",
            "parser tokenizer",
            "--scope",
            "repo",
            "--top",
            "5",
            "--task",
            "final",
        ])
        .output()
        .expect("final search");
    assert_ok(&result);
    assert!(String::from_utf8_lossy(&result.stdout).contains("\"candidates\""));

    let cache = fs::read(dir.join("cm.cache")).expect("read cache");
    assert_eq!(&cache[0..4], b"CMLU");
    assert_eq!(cache[5], 3, "cache layout");
    let table_off = u64::from_le_bytes(cache[24..32].try_into().unwrap()) as usize;
    let cap = u32::from_le_bytes(cache[32..36].try_into().unwrap()) as usize;
    let entries = (0..cap)
        .filter(|index| {
            let offset = table_off + index * 48;
            u32::from_le_bytes(cache[offset + 20..offset + 24].try_into().unwrap()) > 0
        })
        .count();
    assert!(entries > 0, "concurrent queries must leave a usable cache");
    fs::remove_dir_all(&dir).ok();
}
