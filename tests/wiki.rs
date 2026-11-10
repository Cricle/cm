use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use cm_memory::wiki;

fn temp_dir(name: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("cm-wiki-test-{name}-{stamp}"));
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn demo_repo(name: &str) -> PathBuf {
    let dir = temp_dir(name);
    fs::create_dir_all(dir.join("src")).expect("src");
    fs::write(
        dir.join("README.md"),
        "# Demo\n\nA small crate used to test the repository wiki generator.\n",
    )
    .expect("readme");
    fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
    )
    .expect("manifest");
    fs::write(
        dir.join("src/main.rs"),
        "fn main() { println!(\"demo\"); }\n",
    )
    .expect("main");
    fs::write(dir.join("src/lib.rs"), "pub fn helper() -> u32 { 1 }\n").expect("lib");
    dir
}

fn page_markdown(title: &str) -> String {
    format!(
        "# {title}\n\n\
This page explains the demo repository and its source-grounded entry points. \
The crate has a binary entry point in `src/main.rs:1` and a reusable library surface in `src/lib.rs:1`. \
The overview starts from `README.md` and `Cargo.toml`, then follows the code into the library.\n\n\
## Purpose\n\n\
The demo exists to verify that wiki planning, prompting, application, and source freshness checks work as one pipeline. \
The plan is mechanical and the prose is supplied by a model. \
The generated page is a human-facing artifact; it is never loaded into cm recall and never becomes a memory record. \
Use the [wiki index](index.md) to move between pages without guessing file names.\n\n\
## Architecture and data flow\n\n\
The binary in `src/main.rs` calls the helper in `src/lib.rs`. \
There is no persistent state, network client, or background worker in this demo, so the data flow is a direct call from the process entry point to the reusable function. \
That small shape is intentional: it makes every stage of the documentation pipeline observable without hiding behavior behind a framework.\n\n\
```mermaid\n\
flowchart LR\n\
  Main[src/main.rs] --> Lib[src/lib.rs]\n\
  Plan[wiki plan] --> Prompt[wiki prompt]\n\
  Prompt --> Apply[wiki apply]\n\
  Apply --> Check[wiki check]\n\
```\n\n\
## Build and test\n\n\
Run `cargo build` and `cargo test` from the repository root using `Cargo.toml`. \
The manifest is the only build contract in this demo, and the generated prose must cite it rather than inventing commands that the repository does not support.\n\n\
## Failure behavior\n\n\
If a target file disappears, the page is marked stale rather than silently trusted. \
The manifest records a source hash for every page, so a changed README, manifest, or source directory \
makes the page visibly stale before its prose is trusted again. This keeps generated documentation \
from silently drifting away from the code it describes. A failed generation leaves the previous page in place \
and records the page id in the failure report instead of writing partial prose.\n"
    )
}

#[test]
fn wiki_plan_prompt_apply_and_stale_detection() {
    let root = demo_repo("lifecycle");
    let out = root.join(".cm/wiki");
    let plan = wiki::plan(&root, &out).expect("plan");
    assert!(plan.pages.iter().any(|page| page.id == "overview"));
    assert!(plan.pages.iter().any(|page| page.id == "architecture"));
    assert!(plan.pages.iter().any(|page| page.id == "build-and-test"));
    assert!(plan.pages.iter().any(|page| page.id == "module-src"));

    let next = wiki::next(&out).expect("next").expect("pending page");
    assert_eq!(next.0.page.id, "overview");

    let prompt = wiki::prompt(&out, "overview").expect("prompt");
    assert!(prompt.contains("not an outline"));
    assert!(prompt.contains("Project Overview"));
    assert!(prompt.contains("README.md"));
    assert!(prompt.contains("self-check"));
    assert!(prompt.contains("Never cite a call site"));
    let module_prompt = wiki::prompt(&out, "module-src").expect("module prompt");
    assert!(module_prompt.contains("EVIDENCE"));
    assert!(module_prompt.contains("src/lib.rs"));
    assert!(module_prompt.contains("helper"));
    assert!(module_prompt.contains("package-root export surface"));

    let markdown = page_markdown("Project Overview");
    let state = wiki::apply(&out, "overview", &markdown).expect("apply");
    assert_eq!(state.status, "written");
    assert!(out.join("overview.md").is_file());
    assert!(out.join("index.md").is_file());

    let status = wiki::status(&out).expect("status");
    assert_eq!(status.written, 1);
    assert_eq!(status.stale, 0);
    let check = wiki::check(&out).expect("check");
    assert!(check.issues.is_empty(), "{:?}", check.issues);

    fs::write(
        root.join("README.md"),
        "# Demo changed\n\nThe overview source changed.\n",
    )
    .expect("change readme");
    let stale = wiki::status(&out).expect("stale status");
    assert!(stale.stale >= 1);
    assert!(stale
        .pages
        .iter()
        .any(|page| page.page.id == "overview" && page.status == "stale"));
    let next = wiki::next(&out).expect("next stale").expect("stale page");
    assert_eq!(next.0.page.id, "overview");
    assert_eq!(next.0.status, "stale");
    let stale_prompt = wiki::prompt(&out, "overview").expect("stale prompt");
    assert!(stale_prompt.contains("This page is stale"));
    assert!(stale_prompt.contains("current TARGETS"));

    fs::remove_dir_all(&root).ok();
}

