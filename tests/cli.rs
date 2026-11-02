use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_dir(name: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("cm-cli-test-{name}-{stamp}"));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cm"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run cm")
}

fn assert_ok(output: &Output) {
    assert!(
        output.status.success(),
        "cm failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn cache_tags(cache: &Path) -> Vec<(u32, u32)> {
    let data = fs::read(cache).expect("read cm.cache");
    assert_eq!(&data[0..4], b"CMLU");
    assert_eq!(data[4], 1, "cache version stays 1");
    assert_eq!(data[5], 1, "affinity layout");
    let table_off = u64::from_le_bytes(data[24..32].try_into().unwrap()) as usize;
    let cap = u32::from_le_bytes(data[32..36].try_into().unwrap()) as usize;
    let mut tags = Vec::new();
    for index in 0..cap {
        let offset = table_off + index * 48;
        let len = u32::from_le_bytes(data[offset + 20..offset + 24].try_into().unwrap());
        if len > 0 {
            let scope = u32::from_le_bytes(data[offset + 24..offset + 28].try_into().unwrap());
            let task = u32::from_le_bytes(data[offset + 28..offset + 32].try_into().unwrap());
            tags.push((scope, task));
        }
    }
    tags
}

#[test]
fn task_flag_is_accepted_and_tags_the_cache_working_set() {
    let dir = temp_dir("task");
    let file = dir.join("cm");
    let file_arg = file.to_str().unwrap();

    assert_ok(&run(&dir, &["init", "--file", file_arg, "--empty"]));
    for (subject, text) in [
        (
            "parser entry",
            "Parser starts in src/parser.rs and uses recursive descent",
        ),
        (
            "build command",
            "Build with cargo build --release; tests use cargo test",
        ),
        (
            "storage format",
            "The cm file uses compressed 64 KiB blocks",
        ),
    ] {
        assert_ok(&run(
            &dir,
            &[
                "remember",
                "--file",
                file_arg,
                "--subject",
                subject,
                "--text",
                text,
                "--scope",
                "repo",
                "--source",
                "src/lib.rs",
                "--trust",
                "authored",
                "--durable",
            ],
        ));
    }
    assert_ok(&run(&dir, &["reindex", "--file", file_arg]));

    let plain = run(
        &dir,
        &[
            "recall",
            "--file",
            file_arg,
            "--query",
            "parser build",
            "--scope",
            "repo",
            "--budget",
            "800",
            "--format",
            "json",
        ],
    );
    assert_ok(&plain);

    let tagged = run(
        &dir,
        &[
            "recall",
            "--file",
            file_arg,
            "--query",
            "parser build",
            "--scope",
            "repo",
            "--task",
            "task-a",
            "--budget",
            "800",
            "--format",
            "json",
        ],
    );
    assert_ok(&tagged);
    assert_eq!(
        plain.stdout, tagged.stdout,
        "--task must only affect cache affinity, not ranking"
    );

    let tags = cache_tags(&file.with_file_name("cm.cache"));
    assert!(
        tags.iter().any(|(scope, task)| *scope != 0 && *task != 0),
        "query must tag cached blocks with scope and task: {tags:?}"
    );
    let stats = run(&dir, &["cache-stats", "--file", file_arg]);
    assert_ok(&stats);
    let stats: serde_json::Value = serde_json::from_slice(&stats.stdout).expect("cache stats json");
    assert!(stats["entries"].as_u64().unwrap_or(0) > 0);
    assert!(
        stats["hits"].as_u64().unwrap_or(0) + stats["misses"].as_u64().unwrap_or(0) > 0,
        "cache stats must count accesses: {stats}"
    );

    assert_ok(&run(
        &dir,
        &[
            "search", "--file", file_arg, "--query", "parser", "--scope", "repo", "--task",
            "task-b",
        ],
    ));
    assert_ok(&run(
        &dir,
        &[
            "context", "--file", file_arg, "--query", "parser", "--scope", "repo", "--task",
            "task-b", "--format", "json",
        ],
    ));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn feedback_governs_negative_memories() {
    let dir = temp_dir("govern");
    let file = dir.join("cm");
    let file_arg = file.to_str().unwrap();
    assert_ok(&run(&dir, &["init", "--file", file_arg, "--empty"]));
    let remember = run(
        &dir,
        &[
            "remember",
            "--file",
            file_arg,
            "--subject",
            "bad",
            "--text",
            "bad fact",
            "--scope",
            "repo",
            "--source",
            "README",
        ],
    );
    assert_ok(&remember);
    let id = serde_json::from_slice::<serde_json::Value>(&remember.stdout).expect("remember json")
        ["id"]
        .as_str()
        .unwrap()
        .to_string();
    for _ in 0..2 {
        assert_ok(&run(
            &dir,
            &[
                "feedback",
                "--file",
                file_arg,
                "--id",
                &id,
                "--signal",
                "wrong",
                "--note",
                "incorrect",
            ],
        ));
    }
    let govern = run(&dir, &["govern", "--file", file_arg]);
    assert_ok(&govern);
    let report: serde_json::Value = serde_json::from_slice(&govern.stdout).expect("govern json");
    assert_eq!(report["suggestions"][0]["action"], "forget");
    assert_ok(&run(&dir, &["govern", "--file", file_arg, "--apply"]));
    assert!(!run(&dir, &["get", "--file", file_arg, "--id", &id])
        .status
        .success());
    assert_ok(&run(&dir, &["doctor", "--strict", "--file", file_arg]));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn task_frame_roundtrip_and_reindex() {
    let dir = temp_dir("frame");
    let file = dir.join("cm");
    let file_arg = file.to_str().unwrap();
    assert_ok(&run(&dir, &["init", "--file", file_arg, "--empty"]));
    assert_ok(&run(
        &dir,
        &[
            "frame",
            "set",
            "--file",
            file_arg,
            "--task",
            "T1",
            "--goal",
            "Make cache fast",
            "--constraint",
            "keep 8MiB",
            "--next",
            "benchmark 1M",
            "--touch",
            "src/cache.rs",
        ],
    ));
    assert_ok(&run(
        &dir,
        &[
            "remember",
            "--file",
            file_arg,
            "--subject",
            "cache",
            "--text",
            "Make cache fast with an 8MiB working set",
            "--scope",
            "repo",
            "--source",
            "README",
        ],
    ));
    let frame_recall = run(&dir, &["recall", "--file", file_arg, "--task", "T1"]);
    assert_ok(&frame_recall);
    assert!(
        String::from_utf8_lossy(&frame_recall.stdout).contains("cache"),
        "recall without --query should use the frame"
    );
    assert!(
        !String::from_utf8_lossy(&frame_recall.stdout).contains("\tframe\t"),
        "ordinary recall must not return the frame record itself"
    );
    let get = run(&dir, &["frame", "get", "--file", file_arg, "--task", "T1"]);
    assert_ok(&get);
    let frame: serde_json::Value = serde_json::from_slice(&get.stdout).expect("frame json");
    assert_eq!(frame["goal"], "Make cache fast");
    assert_eq!(frame["status"], "active");
    assert_eq!(frame["constraints"][0], "keep 8MiB");

    assert_ok(&run(
        &dir,
        &[
            "frame",
            "set",
            "--file",
            file_arg,
            "--task",
            "T1",
            "--status",
            "paused",
            "--next",
            "check index pages",
        ],
    ));
    let get = run(&dir, &["frame", "get", "--file", file_arg, "--task", "T1"]);
    assert_ok(&get);
    let frame: serde_json::Value = serde_json::from_slice(&get.stdout).expect("updated frame");
    assert_eq!(frame["status"], "paused");
    assert_eq!(frame["goal"], "Make cache fast");
    assert_eq!(frame["next"].as_array().unwrap().len(), 2);

    assert_ok(&run(&dir, &["reindex", "--file", file_arg]));
    let get = run(&dir, &["frame", "get", "--file", file_arg, "--task", "T1"]);
    assert_ok(&get);
    let frame: serde_json::Value = serde_json::from_slice(&get.stdout).expect("indexed frame");
    assert_eq!(frame["status"], "paused");
    let list = run(&dir, &["frame", "list", "--file", file_arg]);
    assert_ok(&list);
    let frames: serde_json::Value = serde_json::from_slice(&list.stdout).expect("frame list");
    assert_eq!(frames.as_array().unwrap().len(), 1);
    assert_ok(&run(&dir, &["doctor", "--strict", "--file", file_arg]));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn help_mentions_task_affinity() {
    let dir = temp_dir("help");
    let output = run(&dir, &["--help"]);
    assert_ok(&output);
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("--task"), "{text}");
    fs::remove_dir_all(&dir).ok();
}
