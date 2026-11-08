use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cm_memory::{
    AbsorbOptions, Cm, ExploreCursor, ExploreMode, ExploreOptions, FeedbackVerdict, NewMemory,
    RecallQuery, StoreOptions, TaskFrame,
};

fn temp_dir(name: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("cm-api-test-{name}-{stamp}"));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn frame(task: &str, goal: &str) -> TaskFrame {
    TaskFrame {
        task: task.to_string(),
        goal: goal.to_string(),
        status: "active".to_string(),
        constraints: Vec::new(),
        next: vec!["answer with exact citations".to_string()],
        open: Vec::new(),
        touches: Vec::new(),
        notes: Vec::new(),
        owner: None,
        lease_until: None,
        updated_at: 0,
    }
}

#[test]
fn recall_respects_budget_max_items_expiry_and_snippets() {
    let dir = temp_dir("recall");
    let path = dir.join("cm");
    Cm::init(&path, true).expect("init");
    let source_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let cm = Cm::open_with(
        &path,
        StoreOptions {
            source_policy: cm_memory::SourcePolicy {
                root: source_root,
                strict: false,
            },
            ..StoreOptions::default()
        },
    )
    .expect("open with source root");

    let parser = cm
        .remember(
            NewMemory::new(
                "fact",
                "repo",
                "parser",
                "The parser splits expressions into tokens before evaluation.",
            )
            .source("src/frames.rs:TaskFrame"),
        )
        .expect("remember parser");
    cm.remember(
        NewMemory::new(
            "fact",
            "repo",
            "parser tests",
            "Parser tests cover tokenizer and expression evaluation.",
        )
        .source("agent:test"),
    )
    .expect("remember tests");
    cm.remember(
        NewMemory::new("fact", "repo", "old parser", "Obsolete parser notes.")
            .source("agent:test")
            .valid_to(1),
    )
    .expect("remember expired");

    let response = cm
        .recall(&RecallQuery {
            query: "parser tokenizer expression".to_string(),
            budget_tokens: 500,
            top: 8,
            max_items: 8,
            min_trust: Some("imported".to_string()),
            snippet_lines: 2,
            ..RecallQuery::default()
        })
        .expect("recall");
    assert!(response.memories.len() <= 8);
    assert!(response.estimated_tokens > 0);
    let parser_hit = response
        .memories
        .iter()
        .find(|hit| hit.memory.id == parser.memory.id)
        .expect("parser hit");
    let snippet = parser_hit.snippet.as_deref().unwrap_or_default();
    assert!(
        snippet.contains("pub struct TaskFrame"),
        "snippet: {snippet}"
    );

    let expired = cm
        .recall(&RecallQuery {
            query: "obsolete parser".to_string(),
            max_items: 8,
            ..RecallQuery::default()
        })
        .expect("recall expired");
    assert!(!expired
        .memories
        .iter()
        .any(|hit| hit.memory.subject == "old parser"));
}

#[test]
fn brief_uses_frame_feedback_and_budget() {
    let dir = temp_dir("brief");
    let path = dir.join("cm");
    let cm = Cm::init(&path, true).expect("init");
    cm.frame_set(&frame("demo", "explain the parser"))
        .expect("frame set");
    let memory = cm
        .remember(NewMemory::new(
            "fact",
            "repo",
            "parser",
            "The parser builds a token stream before evaluation.",
        ))
        .expect("remember")
        .memory;
    let response = cm.brief_for("demo", 1200).expect("brief");
    assert_eq!(response.frame.as_ref().expect("frame").task, "demo");
    assert!(response.prompt.contains("TASK: demo"));
    assert!(response.prompt.contains("QUERY:"));
    assert!(response
        .memories
        .iter()
        .any(|hit| hit.memory.id == memory.id));

    cm.feedback(&memory.id, FeedbackVerdict::Wrong, Some("incorrect"))
        .expect("first feedback");
    cm.feedback(&memory.id, FeedbackVerdict::Wrong, Some("still incorrect"))
        .expect("second feedback");
    let suppressed = cm.brief_for("demo", 1200).expect("brief after feedback");
    assert!(!suppressed
        .memories
        .iter()
        .any(|hit| hit.memory.id == memory.id));

    let tiny = cm.brief_with_query(None, "parser", 1).expect("tiny brief");
    assert!(tiny.memories.is_empty());
}