#[test]
fn wiki_pages_can_be_remembered_as_pointer_memories() {
    let root = demo_repo("remember");
    let out = root.join(".cm/wiki");
    wiki::plan(&root, &out).expect("plan");
    let state = wiki::apply(&out, "overview", &page_markdown("Project Overview")).expect("apply");
    assert_eq!(state.status, "written");

    let cm_path = root.join("cm");
    cm_memory::Cm::init(&cm_path, true).expect("init memory");
    let report =
        wiki::remember_pages(&cm_path, &out, None, "repo", true, false).expect("remember pages");
    assert_eq!(report.remembered, 1);
    assert_eq!(report.ids.len(), 1);

    let hits = cm_memory::Cm::open(&cm_path)
        .expect("open memory")
        .search("demo repository", 5)
        .expect("search");
    assert!(hits
        .iter()
        .any(|memory| memory.kind == "procedure" && memory.subject == "wiki: Project Overview"));

    let again =
        wiki::remember_pages(&cm_path, &out, None, "repo", true, false).expect("replace pages");
    assert_eq!(again.remembered, 1);
    let active = cm_memory::Cm::open(&cm_path)
        .expect("open memory")
        .search("wiki", 20)
        .expect("search");
    assert_eq!(
        active
            .iter()
            .filter(|memory| memory.subject == "wiki: Project Overview")
            .count(),
        1
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn wiki_plan_includes_testing_page_when_tests_exist() {
    let root = demo_repo("testing-page");
    fs::create_dir_all(root.join("tests")).expect("tests dir");
    fs::write(
        root.join("tests/smoke.rs"),
        "#[test]\nfn smoke() { assert_eq!(1, 1); }\n",
    )
    .expect("test file");
    let out = root.join(".cm/wiki");
    let plan = wiki::plan(&root, &out).expect("plan");
    let testing = plan
        .pages
        .iter()
        .find(|page| page.id == "testing")
        .expect("testing page");
    assert!(testing
        .targets
        .iter()
        .any(|target| target == "tests/smoke.rs"));
    assert!(testing.targets.iter().any(|target| target == "Cargo.toml"));
    fs::remove_dir_all(&root).ok();
}

#[test]
fn wiki_apply_rejects_outline_sized_output() {
    let root = demo_repo("short");
    let out = root.join(".cm/wiki");
    wiki::plan(&root, &out).expect("plan");
    let error =
        wiki::apply(&out, "overview", "# Project Overview\n\nToo short.").expect_err("reject");
    assert!(format!("{error:#}").contains("not an outline"));
    fs::remove_dir_all(&root).ok();
}

#[test]
fn wiki_apply_rejects_the_wrong_page_title() {
    let root = demo_repo("wrong-title");
    let out = root.join(".cm/wiki");
    wiki::plan(&root, &out).expect("plan");
    let error = wiki::apply(&out, "overview", &page_markdown("A Different Page"))
        .expect_err("reject wrong title");
    assert!(format!("{error:#}").contains("does not match planned title"));
    fs::remove_dir_all(&root).ok();
}

#[test]
fn wiki_check_flags_bad_line_citations_and_broken_links() {
    let root = demo_repo("check");
    let out = root.join(".cm/wiki");
    wiki::plan(&root, &out).expect("plan");
    let markdown = page_markdown("Project Overview")
        .replace("src/main.rs:1", "src/main.rs:999")
        .replace("src/lib.rs:1", "src/lib.rs:helper")
        + "\nA dotted symbol such as `AgentSession.exportToHtml` is not a path.\n\
           An invalid range is `src/main.rs:1-999`.\n\
           A bare bad line is src/main.rs:9999.\n\
           See [the missing page](missing-page.md).\n";
    wiki::apply(&out, "overview", &markdown).expect("apply");
    let report = wiki::check(&out).expect("check");
    assert!(
        report
            .issues
            .iter()
            .any(|issue| issue.contains("citation does not resolve: src/main.rs:999")),
        "{:?}",
        report.issues
    );
    assert!(
        report
            .issues
            .iter()
            .any(|issue| issue.contains("wiki link does not resolve: missing-page.md")),
        "{:?}",
        report.issues
    );
    assert!(
        report
            .issues
            .iter()
            .any(|issue| issue.contains("citation does not resolve: src/main.rs:1-999")),
        "{:?}",
        report.issues
    );
    assert!(
        report
            .issues
            .iter()
            .any(|issue| issue.contains("citation does not resolve: src/main.rs:9999")),
        "{:?}",
        report.issues
    );
    assert!(
        report
            .issues
            .iter()
            .any(|issue| issue.contains("citation uses a line range")),
        "{:?}",
        report.issues
    );
    assert!(
        !report
            .issues
            .iter()
            .any(|issue| issue.contains("src/lib.rs")),
        "symbol citation should resolve by its path: {:?}",
        report.issues
    );
    assert!(
        !report
            .issues
            .iter()
            .any(|issue| issue.contains("AgentSession.exportToHtml")),
        "dotted symbol is not a path: {:?}",
        report.issues
    );
    fs::remove_dir_all(&root).ok();
}

#[test]
fn wiki_check_rejects_bare_line_shorthand_and_oversized_pages() {
    let root = demo_repo("lint");
    let out = root.join(".cm/wiki");
    wiki::plan(&root, &out).expect("plan");
    let mut markdown = page_markdown("Project Overview");
    markdown.push_str("\nSee `:1` for the same detail.\n");
    markdown.push_str(&"extra detail ".repeat(1600));
    wiki::apply(&out, "overview", &markdown).expect("apply");
    let report = wiki::check(&out).expect("check");
    assert!(
        report
            .issues
            .iter()
            .any(|issue| issue.contains("bare line reference :1")),
        "{:?}",
        report.issues
    );
    assert!(
        report
            .issues
            .iter()
            .any(|issue| issue.contains("page exceeds")),
        "{:?}",
        report.issues
    );
    fs::remove_dir_all(&root).ok();
}

#[test]
fn wiki_check_resolves_an_ambiguous_package_relative_line() {
    let root = demo_repo("ambiguous");
    fs::create_dir_all(root.join("packages/one/src")).expect("one");
    fs::create_dir_all(root.join("packages/two/src")).expect("two");
    let mut lines = String::new();
    for index in 1..=30 {
        lines.push_str(&format!("export const value{index} = {index};\n"));
    }
    fs::write(root.join("packages/one/src/index.ts"), lines).expect("long index");
    fs::write(
        root.join("packages/two/src/index.ts"),
        "export const short = 1;\n",
    )
    .expect("short index");
    let out = root.join(".cm/wiki");
    wiki::plan(&root, &out).expect("plan");
    let markdown =
        page_markdown("Project Overview") + "\nThe package entry is `src/index.ts:26`.\n";
    wiki::apply(&out, "overview", &markdown).expect("apply");
    let report = wiki::check(&out).expect("check");
    assert!(
        !report
            .issues
            .iter()
            .any(|issue| issue.contains("src/index.ts:26")),
        "ambiguous package-relative citation should resolve: {:?}",
        report.issues
    );
    fs::remove_dir_all(&root).ok();
}

#[test]
fn wiki_plan_uses_manifest_title_and_deep_module_files() {
    let root = demo_repo("deep-files");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"deep-crate\"\nversion = \"0.1.0\"\n",
    )
    .expect("manifest");
    fs::create_dir_all(root.join("src/deep")).expect("deep dir");
    for index in 0..65 {
        fs::write(
            root.join(format!("src/a{index:02}.rs")),
            format!("pub fn f{index}() {{}}\n"),
        )
        .expect("source");
    }
    fs::write(root.join("src/deep/lib.rs"), "pub fn deep() {}\n").expect("deep lib");
    let out = root.join(".cm/wiki");
    let plan = wiki::plan(&root, &out).expect("plan");
    let module = plan
        .pages
        .iter()
        .find(|page| page.id == "module-src")
        .expect("module page");
    assert_eq!(module.title, "Module: deep-crate");
    assert!(module
        .targets
        .iter()
        .any(|target| target == "src/deep/lib.rs"));
    fs::remove_dir_all(&root).ok();
}

fn strict_page_markdown() -> String {
    "# Project Overview\n\n\
This repository is a minimal demo whose source of truth is `README.md`. \
The page links back to the [wiki index](index.md) and does not cite files that do not exist.\n\n\
## What it is\n\n\
The demo is intentionally small. It exists to verify the strict acceptance gate for one complete page: \
the heading matches the plan, the prose is long enough, the section count is high enough, and every path citation resolves. \
A strict check must pass only when the page is complete and the manifest has no planned or stale pages left.\n\n\
## Problem and users\n\n\
The user is a coding agent that needs a compact, source-grounded explanation instead of a file inventory. \
The page explains the repository boundary, the reading order, and the evidence rules used by the generator. \
It does not claim behavior that `README.md` does not establish, and it marks unknowns explicitly instead of guessing.\n\n\
## Main components\n\n\
There is one documentation surface, one generated index, and one source file. \
The index is produced by the planner and links to every written page. \
The source file is the only target used for freshness checks in this fixture, so a change to it makes the page stale while a change elsewhere does not.\n\n\
## Runtime surfaces\n\n\
There is no long-running process or network surface in this fixture. \
Generation is a sequence of plan, prompt, apply, and check calls. \
Each call has a deterministic input and a visible output, which makes failures reproducible.\n\n\
## Reading order\n\n\
A new reader should start with the purpose, then read the architecture and runtime sections before scanning the failure behavior. \
The order is intentional because the later sections refer back to the single source file and the generated index. \
A coding agent can stop after the runtime section when it only needs to locate the entry point, \
while a human reviewer should continue to the boundaries section to see what the fixture deliberately leaves out.\n\n\
## Failure behavior\n\n\
A failed agent command leaves the previous page in place and reports the page id. \
A missing citation or a broken link is an acceptance issue. \
A stale page is not silently trusted: `cm wiki status` records the stale state and strict checking reports it until the page is regenerated.\n\n\
## Boundaries\n\n\
This fixture does not claim to model a production service, a database, or a distributed system. \
Those surfaces would need their own pages and their own evidence. \
The value of the page is that it is honest about its scope: it names the one source file it can verify, \
states which behaviors are absent, and gives the reader a precise place to start rather than a broad claim that cannot be checked.\n"
        .to_string()
}

#[test]
fn wiki_plan_module_budget_is_configurable() {
    let root = temp_dir("module-budget");
    fs::write(root.join("README.md"), "# Budget\n\nMany areas.\n").expect("readme");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"budget\"\nversion = \"0.1.0\"\n",
    )
    .expect("manifest");
    for area in ["alpha", "beta", "gamma"] {
        let dir = root.join(area);
        fs::create_dir_all(&dir).expect("area");
        for index in 0..6 {
            fs::write(dir.join(format!("m{index}.rs")), "pub fn f() {}\n").expect("source");
        }
    }
    let out = root.join(".cm/wiki");
    let plan = wiki::plan_with(&root, &out, 2).expect("plan");
    assert_eq!(
        plan.pages
            .iter()
            .filter(|page| page.kind == "module")
            .count(),
        2
    );
    fs::remove_dir_all(&root).ok();
}

