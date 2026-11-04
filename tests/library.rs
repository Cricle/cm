use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use cm_memory::{Cm, NewMemory};

fn temp_dir(name: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("cm-lib-test-{name}-{stamp}"));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[test]
fn library_lifecycle_remember_get_search_and_dedupe() {
    let dir = temp_dir("lifecycle");
    let path = dir.join("cm");
    let cm = Cm::init(&path, true).expect("init");
    assert!(cm.all().expect("all").is_empty());

    let first = cm
        .remember(NewMemory::new(
            "decision",
            "repo",
            "build command",
            "Build with cargo build --release.",
        ))
        .expect("remember");
    assert!(!first.duplicate);
    assert_eq!(first.memory.kind, "decision");

    let fetched = cm.get(&first.memory.id).expect("get").expect("present");
    assert_eq!(fetched.subject, "build command");
    assert!(cm
        .search("cargo release", 5)
        .expect("search")
        .iter()
        .any(|memory| memory.id == first.memory.id));

    let duplicate = cm
        .remember(NewMemory::new(
            "decision",
            "repo",
            "build command",
            "Build with cargo build --release.",
        ))
        .expect("duplicate");
    assert!(duplicate.duplicate);
    assert_eq!(duplicate.memory.id, first.memory.id);
    assert_eq!(cm.all().expect("all after").len(), 1);

    fs::remove_dir_all(&dir).ok();
}
