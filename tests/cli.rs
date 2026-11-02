use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
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
fn governance_removes_exact_duplicates_and_search_matches_sources() {
    let dir = temp_dir("duplicates");
    let file = dir.join("cm");
    let file_arg = file.to_str().unwrap();
    assert_ok(&run(&dir, &["init", "--file", file_arg, "--empty"]));
    let record = r#"{"kind":"fact","scope":"repo","subject":"parser","text":"same body","source":"src/parser.rs","confidence":0.9,"trust":"authored"}"#;
    assert_ok(&run(
        &dir,
        &["append", "--file", file_arg, "--json", record],
    ));
    assert_ok(&run(
        &dir,
        &["append", "--file", file_arg, "--json", record],
    ));
    let search = run(
        &dir,
        &[
            "search",
            "--file",
            file_arg,
            "--query",
            "parser.rs",
            "--top",
            "5",
            "--scope",
            "repo",
        ],
    );
    assert_ok(&search);
    let hits: serde_json::Value = serde_json::from_slice(&search.stdout).expect("search json");
    assert!(!hits["candidates"].as_array().unwrap().is_empty());
    let govern = run(&dir, &["govern", "--file", file_arg]);
    assert_ok(&govern);
    let report: serde_json::Value = serde_json::from_slice(&govern.stdout).expect("govern json");
    assert_eq!(report["suggestions"][0]["action"], "forget");
    assert_ok(&run(&dir, &["govern", "--file", file_arg, "--apply"]));
    let scan = run(&dir, &["scan", "--file", file_arg]);
    assert_ok(&scan);
    assert_eq!(String::from_utf8_lossy(&scan.stdout).lines().count(), 1);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn audit_detects_source_drift_and_feeds_governance() {
    let dir = temp_dir("audit");
    let file = dir.join("cm");
    let file_arg = file.to_str().unwrap();
    let root = dir.join("root");
    fs::create_dir_all(root.join("src")).expect("create source tree");
    let source = root.join("src").join("foo.rs");
    fs::write(&source, "fn hello() {}\n").expect("write source");
    assert_ok(&run(&dir, &["init", "--file", file_arg, "--empty"]));
    assert_ok(&run(
        &dir,
        &[
            "remember",
            "--file",
            file_arg,
            "--subject",
            "foo",
            "--text",
            "hello function",
            "--scope",
            "repo",
            "--source",
            "src/foo.rs:hello",
        ],
    ));
    let root_arg = root.to_str().unwrap();
    let clean = run(&dir, &["audit", "--file", file_arg, "--root", root_arg]);
    assert_ok(&clean);
    let report: serde_json::Value = serde_json::from_slice(&clean.stdout).expect("audit json");
    assert_eq!(report["issue_count"], 0);
    fs::remove_file(&source).expect("remove source");
    let dirty = run(
        &dir,
        &["audit", "--file", file_arg, "--root", root_arg, "--apply"],
    );
    assert_ok(&dirty);
    let report: serde_json::Value = serde_json::from_slice(&dirty.stdout).expect("audit json");
    assert_eq!(report["issue_count"], 1);
    let govern = run(&dir, &["govern", "--file", file_arg, "--stale-days", "0"]);
    assert_ok(&govern);
    let report: serde_json::Value = serde_json::from_slice(&govern.stdout).expect("govern json");
    assert_eq!(report["suggestions"][0]["action"], "verify");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn audit_detects_source_hash_drift() {
    let dir = temp_dir("anchor");
    let file = dir.join("cm");
    let file_arg = file.to_str().unwrap();
    fs::create_dir_all(dir.join("src")).expect("create source tree");
    let source = dir.join("src").join("foo.rs");
    fs::write(&source, "pub fn hello() {\n    println!(\"hi\");\n}\n").expect("write source");
    assert_ok(&run(&dir, &["init", "--file", file_arg, "--empty"]));
    assert_ok(&run(
        &dir,
        &[
            "remember",
            "--file",
            file_arg,
            "--subject",
            "hello",
            "--text",
            "hello prints hi",
            "--scope",
            "repo",
            "--source",
            "src/foo.rs:hello",
        ],
    ));
    let root_arg = dir.to_str().unwrap();
    let clean = run(&dir, &["audit", "--file", file_arg, "--root", root_arg]);
    assert_ok(&clean);
    let report: serde_json::Value = serde_json::from_slice(&clean.stdout).expect("audit json");
    assert_eq!(report["issue_count"], 0);

    fs::write(&source, "pub fn hello() {\n    println!(\"bye\");\n}\n").expect("edit source");
    let dirty = run(&dir, &["audit", "--file", file_arg, "--root", root_arg]);
    assert_ok(&dirty);
    let report: serde_json::Value = serde_json::from_slice(&dirty.stdout).expect("audit json");
    assert_eq!(report["issue_count"], 1);
    assert_eq!(report["issues"][0]["issue"], "source_changed");
    assert_eq!(report["issues"][0]["source_line"], 1);
    let refreshed = run(
        &dir,
        &["audit", "--file", file_arg, "--root", root_arg, "--refresh"],
    );
    assert_ok(&refreshed);
    let report: serde_json::Value = serde_json::from_slice(&refreshed.stdout).expect("audit json");
    assert_eq!(report["refreshed"].as_array().unwrap().len(), 1);
    let clean = run(&dir, &["audit", "--file", file_arg, "--root", root_arg]);
    assert_ok(&clean);
    let report: serde_json::Value = serde_json::from_slice(&clean.stdout).expect("audit json");
    assert_eq!(report["issue_count"], 0);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn claim_and_release_guard_a_task_frame() {
    let dir = temp_dir("claim");
    let file = dir.join("cm");
    let file_arg = file.to_str().unwrap();
    assert_ok(&run(&dir, &["init", "--file", file_arg, "--empty"]));
    assert_ok(&run(
        &dir,
        &[
            "frame", "set", "--file", file_arg, "--task", "T1", "--goal", "Ship it",
        ],
    ));
    let first = run(
        &dir,
        &[
            "claim", "--file", file_arg, "--task", "T1", "--owner", "agent-a", "--lease", "30",
        ],
    );
    assert_ok(&first);
    let blocked = run(
        &dir,
        &[
            "claim", "--file", file_arg, "--task", "T1", "--owner", "agent-b", "--lease", "30",
        ],
    );
    assert!(!blocked.status.success(), "second owner must be rejected");
    let blocked_update = run(
        &dir,
        &[
            "frame", "set", "--file", file_arg, "--task", "T1", "--note", "stolen",
        ],
    );
    assert!(
        !blocked_update.status.success(),
        "frame update must respect a live lease"
    );
    let owned_update = run(
        &dir,
        &[
            "frame",
            "set",
            "--file",
            file_arg,
            "--task",
            "T1",
            "--owner",
            "agent-a",
            "--note",
            "owned update",
        ],
    );
    assert_ok(&owned_update);
    let get = run(&dir, &["frame", "get", "--file", file_arg, "--task", "T1"]);
    assert_ok(&get);
    let frame: serde_json::Value = serde_json::from_slice(&get.stdout).expect("frame json");
    assert_eq!(frame["owner"], "agent-a");
    assert!(frame["lease_until"].as_u64().unwrap() > 0);

    assert_ok(&run(
        &dir,
        &[
            "release", "--file", file_arg, "--task", "T1", "--owner", "agent-a",
        ],
    ));
    let second = run(
        &dir,
        &[
            "claim", "--file", file_arg, "--task", "T1", "--owner", "agent-b", "--lease", "30",
        ],
    );
    assert_ok(&second);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn concurrent_claims_admit_exactly_one_owner() {
    let dir = temp_dir("claim-race");
    let file = dir.join("cm");
    let file_arg = file.to_str().unwrap();
    assert_ok(&run(&dir, &["init", "--file", file_arg, "--empty"]));
    assert_ok(&run(
        &dir,
        &[
            "frame", "set", "--file", file_arg, "--task", "T1", "--goal", "Race",
        ],
    ));
    let mut children = Vec::new();
    for owner in ["agent-a", "agent-b", "agent-c", "agent-d"] {
        children.push(
            Command::new(env!("CARGO_BIN_EXE_cm"))
                .current_dir(&dir)
                .args([
                    "claim", "--file", file_arg, "--task", "T1", "--owner", owner, "--lease", "30",
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn claim"),
        );
    }
    let winners = children
        .into_iter()
        .map(|mut child| child.wait().expect("wait claim").success())
        .filter(|success| *success)
        .count();
    assert_eq!(winners, 1, "concurrent claims must admit exactly one owner");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn finish_closes_frame_writes_summary_and_governs() {
    let dir = temp_dir("finish");
    let file = dir.join("cm");
    let file_arg = file.to_str().unwrap();
    fs::create_dir_all(dir.join("src")).expect("create source tree");
    fs::write(dir.join("src").join("lib.rs"), "pub fn done() {}\n").expect("write source");
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
            "Finish the task",
            "--next",
            "write summary",
        ],
    ));
    assert_ok(&run(
        &dir,
        &[
            "remember",
            "--file",
            file_arg,
            "--subject",
            "done function",
            "--text",
            "The done function marks completion",
            "--scope",
            "repo",
            "--source",
            "src/lib.rs:done",
        ],
    ));
    let root_arg = dir.to_str().unwrap();
    let finish = run(
        &dir,
        &[
            "finish",
            "--file",
            file_arg,
            "--task",
            "T1",
            "--summary",
            "Delivered the task",
            "--root",
            root_arg,
            "--apply",
        ],
    );
    assert_ok(&finish);
    let report: serde_json::Value = serde_json::from_slice(&finish.stdout).expect("finish json");
    assert_eq!(report["status"], "done");
    assert!(report["summary_id"].as_str().is_some());
    assert_eq!(report["audit"]["issue_count"], 0);
    assert_eq!(report["govern"]["ok"], true);

    let get = run(&dir, &["frame", "get", "--file", file_arg, "--task", "T1"]);
    assert_ok(&get);
    let frame: serde_json::Value = serde_json::from_slice(&get.stdout).expect("frame json");
    assert_eq!(frame["status"], "done");
    assert!(frame["owner"].is_null());
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn governance_merges_near_duplicates() {
    let dir = temp_dir("merge");
    let file = dir.join("cm");
    let file_arg = file.to_str().unwrap();
    assert_ok(&run(&dir, &["init", "--file", file_arg, "--empty"]));
    for text in [
        "Cache policy uses LRU and frequency for blocks",
        "Cache policy uses LRU plus frequency for blocks",
    ] {
        assert_ok(&run(
            &dir,
            &[
                "remember",
                "--file",
                file_arg,
                "--subject",
                "cache policy",
                "--text",
                text,
                "--scope",
                "repo",
                "--source",
                "src/cache.rs",
            ],
        ));
    }
    assert_ok(&run(&dir, &["reindex", "--file", file_arg]));
    let govern = run(
        &dir,
        &[
            "govern",
            "--file",
            file_arg,
            "--merge",
            "--merge-threshold",
            "0.5",
            "--apply",
        ],
    );
    assert_ok(&govern);
    let report: serde_json::Value = serde_json::from_slice(&govern.stdout).expect("govern json");
    assert_eq!(report["merged_groups"], 1);
    let scan = run(&dir, &["scan", "--file", file_arg]);
    assert_ok(&scan);
    assert_eq!(String::from_utf8_lossy(&scan.stdout).lines().count(), 1);
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
fn brief_resumes_current_frame_with_budgeted_context() {
    let dir = temp_dir("brief");
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
            "parser",
            "--goal",
            "Fix parser recovery",
            "--constraint",
            "keep format version 1",
            "--next",
            "add resync test",
            "--open",
            "error span",
        ],
    ));
    assert_ok(&run(
        &dir,
        &[
            "remember",
            "--file",
            file_arg,
            "--kind",
            "decision",
            "--subject",
            "parser strategy",
            "--text",
            "Recursive descent parser uses token resync at statement boundaries",
            "--scope",
            "repo",
            "--source",
            "src/parser.rs:parse_stmt",
        ],
    ));
    assert_ok(&run(
        &dir,
        &[
            "remember",
            "--file",
            file_arg,
            "--subject",
            "cache budget",
            "--text",
            "Cache stays at 8 MiB",
            "--scope",
            "repo",
            "--source",
            "src/cache.rs",
        ],
    ));
    let brief = run(
        &dir,
        &[
            "brief", "--file", file_arg, "--task", "current", "--budget", "300",
        ],
    );
    assert_ok(&brief);
    let brief: serde_json::Value = serde_json::from_slice(&brief.stdout).expect("brief json");
    assert_eq!(brief["frame"]["task"], "parser");
    assert_eq!(brief["frame"]["goal"], "Fix parser recovery");
    assert_eq!(brief["frame"]["constraints"][0], "keep format version 1");
    assert!(brief["query"].as_str().unwrap().contains("resync"));
    let memories = brief["memories"].as_array().unwrap();
    assert!(!memories.is_empty(), "brief should retrieve task memories");
    assert!(memories[0]["text"].as_str().unwrap().contains("resync"));
    assert!(
        brief["estimated_tokens"].as_u64().unwrap() <= 300,
        "brief must respect its budget: {brief}"
    );
    let prompt = run(
        &dir,
        &[
            "brief", "--file", file_arg, "--task", "current", "--budget", "300", "--format",
            "prompt",
        ],
    );
    assert_ok(&prompt);
    let prompt = String::from_utf8_lossy(&prompt.stdout);
    assert!(prompt.contains("GOAL: Fix parser recovery"), "{prompt}");
    assert!(
        prompt.contains("source: src/parser.rs:parse_stmt"),
        "{prompt}"
    );

    let fallback = run(
        &dir,
        &[
            "brief", "--file", file_arg, "--task", "missing", "--query", "parser", "--budget",
            "300",
        ],
    );
    assert_ok(&fallback);
    let fallback: serde_json::Value =
        serde_json::from_slice(&fallback.stdout).expect("fallback brief json");
    assert!(fallback["frame"].is_null());
    assert!(!fallback["memories"].as_array().unwrap().is_empty());
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