#[test]
fn wiki_plan_prefers_large_module_files_over_nested_barrels() {
    let root = temp_dir("module-size");
    fs::write(root.join("README.md"), "# Size\n\nOne module.\n").expect("readme");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"size\"\nversion = \"0.1.0\"\n",
    )
    .expect("manifest");
    fs::create_dir_all(root.join("core")).expect("core");
    fs::write(root.join("core/index.ts"), "export {};\n").expect("index");
    for index in 0..12 {
        let dir = root.join(format!("core/sub{index}"));
        fs::create_dir_all(&dir).expect("sub");
        fs::write(dir.join("index.ts"), "export {};\n").expect("barrel");
    }
    for index in 0..20 {
        fs::write(
            root.join(format!("core/a{index:02}.ts")),
            "export const x = 1;\n",
        )
        .expect("small");
    }
    let mut big = String::new();
    for index in 0..500 {
        big.push_str(&format!("export const value{index} = {index};\n"));
    }
    fs::write(root.join("core/big.ts"), big).expect("big");
    fs::write(root.join("core/template.css"), "body {}\n").expect("template");
    let out = root.join(".cm/wiki");
    let plan = wiki::plan(&root, &out).expect("plan");
    let module = plan
        .pages
        .iter()
        .find(|page| page.id == "module-core")
        .expect("module page");
    assert!(module.targets.iter().any(|target| target == "core/big.ts"));
    assert!(!module
        .targets
        .iter()
        .any(|target| target == "core/template.css"));
    fs::remove_dir_all(&root).ok();
}