#[test]
fn frames_claim_release_and_finish() {
    let dir = temp_dir("frames");
    let path = dir.join("cm");
    let cm = Cm::init(&path, true).expect("init");
    cm.frame_set(&frame("t1", "first goal")).expect("set");
    assert_eq!(
        cm.frame_get("t1").expect("get").expect("present").goal,
        "first goal"
    );
    assert_eq!(cm.frames().expect("list").len(), 1);

    cm.claim("t1", "alice", Duration::from_secs(30))
        .expect("claim alice");
    let error = cm
        .claim("t1", "bob", Duration::from_secs(30))
        .expect_err("bob must be rejected");
    assert!(format!("{error:#}").contains("held by alice"));

    cm.release("t1", Some("alice")).expect("release");
    cm.claim("t1", "bob", Duration::from_secs(30))
        .expect("claim bob");
    let finished = cm
        .finish("t1", Some("done in library test"), Some("bob"))
        .expect("finish");
    assert_eq!(finished.frame.status, "done");
    assert_eq!(finished.frame.owner, None);
    let summary = cm.get(&finished.summary_id).expect("get summary");
    assert!(summary.is_some());
    assert!(cm
        .all()
        .expect("all")
        .iter()
        .any(|memory| memory.subject == "task:t1 summary"));
}

#[test]
fn absorb_merge_forget_and_export_import() {
    let dir = temp_dir("mutate");
    let path = dir.join("cm");
    let cm = Cm::init(&path, true).expect("init");
    let evidence = cm
        .remember(NewMemory::new(
            "fact",
            "repo",
            "evidence",
            "Evidence record.",
        ))
        .expect("evidence")
        .memory;
    let content = format!(
        "{{\"op\":\"remember\",\"kind\":\"fact\",\"scope\":\"repo\",\"subject\":\"absorbed\",\"text\":\"Absorbed fact from JSONL.\",\"source\":\"agent:test\",\"evidence\":[\"{}\"]}}\n",
        evidence.id
    );
    let report = cm
        .absorb(&content, &AbsorbOptions::default())
        .expect("absorb");
    assert_eq!(report.written, 1);
    let absorbed = cm
        .all()
        .expect("all")
        .into_iter()
        .find(|memory| memory.subject == "absorbed")
        .expect("absorbed memory");
    let other = cm
        .remember(NewMemory::new("fact", "repo", "other", "Another fact."))
        .expect("other")
        .memory;
    let merged = cm
        .merge(
            &[absorbed.id.clone(), other.id.clone()],
            Some("Merged fact."),
        )
        .expect("merge");
    assert!(merged.merged);
    assert!(cm.get(&absorbed.id).expect("get absorbed").is_none());

    let mut exported = Vec::new();
    cm.export_jsonl(&mut exported).expect("export");
    let imported_path = dir.join("imported.cm");
    let imported = Cm::init(&imported_path, true).expect("init imported");
    let report = imported
        .import_jsonl(Cursor::new(exported))
        .expect("import");
    assert_eq!(report.written, cm.all().expect("source all").len());
    assert_eq!(
        imported.all().expect("imported all").len(),
        cm.all().expect("source all").len()
    );

    cm.forget(&merged.id, Some("test cleanup")).expect("forget");
    assert!(cm.get(&merged.id).expect("get forgotten").is_none());
}

#[test]
fn strict_source_policy_rejects_escape_and_read_only_blocks_writes() {
    let dir = temp_dir("security");
    let root = dir.join("project");
    fs::create_dir_all(root.join("src")).expect("project");
    fs::write(root.join("src/ok.rs"), "pub fn ok() {}\n").expect("source");
    let path = root.join("cm");
    Cm::init(&path, true).expect("init");
    let cm = Cm::open_with(
        &path,
        StoreOptions {
            source_policy: cm_memory::SourcePolicy {
                root: root.clone(),
                strict: true,
            },
            ..StoreOptions::default()
        },
    )
    .expect("open");

    let absolute = dir.join("outside.rs").to_string_lossy().to_string();
    for source in [absolute, "../outside.rs".to_string()] {
        let error = cm
            .remember(NewMemory::new("fact", "repo", "bad", "Bad source.").source(source))
            .expect_err("escape must be rejected");
        assert!(format!("{error:#}").contains("source"));
    }
    cm.remember(NewMemory::new("fact", "repo", "good", "Good source.").source("src/ok.rs:ok"))
        .expect("relative source");

    let read_only = Cm::open_with(
        &path,
        StoreOptions {
            read_only: true,
            ..StoreOptions::default()
        },
    )
    .expect("read only open");
    assert!(read_only.status().expect("status").active_count >= 1);
    let error = read_only
        .remember(NewMemory::new("fact", "repo", "bad", "No writes."))
        .expect_err("read only must reject writes");
    assert!(format!("{error:#}").contains("read-only"));
}

#[test]
fn concurrent_remember_keeps_one_active_record() {
    let dir = temp_dir("concurrent");
    let path = dir.join("cm");
    let cm = Cm::init(&path, true).expect("init");
    let mut handles = Vec::new();
    for _ in 0..16 {
        let cm = cm.clone();
        handles.push(std::thread::spawn(move || {
            cm.remember(NewMemory::new(
                "fact",
                "repo",
                "shared subject",
                "Only one active copy should remain.",
            ))
        }));
    }
    for handle in handles {
        handle.join().expect("join").expect("remember");
    }
    let active = cm.all().expect("all");
    assert_eq!(active.len(), 1);
}

