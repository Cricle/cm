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
    assert_eq!(data[5], 3, "affinity layout");
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
fn help_mentions_task_affinity() {
    let dir = temp_dir("help");
    let output = run(&dir, &["--help"]);
    assert_ok(&output);
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("--task"), "{text}");
    fs::remove_dir_all(&dir).ok();
}