#[test]
fn wiki_module_titles_stay_distinct_inside_one_package() {
    let root = temp_dir("module-titles");
    fs::write(root.join("README.md"), "# Monorepo\n\nTwo modules.\n").expect("readme");
    fs::write(root.join("package.json"), "{\"name\": \"@scope/root\"}\n").expect("root package");
    for area in ["core", "api"] {
        let dir = root.join(format!("packages/thing/src/{area}"));
        fs::create_dir_all(&dir).expect("area");
        fs::write(
            root.join("packages/thing/package.json"),
            "{\"name\": \"@scope/thing\"}\n",
        )
        .expect("package");
        for index in 0..6 {
            fs::write(dir.join(format!("m{index}.ts")), "export const x = 1;\n").expect("source");
        }
    }
    let out = root.join(".cm/wiki");
    let plan = wiki::plan(&root, &out).expect("plan");
    let titles: Vec<&str> = plan
        .pages
        .iter()
        .filter(|page| page.kind == "module")
        .map(|page| page.title.as_str())
        .collect();
    assert!(titles.contains(&"Module: thing / core"), "{titles:?}");
    assert!(titles.contains(&"Module: thing / api"), "{titles:?}");
    let unique: std::collections::HashSet<&str> = titles.iter().copied().collect();
    assert_eq!(
        unique.len(),
        titles.len(),
        "duplicate module titles: {titles:?}"
    );
    fs::remove_dir_all(&root).ok();
}

#[test]
fn wiki_strict_check_flags_missing_diagram_and_incomplete_wiki() {
    let root = demo_repo("strict-fail");
    let out = root.join(".cm/wiki");
    wiki::plan(&root, &out).expect("plan");
    let markdown = page_markdown("Architecture").replace("```mermaid", "```text");
    wiki::apply(&out, "architecture", &markdown).expect("apply");
    let report = wiki::check_with(&out, true).expect("strict check");
    assert!(
        report
            .issues
            .iter()
            .any(|issue| issue.contains("must include a Mermaid diagram")),
        "{:?}",
        report.issues
    );
    assert!(
        report
            .issues
            .iter()
            .any(|issue| issue.contains("still planned")),
        "{:?}",
        report.issues
    );
    fs::remove_dir_all(&root).ok();
}

#[test]
fn wiki_strict_check_accepts_a_complete_single_page_wiki() {
    let root = temp_dir("strict");
    fs::write(
        root.join("README.md"),
        "# Strict Demo\n\nA minimal repository with one documentation target.\n",
    )
    .expect("readme");
    let out = root.join(".cm/wiki");
    let plan = wiki::plan(&root, &out).expect("plan");
    assert_eq!(plan.pages.len(), 1);
    wiki::apply(&out, "overview", &strict_page_markdown()).expect("apply");
    let report = wiki::check_with(&out, true).expect("strict check");
    assert!(report.issues.is_empty(), "{:?}", report.issues);
    fs::remove_dir_all(&root).ok();
}