#[test]
fn explore_plan_switches_modes_and_cursor_round_trips() {
    let dir = temp_dir("explore");
    let root = dir.join("repo");
    fs::create_dir_all(root.join("src")).expect("repo");
    fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n").expect("manifest");
    fs::write(root.join("src/main.rs"), "fn main() {}\n").expect("main");
    fs::write(root.join("src/lib.rs"), "pub fn run() {}\n").expect("lib");
    let path = dir.join("cm");
    let cm = Cm::init(&path, true).expect("init");
    let plan = cm
        .explore_plan(&ExploreOptions {
            root: root.clone(),
            mode: ExploreMode::Auto,
            ..ExploreOptions::default()
        })
        .expect("explore plan");
    assert_eq!(plan.mode, "macro");
    assert!(!plan.targets.is_empty());
    assert!(plan.prompt.contains("MAX_RECORDS:"));
    assert!(plan.targets.len() <= 28);

    let micro = cm
        .explore_plan(&ExploreOptions {
            root: root.clone(),
            mode: ExploreMode::Micro,
            changed: Some(vec!["src/main.rs".to_string()]),
            max_targets: 5,
            ..ExploreOptions::default()
        })
        .expect("micro plan");
    assert_eq!(micro.mode, "micro");
    assert!(micro.targets.len() <= 5);

    let cursor = ExploreCursor {
        git_head: "deadbeef".to_string(),
        built_at: 1,
        mode: "macro".to_string(),
        memories: 3,
    };
    cm.set_explore_cursor(&cursor).expect("set cursor");
    let loaded = cm.explore_cursor().expect("get cursor").expect("cursor");
    assert_eq!(loaded.git_head, "deadbeef");
    assert_eq!(loaded.memories, 3);
}

#[test]
fn lock_timeout_reports_busy_instead_of_hanging() {
    use fs2::FileExt;
    use std::fs::OpenOptions;

    let dir = temp_dir("lock");
    let path = dir.join("cm");
    Cm::init(&path, true).expect("init");
    let lock_path = Path::new(&path).with_file_name("cm.lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .expect("open lock");
    lock.lock_exclusive().expect("hold lock");
    let cm = Cm::open_with(
        &path,
        StoreOptions {
            lock_timeout: Some(Duration::from_millis(50)),
            ..StoreOptions::default()
        },
    )
    .expect("open");
    let error = cm
        .remember(NewMemory::new("fact", "repo", "busy", "Should time out."))
        .expect_err("busy store");
    assert!(format!("{error:#}").contains("store busy"));
    lock.unlock().expect("unlock");
    cm.remember(NewMemory::new(
        "fact",
        "repo",
        "busy",
        "Works after unlock.",
    ))
    .expect("remember after unlock");
}

#[test]
fn maintenance_reports_store_health() {
    let dir = temp_dir("maint");
    let path = dir.join("cm");
    let cm = Cm::init(&path, true).expect("init");
    cm.remember(NewMemory::new(
        "fact",
        "repo",
        "maint",
        "Maintenance record.",
    ))
    .expect("remember");
    let checkpoint = cm.checkpoint(true).expect("checkpoint");
    assert!(checkpoint.bytes_after >= checkpoint.bytes_before);
    let reindex = cm.reindex().expect("reindex");
    assert_eq!(reindex.documents, 1);
    let report = cm.verify(false).expect("verify");
    assert!(report.ok);
    assert!(report.records >= 1);
    let status = cm.status().expect("status");
    assert_eq!(status.active_count, 1);
    assert!(status.bytes > 0);
}

#[test]
fn mechanical_explore_is_deterministic_and_zero_llm() {
    let dir = temp_dir("mechanical");
    let root = dir.join("repo");
    fs::create_dir_all(root.join("src")).expect("src");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"mechanical-demo\"\n",
    )
    .expect("manifest");
    fs::write(root.join("src/main.rs"), "fn main() {}\n").expect("main");
    let path = dir.join("cm");
    let cm = Cm::init(&path, true).expect("init");
    let plan = cm
        .mechanical_explore(
            &ExploreOptions {
                root: root.clone(),
                ..ExploreOptions::default()
            },
            true,
        )
        .expect("mechanical explore");
    assert_eq!(plan.mode, "mechanical");
    assert!(plan.records >= 2);
    let report = plan.report.expect("report");
    assert_eq!(report.written, plan.records);
    let memories = cm.all().expect("all");
    assert!(memories
        .iter()
        .any(|memory| memory.subject == "repo layout"));
    assert!(memories
        .iter()
        .any(|memory| memory.subject == "build and test commands"));
    assert!(memories
        .iter()
        .any(|memory| memory.subject.starts_with("area: ")));
    let second = cm
        .mechanical_explore(
            &ExploreOptions {
                root,
                ..ExploreOptions::default()
            },
            true,
        )
        .expect("second mechanical explore");
    assert_eq!(second.report.expect("report").written, second.records);
    assert_eq!(cm.all().expect("all after").len(), plan.records + 1);
}