#[test]
fn wiki_generate_runs_one_agent_command_per_pending_page() {
    let root = demo_repo("generate");
    let out = root.join(".cm/wiki");
    wiki::plan(&root, &out).expect("plan");
    let page = root.join("agent-page.md");
    fs::write(
        &page,
        format!(
            "Here is the requested page:\n\n```markdown\n{}\n```\n",
            page_markdown("Project Overview")
        ),
    )
    .expect("agent page");
    let command = if cfg!(windows) {
        "type agent-page.md".to_string()
    } else {
        "cat agent-page.md".to_string()
    };
    let report = wiki::generate(&out, &command, 1).expect("generate");
    assert_eq!(report.attempted, 1);
    assert_eq!(report.written, 1);
    assert!(report.failed.is_empty());
    assert!(out.join("overview.md").is_file());

    fs::write(
        root.join("README.md"),
        "# Demo changed\n\nThe overview source changed.\n",
    )
    .expect("change");
    assert!(wiki::status(&out).expect("stale").stale >= 1);
    let report = wiki::generate(&out, &command, 1).expect("regenerate");
    assert_eq!(report.written, 1, "{:?}", report.failed);
    assert_eq!(wiki::status(&out).expect("clean").stale, 0);
    fs::remove_dir_all(&root).ok();
}

fn review_script(root: &std::path::Path, name: &str, verdict: &str) -> String {
    if cfg!(windows) {
        fs::write(
            root.join(format!("{name}.cmd")),
            format!(
                "@echo off\r\necho {{\"verdict\":\"{verdict}\",\"score\":90,\"issues\":[]}}\r\n"
            ),
        )
        .expect("review script");
        format!("{name}.cmd")
    } else {
        fs::write(
            root.join(format!("{name}.sh")),
            format!("#!/bin/sh\nprintf '%s\\n' '{{\"verdict\":\"{verdict}\",\"score\":90,\"issues\":[]}}'\n"),
        )
        .expect("review script");
        format!("sh {name}.sh")
    }
}

#[test]
fn wiki_review_and_generate_with_reviewer() {
    let root = demo_repo("review");
    let out = root.join(".cm/wiki");
    wiki::plan(&root, &out).expect("plan");
    wiki::apply(&out, "overview", &page_markdown("Project Overview")).expect("apply");
    let reviewer = review_script(&root, "review-pass", "pass");
    let review = wiki::review(&out, "overview", &reviewer).expect("review");
    assert_eq!(review.verdict, "pass");
    assert_eq!(review.score, Some(90));
    let status = wiki::status(&out).expect("status");
    let overview = status
        .pages
        .iter()
        .find(|state| state.page.id == "overview")
        .expect("overview page");
    assert_eq!(
        overview.review.as_ref().map(|review| review.score),
        Some(Some(90))
    );
    assert_eq!(status.review_passed, 1);

    let root = demo_repo("reviewed-generate");
    let out = root.join(".cm/wiki");
    wiki::plan(&root, &out).expect("plan");
    fs::write(
        root.join("agent-page.md"),
        page_markdown("Project Overview"),
    )
    .expect("page");
    let agent = if cfg!(windows) {
        "type agent-page.md".to_string()
    } else {
        "cat agent-page.md".to_string()
    };
    let reviewer = review_script(&root, "review-pass", "pass");
    let report = wiki::generate_reviewed(&out, &agent, Some(&reviewer), 1, 1).expect("generate");
    assert_eq!(report.attempted, 1);
    assert_eq!(report.written, 1, "{:?}", report.failed);
    let status = wiki::status(&out).expect("status");
    assert_eq!(status.review_passed, 1);
    assert_eq!(status.review_failed, 0);
    fs::remove_dir_all(&root).ok();
}

#[test]
fn wiki_generate_rejects_an_agent_that_modifies_its_targets() {
    let root = demo_repo("tamper");
    let out = root.join(".cm/wiki");
    wiki::plan(&root, &out).expect("plan");
    fs::write(
        root.join("agent-page.md"),
        page_markdown("Project Overview"),
    )
    .expect("page");
    let command = if cfg!(windows) {
        fs::write(
            root.join("tamper.cmd"),
            "@echo off\r\necho tampered >> README.md\r\ntype agent-page.md\r\n",
        )
        .expect("script");
        "tamper.cmd".to_string()
    } else {
        fs::write(
            root.join("tamper.sh"),
            "#!/bin/sh\necho tampered >> README.md\ncat agent-page.md\n",
        )
        .expect("script");
        "sh tamper.sh".to_string()
    };
    let report = wiki::generate(&out, &command, 1).expect("generate");
    assert_eq!(report.attempted, 1);
    assert_eq!(report.written, 0);
    assert!(
        report
            .failed
            .iter()
            .any(|error| error.contains("modified target files")),
        "{:?}",
        report.failed
    );
    assert!(!out.join("overview.md").is_file());
    fs::remove_dir_all(&root).ok();
}

#[test]
fn wiki_status_uses_git_changes_for_staleness() {
    use std::process::Command;

    let root = demo_repo("git");
    let git = |args: &[&str]| {
        Command::new("git")
            .current_dir(&root)
            .args(args)
            .output()
            .expect("run git")
    };
    if Command::new("git").arg("--version").output().is_err() {
        fs::remove_dir_all(&root).ok();
        return;
    }
    assert!(git(&["init", "-q"]).status.success());
    assert!(git(&["config", "user.email", "test@example.com"])
        .status
        .success());
    assert!(git(&["config", "user.name", "test"]).status.success());
    assert!(git(&["add", "."]).status.success());
    assert!(git(&["commit", "-qm", "initial"]).status.success());

    let out = root.join(".cm/wiki");
    wiki::plan(&root, &out).expect("plan");
    let page = root.join("overview-page.md");
    fs::write(&page, page_markdown("Project Overview")).expect("page");
    wiki::apply(&out, "overview", &page_markdown("Project Overview")).expect("apply");
    assert_eq!(wiki::status(&out).expect("clean").stale, 0);

    fs::write(
        root.join("README.md"),
        "# Demo\n\nA changed README for git-aware staleness.\n",
    )
    .expect("change");
    let status = wiki::status(&out).expect("stale");
    assert!(status.stale >= 1);
    fs::remove_dir_all(&root).ok();
}

#[test]
fn wiki_dirty_target_uses_hash_instead_of_head() {
    use std::process::Command;

    let root = demo_repo("dirty-target");
    if Command::new("git").arg("--version").output().is_err() {
        fs::remove_dir_all(&root).ok();
        return;
    }
    let git = |args: &[&str]| {
        Command::new("git")
            .current_dir(&root)
            .args(args)
            .output()
            .expect("run git")
    };
    assert!(git(&["init", "-q"]).status.success());
    assert!(git(&["config", "user.email", "test@example.com"])
        .status
        .success());
    assert!(git(&["config", "user.name", "test"]).status.success());
    assert!(git(&["add", "."]).status.success());
    assert!(git(&["commit", "-qm", "initial"]).status.success());

    fs::write(root.join("README.md"), "# Demo\n\nDirty target content.\n").expect("dirty readme");
    let out = root.join(".cm/wiki");
    wiki::plan(&root, &out).expect("plan");
    let state = wiki::apply(&out, "overview", &page_markdown("Project Overview")).expect("apply");
    assert!(state.source_commit.is_none());
    assert_eq!(wiki::status(&out).expect("clean").stale, 0);

    assert!(git(&["add", "README.md"]).status.success());
    assert!(git(&["commit", "-qm", "readme"]).status.success());
    assert_eq!(wiki::status(&out).expect("after commit").stale, 0);

    fs::write(root.join("README.md"), "# Demo\n\nAnother dirty change.\n").expect("change again");
    assert!(wiki::status(&out).expect("stale").stale >= 1);
    fs::remove_dir_all(&root).ok();
}

#[test]
fn wiki_root_target_tracks_any_source_change_but_not_its_own_output() {
    use std::process::Command;

    let root = temp_dir("root-target");
    if Command::new("git").arg("--version").output().is_err() {
        fs::remove_dir_all(&root).ok();
        return;
    }
    let git = |args: &[&str]| {
        Command::new("git")
            .current_dir(&root)
            .args(args)
            .output()
            .expect("run git")
    };
    assert!(git(&["init", "-q"]).status.success());
    assert!(git(&["config", "user.email", "test@example.com"])
        .status
        .success());
    assert!(git(&["config", "user.name", "test"]).status.success());
    assert!(git(&["commit", "--allow-empty", "-qm", "empty"])
        .status
        .success());

    let out = root.join(".cm/wiki");
    let plan = wiki::plan(&root, &out).expect("plan");
    assert_eq!(plan.pages.len(), 1);
    assert_eq!(plan.pages[0].targets, vec!["."]);
    wiki::apply(&out, "overview", &strict_page_markdown()).expect("apply");
    assert_eq!(wiki::status(&out).expect("clean").stale, 0);

    fs::write(root.join("notes.txt"), "changed source\n").expect("source change");
    let status = wiki::status(&out).expect("stale");
    assert!(status.stale >= 1, "{:?}", status.pages);
    fs::remove_dir_all(&root).ok();
}

#[test]
fn wiki_module_targets_include_the_package_root_surface() {
    let root = temp_dir("package-surface");
    fs::create_dir_all(root.join("packages/thing/src/core")).expect("core dir");
    fs::write(
        root.join("README.md"),
        "# Thing\n\nA workspace package used to test module planning.\n",
    )
    .expect("readme");
    fs::write(
        root.join("package.json"),
        "{\"name\":\"thing-workspace\",\"private\":true}\n",
    )
    .expect("manifest");
    fs::write(
        root.join("packages/thing/package.json"),
        "{\"name\":\"thing\",\"exports\":{\".\":\"./src/index.ts\"}}\n",
    )
    .expect("package manifest");
    fs::write(
        root.join("packages/thing/src/index.ts"),
        "export { Core } from \"./core/index.ts\";\n",
    )
    .expect("package barrel");
    for name in ["a", "b", "c", "d", "e"] {
        fs::write(
            root.join(format!("packages/thing/src/core/{name}.ts")),
            format!("export const {name} = 1;\n"),
        )
        .expect("core source");
    }
    let out = root.join(".cm/wiki");
    let plan = wiki::plan(&root, &out).expect("plan");
    let core = plan
        .pages
        .iter()
        .find(|page| page.title == "Module: thing / core")
        .expect("core module page");
    assert!(
        core.targets
            .contains(&"packages/thing/src/index.ts".to_string()),
        "{:?}",
        core.targets
    );
    assert!(
        core.targets
            .contains(&"packages/thing/package.json".to_string()),
        "{:?}",
        core.targets
    );
    fs::remove_dir_all(&root).ok();
}

#[test]
fn wiki_strict_check_flags_unmentioned_target_file() {
    let root = demo_repo("coverage-fail");
    let out = root.join(".cm/wiki");
    let plan = wiki::plan(&root, &out).expect("plan");
    let title = plan
        .pages
        .iter()
        .find(|page| page.id == "module-src")
        .expect("module page")
        .title
        .clone();
    let markdown = page_markdown(&title).replace("src/lib.rs", "src/main.rs");
    wiki::apply(&out, "module-src", &markdown).expect("apply");
    let report = wiki::check_with(&out, true).expect("strict check");
    assert!(
        report
            .issues
            .iter()
            .any(|issue| issue.contains("target is not covered: src/lib.rs")),
        "{:?}",
        report.issues
    );
    fs::remove_dir_all(&root).ok();
}

#[test]
fn wiki_generate_page_rejects_output_that_omits_a_target() {
    let root = demo_repo("generate-missing-target");
    let out = root.join(".cm/wiki");
    let plan = wiki::plan(&root, &out).expect("plan");
    let title = plan
        .pages
        .iter()
        .find(|page| page.id == "module-src")
        .expect("module page")
        .title
        .clone();
    let page = root.join("agent-page.md");
    let markdown = page_markdown(&title).replace("src/lib.rs", "src/main.rs");
    fs::write(&page, markdown).expect("agent page");
    let command = if cfg!(windows) {
        "type agent-page.md".to_string()
    } else {
        "cat agent-page.md".to_string()
    };
    let report = wiki::generate_page(&out, &command, "module-src").expect("generate");
    assert_eq!(report.written, 0, "{:?}", report.failed);
    assert!(
        report
            .failed
            .iter()
            .any(|failure| failure.contains("target is not covered: src/lib.rs")),
        "{:?}",
        report.failed
    );
    assert_eq!(wiki::status(&out).expect("status").stale, 1);
    fs::remove_dir_all(&root).ok();
}

#[test]
fn wiki_generate_reviewed_retries_after_mechanical_failure() {
    let root = demo_repo("reviewed-mechanical-retry");
    let out = root.join(".cm/wiki");
    let plan = wiki::plan(&root, &out).expect("plan");
    let title = plan
        .pages
        .iter()
        .find(|page| page.id == "module-src")
        .expect("module page")
        .title
        .clone();
    fs::write(
        root.join("bad.md"),
        page_markdown(&title).replace("src/lib.rs", "src/main.rs"),
    )
    .expect("bad page");
    fs::write(root.join("good.md"), page_markdown(&title)).expect("good page");
    let agent = if cfg!(windows) {
        fs::write(
            root.join("retry.cmd"),
            "@echo off\r\nif exist count.txt (\r\n  type good.md\r\n) else (\r\n  echo 1> count.txt\r\n  type bad.md\r\n)\r\n",
        )
        .expect("agent script");
        "retry.cmd".to_string()
    } else {
        fs::write(
            root.join("retry.sh"),
            "#!/bin/sh\nif [ -f count.txt ]; then cat good.md; else echo 1 > count.txt; cat bad.md; fi\n",
        )
        .expect("agent script");
        "sh retry.sh".to_string()
    };
    let reviewer = review_script(&root, "review-pass-retry", "pass");
    let report = wiki::generate_reviewed_page(&out, &agent, Some(&reviewer), 2, "module-src")
        .expect("generate");
    assert_eq!(report.written, 1, "{:?}", report.failed);
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert_eq!(wiki::status(&out).expect("status").stale, 0);
    fs::remove_dir_all(&root).ok();
}

#[test]
fn wiki_check_ignores_bare_package_subpath_prose() {
    let root = demo_repo("package-subpath");
    let out = root.join(".cm/wiki");
    wiki::plan(&root, &out).expect("plan");
    let markdown = page_markdown("Project Overview").replace(
        "The crate has a binary entry point",
        "The core does not import `pi-ai/compat`; it may read `~/.config/gcloud/application_default_credentials.json`; the crate has a binary entry point",
    );
    wiki::apply(&out, "overview", &markdown).expect("apply");
    let report = wiki::check(&out).expect("check");
    assert!(report.issues.is_empty(), "{:?}", report.issues);
    fs::remove_dir_all(&root).ok();
}

#[test]
fn wiki_review_failure_is_recorded_and_revision_clears_it() {
    let root = demo_repo("review-state");
    let out = root.join(".cm/wiki");
    wiki::plan(&root, &out).expect("plan");
    wiki::apply(&out, "overview", &page_markdown("Project Overview")).expect("apply");
    let reviewer = review_script(&root, "review-fail-state", "fail");
    let review = wiki::review(&out, "overview", &reviewer).expect("review");
    assert_eq!(review.verdict, "fail");
    let status = wiki::status(&out).expect("status");
    assert_eq!(status.review_failed, 1);
    assert_eq!(status.review_passed, 0);
    let overview = status
        .pages
        .iter()
        .find(|state| state.page.id == "overview")
        .expect("overview");
    assert_eq!(
        overview
            .review
            .as_ref()
            .map(|review| review.verdict.as_str()),
        Some("fail")
    );

    wiki::apply(&out, "overview", &page_markdown("Project Overview")).expect("apply again");
    let status = wiki::status(&out).expect("status after revision");
    assert_eq!(status.review_failed, 0);
    assert_eq!(status.review_passed, 0);
    fs::remove_dir_all(&root).ok();
}

#[test]
fn wiki_plan_architecture_targets_are_bounded_files() {
    let root = demo_repo("architecture-targets");
    let out = root.join(".cm/wiki");
    let plan = wiki::plan(&root, &out).expect("plan");
    let architecture = plan
        .pages
        .iter()
        .find(|page| page.kind == "architecture")
        .expect("architecture page");
    assert!(
        architecture.targets.len() <= 20,
        "{:?}",
        architecture.targets
    );
    assert!(
        architecture
            .targets
            .iter()
            .all(|target| root.join(target).is_file()),
        "{:?}",
        architecture.targets
    );
    assert!(architecture
        .targets
        .iter()
        .any(|target| target == "Cargo.toml"));
    assert!(architecture
        .targets
        .iter()
        .any(|target| target == "src/main.rs"));
    fs::remove_dir_all(&root).ok();
}

#[test]
fn wiki_plan_key_flows_prefers_real_entrypoints() {
    let root = demo_repo("flow-targets");
    let out = root.join(".cm/wiki");
    let plan = wiki::plan(&root, &out).expect("plan");
    let flows = plan
        .pages
        .iter()
        .find(|page| page.kind == "flows")
        .expect("flows page");
    assert!(flows.targets.iter().any(|target| target == "src/main.rs"));
    assert!(!flows
        .targets
        .iter()
        .any(|target| target.contains("examples/")));
    fs::remove_dir_all(&root).ok();
}

#[test]
fn wiki_review_pass_with_issues_is_not_clean() {
    let root = demo_repo("review-not-clean");
    let out = root.join(".cm/wiki");
    wiki::plan(&root, &out).expect("plan");
    wiki::apply(&out, "overview", &page_markdown("Project Overview")).expect("apply");
    let reviewer = if cfg!(windows) {
        fs::write(
            root.join("review-warn.cmd"),
            "@echo off\r\necho {\"verdict\":\"pass\",\"score\":90,\"issues\":[\"minor citation issue\"]}\r\n",
        )
        .expect("review script");
        "review-warn.cmd".to_string()
    } else {
        fs::write(
            root.join("review-warn.sh"),
            "#!/bin/sh\nprintf '%s\n' '{\"verdict\":\"pass\",\"score\":90,\"issues\":[\"minor citation issue\"]}'\n",
        )
        .expect("review script");
        "sh review-warn.sh".to_string()
    };
    let review = wiki::review(&out, "overview", &reviewer).expect("review");
    assert_eq!(review.verdict, "pass");
    assert!(!review.issues.is_empty());
    let status = wiki::status(&out).expect("status");
    assert_eq!(status.review_passed, 0);
    assert_eq!(status.review_failed, 1);
    fs::remove_dir_all(&root).ok();
}

#[test]
fn wiki_excerpt_returns_only_matching_sections() {
    let root = demo_repo("excerpt");
    let out = root.join(".cm/wiki");
    wiki::plan(&root, &out).expect("plan");
    wiki::apply(&out, "overview", &page_markdown("Project Overview")).expect("apply");

    let build = wiki::excerpt(&out, "overview", "build test", 600, 1).expect("build excerpt");
    assert!(build.text.contains("## Build and test"), "{}", build.text);
    assert!(
        !build.text.contains("## Architecture and data flow"),
        "{}",
        build.text
    );
    assert_eq!(build.sections.len(), 1);
    assert_eq!(build.sections[0].heading, "Build and test");
    assert!(build.estimated_tokens <= 600);

    let architecture =
        wiki::excerpt(&out, "overview", "architecture data flow", 600, 1).expect("arch excerpt");
    assert!(
        architecture.text.contains("## Architecture and data flow"),
        "{}",
        architecture.text
    );
    assert!(
        !architecture.text.contains("## Build and test"),
        "{}",
        architecture.text
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn wiki_find_routes_a_query_to_the_matching_page() {
    let root = demo_repo("find");
    let out = root.join(".cm/wiki");
    wiki::plan(&root, &out).expect("plan");
    wiki::apply(&out, "build-and-test", &page_markdown("Build and Test")).expect("apply build");
    wiki::apply(&out, "overview", &page_markdown("Project Overview")).expect("apply overview");

    let matches = wiki::find(&out, "build test commands", 5).expect("find");
    assert_eq!(
        matches.first().map(|item| item.page.as_str()),
        Some("build-and-test")
    );
    assert!(!matches[0].headings.is_empty());

    fs::remove_dir_all(&root).ok();
}

#[test]
fn wiki_route_auto_selects_none_excerpt_or_full() {
    let root = demo_repo("route");
    let out = root.join(".cm/wiki");
    wiki::plan(&root, &out).expect("plan");
    wiki::apply(&out, "build-and-test", &page_markdown("Build and Test")).expect("apply build");
    wiki::apply(&out, "overview", &page_markdown("Project Overview")).expect("apply overview");

    let auto = wiki::route(&out, "build test commands", 600, 2, "auto").expect("route auto");
    assert_eq!(auto.mode, "excerpt");
    assert_eq!(auto.page.as_deref(), Some("build-and-test"));
    assert!(auto.text.contains("## Build and test"));

    let small = wiki::route(&out, "build test commands", 600, 2, "small").expect("route small");
    assert_eq!(small.mode, "none");
    assert!(small.text.is_empty());

    let none = wiki::route(&out, "zzzzzz no such topic", 600, 2, "auto").expect("route none");
    assert_eq!(none.mode, "none");

    fs::remove_dir_all(&root).ok();
}
