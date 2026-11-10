//! Generated repository wiki.
//!
//! Wiki pages are human-facing output, not cm memory. They live in a separate
//! output directory and are never loaded by recall unless the caller explicitly
//! writes compact pointer records with [`remember_pages`]. The plan is
//! deterministic; the prose for one page is produced by a model and committed
//! back through `wiki apply`, which checks the page shape and source freshness.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use xxhash_rust::xxh3::xxh3_64;

use crate::explore::{scan_repo, DirStat};
use crate::model::now_ms;

pub const WIKI_DIR: &str = ".cm/wiki";

/// Hard cap for one wiki agent/reviewer process. Wiki generation is expected
/// to use several LLM calls; an unbounded child would otherwise hang the whole
/// batch when a provider stalls. Long-form wiki pages are allowed a long
/// default; `CM_WIKI_AGENT_TIMEOUT_SECS` can override it per run.
const WIKI_AGENT_TIMEOUT_SECS: u64 = 3600;

const MIN_PAGE_CHARS: usize = 1200;
const MIN_PAGE_SECTIONS: usize = 3;
const STRICT_PAGE_CHARS: usize = 2400;
const STRICT_PAGE_SECTIONS: usize = 4;

const WIKI_FILE_EXTS: &[&str] = &[
    "ts", "tsx", "js", "jsx", "mjs", "cjs", "rs", "py", "go", "java", "kt", "c", "h", "cpp", "hpp",
    "cs", "rb", "php", "swift", "scala", "sh", "sql", "md", "json", "toml", "yaml", "yml",
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WikiPage {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub summary: String,
    pub targets: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WikiPlan {
    pub root: String,
    pub out: String,
    pub pages: Vec<WikiPage>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WikiPageState {
    pub page: WikiPage,
    pub file: String,
    pub source_hash: String,
    #[serde(default)]
    pub source_commit: Option<String>,
    pub status: String,
    pub updated_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<WikiReviewState>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WikiReviewState {
    pub verdict: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<u32>,
    #[serde(default)]
    pub issues: Vec<String>,
    #[serde(default)]
    pub notes: Vec<String>,
    pub reviewed_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WikiManifest {
    pub version: u32,
    pub root: String,
    pub generated_at: u64,
    pub pages: Vec<WikiPageState>,
}

#[derive(Clone, Debug, Serialize)]
pub struct WikiGenerateReport {
    pub attempted: usize,
    pub written: usize,
    pub failed: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct WikiStatus {
    pub pages: Vec<WikiPageState>,
    pub written: usize,
    pub planned: usize,
    pub stale: usize,
    pub review_passed: usize,
    pub review_failed: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct WikiCheckReport {
    pub checked: usize,
    pub issues: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct WikiRememberReport {
    pub pages: usize,
    pub remembered: usize,
    pub skipped: Vec<String>,
    pub ids: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct WikiExcerptSection {
    pub heading: String,
    pub line: usize,
    pub score: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct WikiExcerpt {
    pub page: String,
    pub title: String,
    pub file: String,
    pub text: String,
    pub estimated_tokens: usize,
    pub sections: Vec<WikiExcerptSection>,
}

#[derive(Clone, Debug, Serialize)]
pub struct WikiPageMatch {
    pub page: String,
    pub title: String,
    pub file: String,
    pub score: f64,
    pub headings: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct WikiContextPlan {
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    pub text: String,
    pub estimated_tokens: usize,
    pub reasons: Vec<String>,
    pub candidates: Vec<WikiPageMatch>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WikiReview {
    pub verdict: String,
    #[serde(default)]
    pub score: Option<u32>,
    #[serde(default)]
    pub issues: Vec<String>,
    #[serde(default)]
    pub notes: Vec<String>,
}

fn display_path(path: &Path) -> String {
    let text = path.to_string_lossy().replace("\\", "/");
    text.strip_prefix("//?/").unwrap_or(&text).to_string()
}

fn truncate_chars(text: &mut String, max: usize) {
    if text.chars().count() <= max {
        return;
    }
    let mut truncated: String = text.chars().take(max.saturating_sub(1)).collect();
    truncated.push('…');
    *text = truncated;
}

fn page_summary(markdown: &str) -> String {
    let mut summary = String::new();
    let mut in_body = false;
    for raw in markdown.lines() {
        let line = raw.trim();
        if !in_body {
            if line.starts_with("# ") {
                in_body = true;
            }
            continue;
        }
        if line.is_empty() {
            if !summary.is_empty() {
                break;
            }
            continue;
        }
        if line.starts_with('#')
            || line.starts_with("```")
            || line.starts_with('|')
            || line.starts_with("<!--")
        {
            if !summary.is_empty() {
                break;
            }
            continue;
        }
        if !summary.is_empty() {
            summary.push(' ');
        }
        summary.push_str(line);
        if summary.chars().count() >= 420 {
            break;
        }
    }
    if summary.is_empty() {
        summary.push_str("Repository wiki page.");
    }
    truncate_chars(&mut summary, 360);
    summary
}

/// Write one compact pointer memory per reviewed wiki page.
///
/// The page prose stays in the wiki output directory. Each memory records the
/// page summary, file location, and targets, so normal `cm recall`/`brief`
/// retrieval can point an agent at the relevant page without copying the page
/// into the hot memory file.
pub fn remember_pages(
    memory_file: &Path,
    out: &Path,
    page_id: Option<&str>,
    scope: &str,
    replace: bool,
    require_review: bool,
) -> Result<WikiRememberReport> {
    let manifest = load_manifest(out)?;
    let root = PathBuf::from(&manifest.root);
    let cm = crate::api::Cm::open(memory_file)?;
    let existing = if replace { cm.all()? } else { Vec::new() };
    let mut report = WikiRememberReport {
        pages: manifest.pages.len(),
        remembered: 0,
        skipped: Vec::new(),
        ids: Vec::new(),
    };
    for state in &manifest.pages {
        if let Some(selected) = page_id {
            if state.page.id != selected {
                continue;
            }
        }
        if state.status != "written" {
            report
                .skipped
                .push(format!("{}: status is {}", state.page.id, state.status));
            continue;
        }
        if require_review
            && !matches!(
                state.review.as_ref(),
                Some(review) if review.verdict == "pass" && review.issues.is_empty()
            )
        {
            report
                .skipped
                .push(format!("{}: not review-passed", state.page.id));
            continue;
        }
        let mechanical = page_check_issues(out, &state.page.id)?;
        if !mechanical.is_empty() {
            report.skipped.push(format!(
                "{}: mechanical issues: {}",
                state.page.id,
                mechanical.join("; ")
            ));
            continue;
        }
        let full = out.join(&state.file);
        let markdown = fs::read_to_string(&full)
            .with_context(|| format!("read wiki page {}", full.display()))?;
        let summary = page_summary(&markdown);
        let subject = format!("wiki: {}", state.page.title);
        let source = full
            .strip_prefix(&root)
            .map(display_path)
            .unwrap_or_else(|_| display_path(&full));
        let mut text = format!("{summary} Page: {}.", state.file);
        if !state.page.targets.is_empty() {
            let targets: Vec<&str> = state
                .page
                .targets
                .iter()
                .take(6)
                .map(String::as_str)
                .collect();
            text.push_str(" Targets: ");
            text.push_str(&targets.join(", "));
            if state.page.targets.len() > targets.len() {
                text.push_str(", ...");
            }
        }
        truncate_chars(&mut text, 700);
        let supersedes: Vec<String> = existing
            .iter()
            .filter(|memory| {
                memory.scope == scope && memory.kind == "procedure" && memory.subject == subject
            })
            .map(|memory| memory.id.clone())
            .collect();
        let mut input =
            crate::api::NewMemory::new("procedure", scope.to_string(), subject.clone(), text)
                .source(source)
                .confidence(0.9)
                .trust("verified");
        if let Some(commit) = &state.source_commit {
            input = input.source_commit(commit.clone());
        }
        if !supersedes.is_empty() {
            input = input.supersedes(supersedes);
        }
        let result = cm.remember(input)?;
        report.ids.push(result.memory.id);
        report.remembered += 1;
    }
    Ok(report)
}

fn slug(text: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "page".to_string()
    } else {
        out
    }
}

fn page(
    seen: &mut HashSet<String>,
    id: &str,
    title: impl Into<String>,
    kind: &str,
    summary: impl Into<String>,
    targets: Vec<String>,
) -> WikiPage {
    let mut id = slug(id);
    let base = id.clone();
    let mut suffix = 2;
    while !seen.insert(id.clone()) {
        id = format!("{base}-{suffix}");
        suffix += 1;
    }
    WikiPage {
        id,
        title: title.into(),
        kind: kind.to_string(),
        summary: summary.into(),
        targets: targets
            .into_iter()
            .filter(|target| !target.is_empty())
            .collect(),
    }
}

fn find_readme(root: &Path) -> Option<String> {
    for name in ["README.md", "readme.md", "README", "docs/README.md"] {
        if root.join(name).is_file() {
            return Some(name.to_string());
        }
    }
    None
}

fn root_manifests(scan: &crate::explore::RepoScan) -> Vec<String> {
    scan.manifests
        .iter()
        .filter(|path| !path.contains('/'))
        .take(6)
        .cloned()
        .collect()
}

fn is_test_area(path: &str) -> bool {
    path.split('/').any(|part| {
        matches!(
            part.to_ascii_lowercase().as_str(),
            "test" | "tests" | "spec" | "specs" | "__tests__" | "fixtures"
        )
    })
}

fn is_test_file(path: &str) -> bool {
    if is_test_area(path) {
        return true;
    }
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    name.ends_with("_test.rs")
        || name.ends_with("_tests.rs")
        || name.ends_with("_test.py")
        || name.ends_with("_test.go")
        || name.ends_with(".test.ts")
        || name.ends_with(".test.tsx")
        || name.ends_with(".test.js")
        || name.ends_with(".test.jsx")
        || name.ends_with(".spec.ts")
        || name.ends_with(".spec.tsx")
        || name.ends_with(".spec.js")
        || name.ends_with(".spec.jsx")
}

fn test_targets(
    scan: &crate::explore::RepoScan,
    root: &Path,
    manifests: &[String],
) -> Vec<String> {
    let mut files: Vec<String> = scan
        .file_paths
        .iter()
        .filter(|path| is_test_file(path) && root.join(path).is_file())
        .cloned()
        .collect();
    if files.is_empty() {
        return files;
    }
    files.sort_by(|left, right| {
        left.matches('/')
            .count()
            .cmp(&right.matches('/').count())
            .then_with(|| left.cmp(right))
    });
    files.truncate(12);
    files.extend(manifests.iter().take(2).cloned());
    files.dedup();
    files
}

fn module_files(scan: &crate::explore::RepoScan, root: &Path, area: &str) -> Vec<String> {
    let prefix = format!("{}/", area.trim_end_matches('/'));
    let mut files: Vec<String> = scan
        .file_paths
        .iter()
        .filter(|file| {
            file.starts_with(&prefix)
                && is_wiki_file(file)
                && !is_test_area(file)
                && root.join(file.as_str()).is_file()
        })
        .cloned()
        .collect();
    if files.is_empty() {
        collect_area_files(root, &root.join(area), &mut files, 0);
    }
    files.sort_by(|left, right| {
        file_score(left, area)
            .cmp(&file_score(right, area))
            // Within a rank, prefer the files that carry the most behavior.
            // Alphabetical order alone tends to spend the target budget on
            // small barrels and misses the central implementation files.
            .then_with(|| file_size(root, right).cmp(&file_size(root, left)))
            .then_with(|| left.matches('/').count().cmp(&right.matches('/').count()))
            .then_with(|| left.cmp(right))
    });
    files.dedup();
    // Large modules get a wider read set so the page can cover public entry
    // points, persistence, settings, and runtime, not just the first dozen
    // files ranked by name.
    files.truncate(if files.len() > 50 {
        24
    } else if files.len() > 30 {
        18
    } else {
        12
    });
    // A module inside a package must also expose the package-root surface:
    // the export map and the source barrel decide what is public. Those files
    // sit above the module directory, so the area scan cannot discover them.
    if let Some(rest) = area.strip_prefix("packages/") {
        if let Some(package) = rest.split('/').next() {
            for candidate in [
                format!("packages/{package}/src/index.ts"),
                format!("packages/{package}/package.json"),
            ] {
                if root.join(&candidate).is_file() && !files.iter().any(|file| file == &candidate) {
                    files.push(candidate);
                }
            }
        }
    }
    if files.is_empty() {
        vec![area.to_string()]
    } else {
        files
    }
}

fn readme_title(dir: &Path) -> Option<String> {
    let path = ["README.md", "readme.md", "README"]
        .iter()
        .map(|name| dir.join(name))
        .find(|path| path.is_file())?;
    let text = fs::read_to_string(path).ok()?;
    text.lines()
        .find_map(|line| line.trim().strip_prefix("# ").map(str::trim))
        .filter(|title| !title.is_empty())
        .map(str::to_string)
}

fn manifest_title(dir: &Path) -> Option<String> {
    if let Ok(text) = fs::read_to_string(dir.join("package.json")) {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(name) = value.get("name").and_then(|name| name.as_str()) {
                return Some(name.to_string());
            }
        }
    }
    for manifest in ["Cargo.toml", "pyproject.toml"] {
        let Ok(text) = fs::read_to_string(dir.join(manifest)) else {
            continue;
        };
        let wanted = if manifest == "Cargo.toml" {
            &["[package]"][..]
        } else {
            &["[project]", "[tool.poetry]"][..]
        };
        let mut in_section = false;
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                in_section = wanted.contains(&line);
                continue;
            }
            if !in_section {
                continue;
            }
            let Some(rest) = line.strip_prefix("name") else {
                continue;
            };
            let Some((_, value)) = rest.split_once('=') else {
                continue;
            };
            let name = value.trim().trim_matches(['"', '\'']);
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    if let Ok(text) = fs::read_to_string(dir.join("go.mod")) {
        if let Some(module) = text
            .lines()
            .find_map(|line| line.trim().strip_prefix("module "))
        {
            let name = module.rsplit('/').next().unwrap_or(module).trim();
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

fn module_title(root: &Path, area: &str) -> String {
    let dir = root.join(area);
    // A module page often targets `src` or `src/...`, while the package
    // manifest lives one or more levels above it. Walk up to the repository
    // root and use the nearest package name. Several areas of one package
    // would otherwise share the same title, so append the area inside the
    // package (`core`, `api`, `examples/extensions`, ...).
    let mut current = dir.as_path();
    loop {
        if let Some(name) = manifest_title(current) {
            let short = name.rsplit('/').next().unwrap_or(&name);
            let package_rel = current
                .strip_prefix(root)
                .ok()
                .map(|path| path.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            let mut suffix = area
                .strip_prefix(package_rel.trim_matches('/'))
                .unwrap_or(area)
                .trim_matches('/')
                .to_string();
            if let Some(rest) = suffix.strip_prefix("src/") {
                suffix = rest.to_string();
            } else if suffix == "src" {
                suffix.clear();
            }
            return if suffix.is_empty() {
                format!("Module: {short}")
            } else {
                format!("Module: {short} / {suffix}")
            };
        }
        if current == root {
            break;
        }
        let Some(parent) = current.parent() else {
            break;
        };
        current = parent;
    }
    if let Some(title) = readme_title(&dir) {
        return title;
    }
    let label = area.rsplit('/').next().unwrap_or(area);
    format!("Module: {label}")
}

fn is_wiki_file(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| WIKI_FILE_EXTS.contains(&extension.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

fn file_score(path: &str, area: &str) -> u8 {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    let path_lower = path.to_ascii_lowercase();
    // These nested barrels are subsystem entry points, not incidental
    // re-exports; keep them visible in large module pages.
    if path_lower.ends_with("/tools/index.ts")
        || path_lower.ends_with("/tools/index.tsx")
        || path_lower.ends_with("/tools/renderers/index.ts")
        || path_lower.ends_with("/export-html/index.ts")
    {
        return 2;
    }
    let parent = path
        .rsplit_once('/')
        .map(|(parent, _)| parent)
        .unwrap_or("");
    let at_area_root = parent == area.trim_end_matches('/');
    match name.as_str() {
        "index.ts" | "index.tsx" => {
            if at_area_root {
                0
            } else {
                5
            }
        }
        "lib.rs" | "mod.rs" | "main.rs" | "main.ts" => {
            if at_area_root {
                0
            } else {
                3
            }
        }
        "types.ts" | "types.rs" | "schema.ts" | "sdk.ts" | "package.json" => 1,
        "system-prompt.ts" | "messages.ts" | "bash-executor.ts" | "auth-storage.ts"
        | "auth-guidance.ts" | "event-bus.ts" | "readme.md" | "node.ts" | "mod.ts" => 2,
        // Generated HTML templates are output assets, not core behavior.
        "template.js" => 6,
        _ if name.ends_with(".md") => 6,
        _ => 4,
    }
}

fn file_size(root: &Path, path: &str) -> u64 {
    fs::metadata(root.join(path))
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}

fn collect_area_files(root: &Path, dir: &Path, out: &mut Vec<String>, depth: usize) {
    if depth > 4 || out.len() >= 60 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            if is_test_area(&name) {
                continue;
            }
            collect_area_files(root, &path, out, depth + 1);
            continue;
        }
        if !path.is_file() {
            continue;
        }
        let extension = path
            .extension()
            .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if !WIKI_FILE_EXTS.contains(&extension.as_str()) {
            continue;
        }
        if let Ok(relative) = path.strip_prefix(root) {
            out.push(relative.to_string_lossy().replace('\\', "/"));
        }
    }
}

fn flow_targets(scan: &crate::explore::RepoScan, root: &Path) -> Vec<String> {
    let mut targets = Vec::new();
    for path in &scan.file_paths {
        if path.starts_with("examples/")
            || path.contains("/examples/")
            || !root.join(path).is_file()
        {
            continue;
        }
        let name = path.rsplit('/').next().unwrap_or(path);
        if matches!(
            name,
            "main.ts"
                | "main.rs"
                | "cli.ts"
                | "rpc-entry.ts"
                | "agent.ts"
                | "agent-loop.ts"
                | "agent-session.ts"
                | "session-manager.ts"
                | "compaction.ts"
        ) || path.ends_with("/tools/index.ts")
        {
            targets.push(path.clone());
        }
    }
    targets.extend(
        scan.entrypoints
            .iter()
            .filter(|path| {
                !path.starts_with("examples/")
                    && !path.contains("/examples/")
                    && root.join(path).is_file()
            })
            .take(4)
            .cloned(),
    );
    let mut seen = HashSet::new();
    targets.retain(|target| seen.insert(target.clone()));
    targets.truncate(12);
    targets
}

fn module_targets(scan: &crate::explore::RepoScan, max_modules: usize) -> Vec<DirStat> {
    let mut areas: Vec<DirStat> = scan
        .areas
        .iter()
        .filter(|area| area.sources >= 5 && !is_test_area(&area.path))
        .cloned()
        .collect();
    if areas.is_empty() {
        areas = scan
            .top_dirs
            .iter()
            .filter(|area| area.sources > 0 && !is_test_area(&area.path))
            .cloned()
            .collect();
    }
    areas.truncate(max_modules.max(1));
    areas
}

/// Default upper bound for module pages. Large monorepos get broad coverage,
/// while `plan_with` lets a caller trade breadth for generation cost.
pub const DEFAULT_MAX_MODULES: usize = 24;

/// Build the deterministic page plan and manifest with the default module
/// budget. Existing page states are preserved so a plan refresh does not throw
/// away generated prose.
pub fn plan(root: &Path, out: &Path) -> Result<WikiPlan> {
    plan_with(root, out, DEFAULT_MAX_MODULES)
}

pub fn plan_with(root: &Path, out: &Path, max_modules: usize) -> Result<WikiPlan> {
    let root = root
        .canonicalize()
        .with_context(|| format!("canonicalize wiki root {}", root.display()))?;
    if !root.is_dir() {
        bail!("wiki root is not a directory: {}", root.display());
    }
    let scan = scan_repo(&root);
    let manifests = root_manifests(&scan);
    let readme = find_readme(&root);
    let mut seen = HashSet::new();
    let mut pages = Vec::new();

    let mut overview_targets = Vec::new();
    if let Some(readme) = &readme {
        overview_targets.push(readme.clone());
    }
    overview_targets.extend(manifests.iter().cloned());
    if overview_targets.is_empty() {
        overview_targets.push(".".to_string());
    }
    pages.push(page(
        &mut seen,
        "overview",
        "Project Overview",
        "overview",
        "Explain what this repository is, what problem it solves, its stack, and the main runtime surfaces. Do not write a file inventory.",
        overview_targets,
    ));

    let mut architecture_targets: Vec<String> = manifests.iter().take(4).cloned().collect();
    architecture_targets.extend(
        scan.entrypoints
            .iter()
            .filter(|path| root.join(path).is_file())
            .take(8)
            .cloned(),
    );
    let mut entry_files: Vec<String> = scan
        .file_paths
        .iter()
        .filter(|path| {
            root.join(path).is_file()
                && (path.ends_with("/src/index.ts")
                    || path.ends_with("/src/index.tsx")
                    || path.ends_with("/src/main.ts")
                    || path.ends_with("/src/main.rs")
                    || path.ends_with("/src/lib.rs")
                    || path.ends_with("/package.json")
                    || path.ends_with("/Cargo.toml"))
        })
        .cloned()
        .collect();
    let entry_rank = |path: &str| {
        if path.ends_with("/src/index.ts") || path.ends_with("/src/index.tsx") {
            0
        } else if path.ends_with("/src/main.ts")
            || path.ends_with("/src/main.rs")
            || path.ends_with("/src/lib.rs")
        {
            1
        } else if path.ends_with("/package.json") || path.ends_with("/Cargo.toml") {
            2
        } else {
            3
        }
    };
    entry_files.sort_by(|left, right| {
        entry_rank(left)
            .cmp(&entry_rank(right))
            .then_with(|| left.matches('/').count().cmp(&right.matches('/').count()))
            .then_with(|| left.cmp(right))
    });
    architecture_targets.extend(entry_files.into_iter().take(12));
    let mut seen_targets = HashSet::new();
    architecture_targets.retain(|target| seen_targets.insert(target.clone()));
    architecture_targets.truncate(20);
    if !architecture_targets.is_empty() {
        pages.push(page(
            &mut seen,
            "architecture",
            "Architecture",
            "architecture",
            "Explain the package boundaries, major components, data flow, and how the entry points reach storage, network, and UI. Include a Mermaid diagram.",
            architecture_targets,
        ));
    }

    if !manifests.is_empty() {
        pages.push(page(
            &mut seen,
            "build-and-test",
            "Build and Test",
            "operations",
            "Document the build, test, check, release, and required environment commands. Prefer exact manifest scripts over generic language advice.",
            manifests.clone(),
        ));
    }

    let testing_targets = test_targets(&scan, &root, &manifests);
    if !testing_targets.is_empty() {
        pages.push(page(
            &mut seen,
            "testing",
            "Testing Strategy",
            "testing",
            "Explain the test layers, fixture strategy, isolation, what behavior is covered, how to run a focused test, and which important behavior is not covered.",
            testing_targets,
        ));
    }

    let flow_targets = flow_targets(&scan, &root);
    if !flow_targets.is_empty() {
        pages.push(page(
            &mut seen,
            "key-flows",
            "Key Runtime Flows",
            "flows",
            "Trace the main runtime flows from the detected entry points through the core modules. Explain inputs, outputs, state, and failure behavior.",
            flow_targets,
        ));
    }

    for area in module_targets(&scan, max_modules) {
        let title = module_title(&root, &area.path);
        let mut targets = module_files(&scan, &root, &area.path);
        if !targets.iter().any(|target| target == &area.path) {
            targets.push(area.path.clone());
        }
        pages.push(page(
            &mut seen,
            &format!("module-{}", area.path),
            title,
            "module",
            format!(
                "Explain the responsibility, public interfaces, key files/symbols, dependencies, state, and failure modes of {}.",
                area.path
            ),
            targets,
        ));
    }

    let out = out.to_path_buf();
    let existing = load_manifest(&out).ok();
    let mut states = Vec::with_capacity(pages.len());
    for page in pages {
        let old = existing
            .as_ref()
            .and_then(|manifest| manifest.pages.iter().find(|state| state.page.id == page.id));
        let (file, source_hash, source_commit, status, updated_at) = match old {
            Some(old) => {
                let metadata_changed = old.page != page;
                let status = if old.status == "written" && metadata_changed {
                    "stale".to_string()
                } else {
                    old.status.clone()
                };
                (
                    old.file.clone(),
                    old.source_hash.clone(),
                    old.source_commit.clone(),
                    status,
                    old.updated_at,
                )
            }
            None => (
                format!("{}.md", page.id),
                String::new(),
                None,
                "planned".to_string(),
                0,
            ),
        };
        let review = old
            .filter(|old| old.page == page && old.status == "written")
            .and_then(|old| old.review.clone());
        states.push(WikiPageState {
            page,
            file,
            source_hash,
            source_commit,
            status,
            updated_at,
            review,
        });
    }
    fs::create_dir_all(&out)?;
    let manifest = WikiManifest {
        version: 1,
        root: root.display().to_string(),
        generated_at: now_ms(),
        pages: states,
    };
    save_manifest(&out, &manifest)?;
    render_index(&out, &manifest)?;
    Ok(WikiPlan {
        root: display_path(&root),
        out: display_path(&out),
        pages: manifest.pages.into_iter().map(|state| state.page).collect(),
    })
}

pub fn load_manifest(out: &Path) -> Result<WikiManifest> {
    let text = fs::read_to_string(out.join("manifest.json"))
        .with_context(|| format!("read {}", out.join("manifest.json").display()))?;
    Ok(serde_json::from_str(&text)?)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn save_manifest(out: &Path, manifest: &WikiManifest) -> Result<()> {
    fs::create_dir_all(out)?;
    write_atomic(
        &out.join("manifest.json"),
        &serde_json::to_vec_pretty(manifest)?,
    )
}

fn symbol_name(line: &str) -> Option<String> {
    const PREFIXES: &[&str] = &[
        "pub async unsafe fn ",
        "pub async fn ",
        "pub unsafe fn ",
        "pub fn ",
        "pub struct ",
        "pub enum ",
        "pub trait ",
        "pub type ",
        "pub const ",
        "pub mod ",
        "export async function ",
        "export default function ",
        "export function ",
        "export class ",
        "export interface ",
        "export type ",
        "export const ",
        "public static class ",
        "public class ",
        "public interface ",
        "function ",
        "class ",
        "interface ",
        "def ",
        "func ",
        "type ",
    ];
    let line = line.trim_start();
    for prefix in PREFIXES {
        if let Some(rest) = line.strip_prefix(prefix) {
            let name: String = rest
                .chars()
                .take_while(|ch| ch.is_alphanumeric() || *ch == '_')
                .collect();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    None
}

fn evidence_map(root: &Path, targets: &[String]) -> String {
    let mut out = String::new();
    let mut total = 0usize;
    for target in targets {
        if total >= 240 {
            break;
        }
        let path = root.join(target);
        if !path.is_file() {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let mut count = 0usize;
        for (index, line) in text.lines().enumerate() {
            if count >= 20 || total >= 240 {
                break;
            }
            if let Some(name) = symbol_name(line) {
                out.push_str(&format!("- {target}:{} `{name}`\n", index + 1));
                count += 1;
                total += 1;
            }
        }
    }
    out
}

/// Line-numbered source excerpts so a no-tools agent can write and verify
/// citations without entering a read-tool loop. Large files are truncated;
/// the agent can still use the EVIDENCE symbol lines.
fn source_excerpts(root: &Path, targets: &[String]) -> String {
    const MAX_TOTAL_CHARS: usize = 60_000;
    const MAX_PER_FILE_LINES: usize = 140;
    let mut out = String::new();
    let mut total = 0usize;
    for target in targets {
        if total >= MAX_TOTAL_CHARS {
            break;
        }
        let path = root.join(target);
        if !path.is_file() {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        out.push_str(&format!("\n--- {target} ---\n"));
        total += target.len() + 10;
        for (index, line) in text.lines().enumerate() {
            if index >= MAX_PER_FILE_LINES || total >= MAX_TOTAL_CHARS {
                out.push_str("... [truncated]\n");
                break;
            }
            // `pir print` expands `@file` references in its prompt input.
            // Source excerpts can contain scoped package names such as
            // `@earendil-works/...`; use a fullwidth at-sign in the prompt so
            // the model sees the text but the print host does not treat it as
            // an attachment reference.
            let row = format!("{}: {}\n", index + 1, line.replace('@', "＠"));
            total += row.len();
            out.push_str(&row);
        }
    }
    out
}

fn required_sections(kind: &str) -> &'static str {
    match kind {
        "overview" => {
            "What it is; Problem and users; Main components; Runtime surfaces; Getting started; Boundaries"
        }
        "architecture" => {
            "System context; Component boundaries; Data flow; Storage and state; Concurrency and failure; Extension points"
        }
        "operations" => "Prerequisites; Build; Test; Run; Release; Troubleshooting",
        "testing" => {
            "Test layers; Layout and fixtures; Running tests; Isolation and determinism; Coverage gaps; Adding a test"
        }
        "flows" => "Trigger; Step-by-step trace; State changes; Failure and recovery; Observability",
        "module" => {
            "Responsibility; Public interfaces; Key types and data; Dependencies and external coupling; Configuration and tooling; State and lifecycle; Failure modes"
        }
        _ => "Purpose; Interfaces; Data flow; Failure behavior",
    }
}

pub fn prompt(out: &Path, page_id: &str) -> Result<String> {
    let manifest = load_manifest(out)?;
    let state = manifest
        .pages
        .iter()
        .find(|state| state.page.id == page_id)
        .ok_or_else(|| anyhow!("unknown wiki page {page_id:?}"))?;
    let targets = state.page.targets.join(", ");
    let word_target = if state.page.targets.len() >= 24 {
        "1500-2500"
    } else if state.page.targets.len() >= 16 {
        "1200-2000"
    } else {
        "800-1500"
    };
    let evidence = evidence_map(Path::new(&manifest.root), &state.page.targets);
    let evidence_note = if evidence.is_empty() {
        String::new()
    } else {
        format!("\nEVIDENCE (mechanically extracted; verify before citing):\n{evidence}")
    };
    let excerpts = source_excerpts(Path::new(&manifest.root), &state.page.targets);
    let excerpt_note = if excerpts.is_empty() {
        String::new()
    } else {
        format!("\nSOURCE EXCERPTS (line-numbered; cite these exact lines when possible):\n{excerpts}")
    };
    let stale_note = if state.status == "stale" {
        "\nThis page is stale. Rewrite it from the current TARGETS; do not consult any previous wiki output.\n"
            .to_string()
    } else {
        String::new()
    };
    Ok(format!(
        "You are writing one source-grounded page of a repository wiki. This is not an outline and not a file inventory.\n\
         You are read-only: do not create, edit, delete, or rename any repository file. Do not run builds, tests, installs, or formatters. Return Markdown on stdout only.\n\
         \n\
         PAGE: {title}\n\
         KIND: {kind}\n\
         PURPOSE: {summary}\n\
         ROOT: {root}\n\
         TARGETS: {targets}\n\
         {evidence_note}\
         {excerpt_note}\
         {stale_note}\
         \n\
         Method:\n\
         - Read every TARGETS entry. Follow a direct import, caller, or test only when it is needed to explain behavior.\n\
         - EVIDENCE lists mechanically extracted declaration lines. Prefer those exact `path:line` values; if you cite anything else, read the line first and verify its content.\n\
         - Do not scan the whole repository. Do not repeat the target list as a file inventory.\n\
         - Ground every non-obvious claim in an existing path or symbol. Every citation must use a full repository-relative path with at least one `/` (except a root file such as `README.md`), one exact declaration or operation line, and no `start-end` range such as `6-14`. Repeat the full path for every citation; never use bare `:NN` shorthand. If a construct spans lines, cite its declaration line.\n\
         - When a sentence or table row names several exports, cite each export's declaration line separately. A single shared citation is not enough.\n\
         - For a throw, return, or rejection claim, cite the exact throw/return line, not the enclosing guard, catch, function header, or loop header. If several branches share a behavior, cite each branch's operation line or state the branch boundary.\n\
         - For every TARGET file, name its primary exported type/function or explicitly state why it is out of scope; do not silently omit a target.\n\
         - Include a Dependencies section for module pages: external packages, internal layers, and any UI/theme coupling that exists.\n\
         - Include automatic compaction, retry, and overflow/recovery paths when the source has them.\n\
         - For package/workspace/version claims, cite the exact `package.json`/`Cargo.toml` declaration. Workspace membership does not establish publication; package consumption requires a dependency declaration; do not infer either from prose.\n\
         - If the source does not establish something, write \"Not established from the available source\" instead of guessing.\n\
         \n\
         Required shape:\n\
         - Start with `# {title}` as the only level-1 heading.\n\
         - Use 4-8 `##` sections. Suggested sections for this kind: {sections}.\n\
         - Target {word_target} words of prose. Use bullets or tables only for contracts, commands, or invariants.\n\
         - Architecture and flow pages must include one Mermaid diagram that matches the actual code paths.\n\
         \n\
         Before returning, self-check:\n\
         - Every named class, method, and service is owned by the file you place it in; check the actual class boundary.\n\
         - Public vs internal labels follow the package export map/barrel files, not intuition.\n\
         - Words like \"always\", \"never\", or \"independent\" have direct evidence; otherwise qualify the claim. The same applies to \"only\", \"all\", and \"every\".\n\
         - Every citation repeats the full repository-relative path; no bare `:NN` shorthand remains.\n\
         - Every multi-symbol table row or sentence cites each symbol's own declaration line.\n\
         - Every Mermaid edge is an actual call/import relationship, not a conceptual or transitive grouping.\n\
         - The prose stays within the stated word target; remove secondary detail instead of adding sections.\n\
         - Every citation line is the declaration of the named symbol or the exact operation being described. Never cite a call site when claiming a declaration, and never cite a line that merely mentions the symbol in passing.\n\
         - Verify the exact line number immediately before citing it. If the line is blank, a comment, an import brace, or a different declaration, find the real line.\n\
         - When claiming a method throws, returns early, or rejects input, enumerate every guard and any exception path that can bypass it; do not generalize one branch to the whole method.\n\
         - For module pages, cover the package-root export surface and every TARGET file in the source table, including renderer registries, HTML-export helpers, queue mutators, and extension command/runtime context types when the targets contain them.\n\
         - State whether a service is reused or newly constructed when describing construction.\n\
         - Do not claim what a template substitution does across multiple files; verify each placeholder.\n\
         - Output only Markdown, with no preamble, analysis, or outer code fence.\n",
        title = state.page.title,
        kind = state.page.kind,
        summary = state.page.summary,
        root = display_path(Path::new(&manifest.root)),
        targets = targets,
        sections = required_sections(&state.page.kind),
        word_target = word_target,
    ))
}

fn is_cjk(ch: char) -> bool {
    matches!(
        ch,
        '\u{3400}'..='\u{4DBF}'
            | '\u{4E00}'..='\u{9FFF}'
            | '\u{F900}'..='\u{FAFF}'
            | '\u{3040}'..='\u{30FF}'
            | '\u{AC00}'..='\u{D7AF}'
    )
}

fn query_terms(query: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut ascii = String::new();
    for ch in query.chars() {
        if ch.is_ascii_alphanumeric() {
            ascii.push(ch.to_ascii_lowercase());
        } else {
            if ascii.len() >= 2 {
                terms.push(ascii.clone());
            }
            ascii.clear();
        }
    }
    if ascii.len() >= 2 {
        terms.push(ascii);
    }
    let cjk: Vec<char> = query.chars().filter(|ch| is_cjk(*ch)).collect();
    for pair in cjk.windows(2) {
        terms.push(pair.iter().collect());
    }
    terms.sort();
    terms.dedup();
    terms
}

fn section_score(terms: &[String], heading: &str, body: &str) -> f64 {
    if terms.is_empty() {
        return 0.0;
    }
    let heading = heading.to_lowercase();
    let body = body.to_lowercase();
    let mut score = 0.0;
    let mut heading_hits = 0usize;
    let mut body_hits = 0usize;
    for term in terms {
        if heading.contains(term) {
            heading_hits += 1;
            score += 10.0;
        }
        let count = body.matches(term).count().min(5);
        if count > 0 {
            body_hits += 1;
            score += count as f64;
        }
    }
    if score == 0.0 {
        return 0.0;
    }
    score += (heading_hits * 2 + body_hits) as f64;
    score / (1.0 + (body.split_whitespace().count() as f64).sqrt() / 20.0)
}

fn truncate_to_chars(text: &mut String, max_chars: usize) {
    if text.chars().count() <= max_chars {
        return;
    }
    let mut truncated: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    truncated.push('…');
    *text = truncated;
}

/// Return only the page introduction plus the H2 sections most relevant to a
/// query. This is the low-token alternative to feeding an agent the whole page.
pub fn excerpt(
    out: &Path,
    page_id: &str,
    query: &str,
    budget: usize,
    max_sections: usize,
) -> Result<WikiExcerpt> {
    let manifest = load_manifest(out)?;
    let state = manifest
        .pages
        .iter()
        .find(|state| state.page.id == page_id)
        .ok_or_else(|| anyhow!("unknown wiki page {page_id:?}"))?;
    let full = out.join(&state.file);
    let markdown =
        fs::read_to_string(&full).with_context(|| format!("read wiki page {}", full.display()))?;
    let mut title = state.page.title.clone();
    let mut intro = String::new();
    let mut sections: Vec<(String, usize, String)> = Vec::new();
    let mut current: Option<(String, usize, String)> = None;
    for (index, line) in markdown.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("# ") && title == state.page.title {
            title = trimmed.trim_start_matches("# ").trim().to_string();
            continue;
        }
        if let Some(heading) = trimmed.strip_prefix("## ") {
            if let Some(section) = current.take() {
                sections.push(section);
            }
            current = Some((heading.trim().to_string(), index + 1, String::new()));
            continue;
        }
        if let Some(section) = current.as_mut() {
            section.2.push_str(line);
            section.2.push('\n');
        } else if !title.is_empty() {
            intro.push_str(line);
            intro.push('\n');
        }
    }
    if let Some(section) = current.take() {
        sections.push(section);
    }
    let terms = query_terms(query);
    let mut ranked: Vec<(f64, &(String, usize, String))> = sections
        .iter()
        .map(|section| (section_score(&terms, &section.0, &section.2), section))
        .collect();
    ranked.sort_by(|left, right| {
        right
            .0
            .partial_cmp(&left.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.1 .1.cmp(&right.1 .1))
    });
    let mut selected: Vec<&(String, usize, String)> = ranked
        .iter()
        .filter(|(score, _)| *score > 0.0)
        .map(|(_, section)| *section)
        .take(max_sections.max(1))
        .collect();
    if selected.is_empty() {
        selected.extend(sections.iter().take(max_sections.max(1)));
    }
    let mut intro = intro.trim().to_string();
    truncate_to_chars(&mut intro, 600);
    let mut text = format!("# {title}\n\n{intro}\n");
    let mut chosen = Vec::new();
    let section_allowance = (budget / max_sections.max(1)).max(120);
    for (heading, line, body) in selected {
        let mut body = body.trim().to_string();
        truncate_to_chars(&mut body, section_allowance.saturating_mul(4));
        let separator = if text.ends_with('\n') { "\n" } else { "\n\n" };
        let candidate = format!("{separator}## {heading}\n{body}\n");
        let used = crate::model::estimate_tokens(&text);
        if used + crate::model::estimate_tokens(&candidate) > budget {
            let room = budget.saturating_sub(used).saturating_mul(4);
            if room < 120 {
                break;
            }
            truncate_to_chars(&mut body, room);
        }
        text.push_str(&format!("{separator}## {heading}\n{body}\n"));
        chosen.push(WikiExcerptSection {
            heading: heading.clone(),
            line: *line,
            score: ranked
                .iter()
                .find(|(_, section)| section.1 == *line)
                .map(|(score, _)| *score)
                .unwrap_or(0.0),
        });
    }
    let estimated_tokens = crate::model::estimate_tokens(&text);
    Ok(WikiExcerpt {
        page: state.page.id.clone(),
        title,
        file: state.file.clone(),
        text,
        estimated_tokens,
        sections: chosen,
    })
}

/// Rank wiki pages for a query using title, summary, and H2 section text.
/// This is the page-routing step before [`excerpt`].
pub fn find(out: &Path, query: &str, limit: usize) -> Result<Vec<WikiPageMatch>> {
    let manifest = load_manifest(out)?;
    let terms = query_terms(query);
    let mut matches = Vec::new();
    for state in &manifest.pages {
        if state.status != "written" {
            continue;
        }
        let full = out.join(&state.file);
        let Ok(markdown) = fs::read_to_string(&full) else {
            continue;
        };
        let mut sections: Vec<(String, String)> = Vec::new();
        let mut current: Option<(String, String)> = None;
        for line in markdown.lines() {
            let trimmed = line.trim_start();
            if let Some(heading) = trimmed.strip_prefix("## ") {
                if let Some(section) = current.take() {
                    sections.push(section);
                }
                current = Some((heading.trim().to_string(), String::new()));
                continue;
            }
            if let Some(section) = current.as_mut() {
                section.1.push_str(line);
                section.1.push('\n');
            }
        }
        if let Some(section) = current.take() {
            sections.push(section);
        }
        let mut scored: Vec<(f64, String)> = sections
            .iter()
            .map(|(heading, body)| (section_score(&terms, heading, body), heading.clone()))
            .collect();
        scored.sort_by(|left, right| {
            right
                .0
                .partial_cmp(&left.0)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let body_score: f64 = scored.iter().take(3).map(|(score, _)| *score).sum();
        let title_hits = terms
            .iter()
            .filter(|term| state.page.title.to_lowercase().contains(*term))
            .count() as f64
            * 4.0;
        let summary_hits = terms
            .iter()
            .filter(|term| state.page.summary.to_lowercase().contains(*term))
            .count() as f64;
        let score = body_score + title_hits + summary_hits;
        if score > 0.0 {
            matches.push(WikiPageMatch {
                page: state.page.id.clone(),
                title: state.page.title.clone(),
                file: state.file.clone(),
                score,
                headings: scored
                    .into_iter()
                    .filter(|(score, _)| *score > 0.0)
                    .take(3)
                    .map(|(_, heading)| heading)
                    .collect(),
            });
        }
    }
    matches.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    matches.truncate(limit.max(1));
    Ok(matches)
}

fn is_generic_heading(heading: &str) -> bool {
    let heading = heading.to_lowercase();
    [
        "responsibility",
        "public interfaces",
        "key types",
        "source map",
        "source table",
        "dependencies",
        "configuration",
        "state and lifecycle",
        "state, lifecycle",
        "failure modes",
        "observability",
        "out-of-scope",
        "key files",
    ]
    .iter()
    .any(|generic| heading.contains(generic))
}

/// Choose the least expensive useful wiki context for a query:
/// no wiki for small tasks, an excerpt when a specific section matches, or one
/// full page when only generic sections are relevant.
pub fn route(
    out: &Path,
    query: &str,
    budget: usize,
    max_sections: usize,
    task_size: &str,
) -> Result<WikiContextPlan> {
    let candidates = find(out, query, 5)?;
    let mut reasons = Vec::new();
    let Some(top) = candidates.first() else {
        reasons.push("no wiki page matched the query".to_string());
        return Ok(WikiContextPlan {
            mode: "none".to_string(),
            page: None,
            title: None,
            file: None,
            text: String::new(),
            estimated_tokens: 0,
            reasons,
            candidates,
        });
    };
    reasons.push(format!("top page {} scored {:.2}", top.page, top.score));
    if task_size == "small" {
        reasons.push("task_size=small: source reading is cheaper than wiki".to_string());
        return Ok(WikiContextPlan {
            mode: "none".to_string(),
            page: Some(top.page.clone()),
            title: Some(top.title.clone()),
            file: Some(top.file.clone()),
            text: String::new(),
            estimated_tokens: 0,
            reasons,
            candidates,
        });
    }
    if top.score < 20.0 {
        reasons.push("page relevance is too low to justify wiki context".to_string());
        return Ok(WikiContextPlan {
            mode: "none".to_string(),
            page: Some(top.page.clone()),
            title: Some(top.title.clone()),
            file: Some(top.file.clone()),
            text: String::new(),
            estimated_tokens: 0,
            reasons,
            candidates,
        });
    }
    let excerpt = excerpt(out, &top.page, query, budget, max_sections)?;
    let terms = query_terms(query);
    let specific = excerpt
        .sections
        .iter()
        .any(|section| !is_generic_heading(&section.heading));
    let heading_match = excerpt.sections.iter().any(|section| {
        let heading = section.heading.to_lowercase();
        terms.iter().any(|term| heading.contains(term))
    });
    if specific && heading_match {
        reasons.push(format!(
            "specific section matched: {}",
            excerpt
                .sections
                .first()
                .map(|section| section.heading.as_str())
                .unwrap_or("")
        ));
        return Ok(WikiContextPlan {
            mode: "excerpt".to_string(),
            page: Some(excerpt.page),
            title: Some(excerpt.title),
            file: Some(excerpt.file),
            text: excerpt.text,
            estimated_tokens: excerpt.estimated_tokens,
            reasons,
            candidates,
        });
    }
    let full = out.join(&top.file);
    let markdown =
        fs::read_to_string(&full).with_context(|| format!("read wiki page {}", full.display()))?;
    reasons
        .push("only generic sections matched; one full page keeps the needed context".to_string());
    Ok(WikiContextPlan {
        mode: "full".to_string(),
        page: Some(top.page.clone()),
        title: Some(top.title.clone()),
        file: Some(top.file.clone()),
        estimated_tokens: crate::model::estimate_tokens(&markdown),
        text: markdown,
        reasons,
        candidates,
    })
}

fn prose_lines(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut fence = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            fence = !fence;
            continue;
        }
        if !fence {
            out.push(line);
        }
    }
    out
}

fn normalized_title(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

pub fn apply(out: &Path, page_id: &str, markdown: &str) -> Result<WikiPageState> {
    let mut manifest = load_manifest(out)?;
    let index = manifest
        .pages
        .iter()
        .position(|state| state.page.id == page_id)
        .ok_or_else(|| anyhow!("unknown wiki page {page_id:?}"))?;
    let markdown = markdown.trim();
    if !markdown.starts_with("# ") {
        bail!("wiki page must start with a level-1 heading, not preamble");
    }
    let lines = prose_lines(markdown);
    let headings: Vec<&str> = lines
        .iter()
        .filter_map(|line| line.trim_start().strip_prefix("# "))
        .collect();
    if headings.len() != 1 {
        bail!("wiki page must contain exactly one level-1 heading, not an outline");
    }
    let page = manifest.pages[index].page.clone();
    if normalized_title(headings[0]) != normalized_title(&page.title) {
        bail!(
            "wiki page heading {:?} does not match planned title {:?}",
            headings[0].trim(),
            page.title
        );
    }
    let sections = lines
        .iter()
        .filter(|line| line.trim_start().starts_with("## "))
        .count();
    if markdown.chars().count() < MIN_PAGE_CHARS || sections < MIN_PAGE_SECTIONS {
        bail!(
            "wiki page must be at least {MIN_PAGE_CHARS} chars with {MIN_PAGE_SECTIONS} sections, not an outline"
        );
    }
    let file = format!("{}.md", page.id);
    fs::create_dir_all(out)?;
    write_atomic(&out.join(&file), format!("{markdown}\n").as_bytes())?;
    let source_hash = hash_targets(Path::new(&manifest.root), &page.targets)?;
    manifest.pages[index].file = file;
    manifest.pages[index].source_hash = source_hash;
    manifest.pages[index].source_commit = git_clean_head(Path::new(&manifest.root), &page.targets);
    manifest.pages[index].status = "written".to_string();
    manifest.pages[index].updated_at = now_ms();
    // A new page revision invalidates any previous review verdict.
    manifest.pages[index].review = None;
    save_manifest(out, &manifest)?;
    render_index(out, &manifest)?;
    Ok(manifest.pages[index].clone())
}

pub fn status(out: &Path) -> Result<WikiStatus> {
    let mut manifest = load_manifest(out)?;
    let root = PathBuf::from(&manifest.root);
    let mut written = 0usize;
    let mut planned = 0usize;
    let mut stale = 0usize;
    let mut review_passed = 0usize;
    let mut review_failed = 0usize;
    // Pages generated in the same run normally share a commit. Diff each
    // commit once instead of spawning four git processes per page.
    let mut changed_by_commit: HashMap<String, Option<HashSet<String>>> = HashMap::new();
    for state in &mut manifest.pages {
        if state.status == "written" {
            let stale_by_git = state.source_commit.as_deref().and_then(|commit| {
                let changed = changed_by_commit
                    .entry(commit.to_string())
                    .or_insert_with(|| git_changed_paths(&root, commit));
                changed.as_ref().map(|changed| {
                    state
                        .page
                        .targets
                        .iter()
                        .any(|target| target_changed(target, changed))
                })
            });
            let is_stale = match stale_by_git {
                Some(value) => value,
                None => {
                    let current = hash_targets(&root, &state.page.targets)?;
                    current != state.source_hash
                }
            };
            if is_stale {
                state.status = "stale".to_string();
                state.review = None;
            }
        }
        match state.status.as_str() {
            "written" => written += 1,
            "planned" => planned += 1,
            "stale" => stale += 1,
            _ => {}
        }
        match state.review.as_ref() {
            Some(review) if review.verdict == "pass" && review.issues.is_empty() => {
                review_passed += 1
            }
            Some(_) => review_failed += 1,
            None => {}
        }
    }
    save_manifest(out, &manifest)?;
    render_index(out, &manifest)?;
    Ok(WikiStatus {
        pages: manifest.pages,
        written,
        planned,
        stale,
        review_passed,
        review_failed,
    })
}

/// Check written pages for shape and citation existence. This is deliberately
/// advisory: it catches missing files, wrong page titles, broken lines, and
/// outline-sized pages, not semantic correctness of every claim.
pub fn check(out: &Path) -> Result<WikiCheckReport> {
    check_with(out, false)
}

/// `strict` raises the bar to the prompt's acceptance target and rejects a
/// wiki that still has planned or stale pages.
pub fn check_with(out: &Path, strict: bool) -> Result<WikiCheckReport> {
    let status = status(out)?;
    let manifest = load_manifest(out)?;
    let root = PathBuf::from(&manifest.root);
    let mut checked = 0usize;
    let mut issues = Vec::new();
    if strict && status.planned > 0 {
        issues.push(format!(
            "wiki is incomplete: {} page(s) still planned",
            status.planned
        ));
    }
    for state in manifest
        .pages
        .iter()
        .filter(|state| state.status != "planned")
    {
        checked += 1;
        if state.status == "stale" {
            issues.push(format!(
                "{}: source changed since generation; regenerate this page",
                state.page.id
            ));
        }
        let path = out.join(&state.file);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => {
                issues.push(format!("{}: cannot read page: {error}", state.page.id));
                continue;
            }
        };
        let lines = prose_lines(&text);
        let starts_with_h1 = lines
            .iter()
            .find(|line| !line.trim().is_empty())
            .map(|line| line.trim_start().starts_with("# "))
            .unwrap_or(false);
        let headings: Vec<&str> = lines
            .iter()
            .filter_map(|line| line.trim_start().strip_prefix("# "))
            .collect();
        if !starts_with_h1 {
            issues.push(format!(
                "{}: page does not start with a level-1 heading",
                state.page.id
            ));
        }
        if headings.len() != 1 {
            issues.push(format!(
                "{}: expected exactly one level-1 heading, found {}",
                state.page.id,
                headings.len()
            ));
        } else if normalized_title(headings[0]) != normalized_title(&state.page.title) {
            issues.push(format!(
                "{}: heading {:?} does not match planned title {:?}",
                state.page.id,
                headings[0].trim(),
                state.page.title
            ));
        }
        let section_headings: Vec<&str> = lines
            .iter()
            .filter_map(|line| line.trim_start().strip_prefix("## "))
            .collect();
        let sections = section_headings.len();
        let min_sections = if strict {
            STRICT_PAGE_SECTIONS
        } else {
            MIN_PAGE_SECTIONS
        };
        let min_chars = if strict {
            STRICT_PAGE_CHARS
        } else {
            MIN_PAGE_CHARS
        };
        if sections < min_sections {
            issues.push(format!(
                "{}: fewer than {min_sections} sections",
                state.page.id
            ));
        }
        if text.chars().count() < min_chars {
            issues.push(format!("{}: page is too short", state.page.id));
        }
        if strict
            && matches!(state.page.kind.as_str(), "architecture" | "flows")
            && !text.contains("```mermaid")
        {
            issues.push(format!(
                "{}: {} page must include a Mermaid diagram",
                state.page.id, state.page.kind
            ));
        }
        if state.page.kind == "module" {
            if strict {
                let heading_text = section_headings.join(" ").to_ascii_lowercase();
                if !heading_text.contains("depend") {
                    issues.push(format!(
                        "{}: module page must include a Dependencies section",
                        state.page.id
                    ));
                }
                if !heading_text.contains("fail") {
                    issues.push(format!(
                        "{}: module page must include a Failure modes section",
                        state.page.id
                    ));
                }
            }
            for target in &state.page.targets {
                if root.join(target).is_file() {
                    let target = target.replace('\\', "/");
                    if !text.contains(&target) {
                        issues.push(format!(
                            "{}: target is not covered: {target}",
                            state.page.id
                        ));
                    }
                }
            }
        }
        let prose = lines.join("\n");
        let words = prose.split_whitespace().count();
        let max_words = max_prose_words(&state.page.targets);
        if words > max_words {
            issues.push(format!(
                "{}: page exceeds {max_words} prose words (found {words}); trim secondary detail",
                state.page.id
            ));
        }
        for reference in bare_line_references(&prose) {
            issues.push(format!(
                "{}: bare line reference {reference}; repeat the full repository-relative path",
                state.page.id
            ));
        }
        for citation in citations(&prose, &root, &state.page.targets) {
            if citation_range(&citation).is_some() {
                issues.push(format!(
                    "{}: citation uses a line range; cite the exact declaration line: {citation}",
                    state.page.id
                ));
            }
            if !citation_exists(&root, &state.page.targets, &citation) {
                issues.push(format!(
                    "{}: citation does not resolve: {citation}",
                    state.page.id
                ));
            }
        }
        for link in wiki_links(&prose) {
            let target = link.trim_start_matches("./");
            if !out.join(target).exists() && !root.join(target).exists() {
                issues.push(format!(
                    "{}: wiki link does not resolve: {link}",
                    state.page.id
                ));
            }
        }
    }
    Ok(WikiCheckReport { checked, issues })
}

/// Page-local mechanical validation used by the generation loop. Unlike
/// `check_with(out, true)` it does not fail because other pages are still
/// planned or because a page is shorter than the strict human-review target;
/// apply already enforces the minimum shape. It does check target coverage and
/// every citation, so a generator cannot silently omit a target file.
fn page_check_issues(out: &Path, page_id: &str) -> Result<Vec<String>> {
    let report = check_with(out, false)?;
    let prefix = format!("{page_id}:");
    Ok(report
        .issues
        .into_iter()
        .filter(|issue| issue.starts_with(&prefix))
        // A page may link to other planned pages that do not exist yet. Link
        // completeness belongs to the whole-wiki `check`, not the per-page
        // generation gate.
        .filter(|issue| !issue.contains("wiki link does not resolve"))
        .collect())
}

fn needs_generation(out: &Path, state: &WikiPageState) -> bool {
    if state.status != "written"
        || state
            .review
            .as_ref()
            .is_none_or(|review| review.verdict != "pass" || !review.issues.is_empty())
    {
        return true;
    }
    page_check_issues(out, &state.page.id)
        .map(|issues| !issues.is_empty())
        .unwrap_or(true)
}

fn split_line(candidate: &str) -> (&str, Option<usize>) {
    // `path:12` is a line citation; `path:symbol` and `path:Type::method`
    // are symbol citations. Split at the first colon so qualified symbols do
    // not become part of the path.
    if let Some(index) = candidate.find(':') {
        let path = &candidate[..index];
        let suffix = &candidate[index + 1..];
        if !path.is_empty() && !suffix.is_empty() {
            if suffix.chars().all(|ch| ch.is_ascii_digit()) {
                if let Ok(line) = suffix.parse::<usize>() {
                    return (path, Some(line));
                }
            }
            return (path, None);
        }
    }
    (candidate, None)
}

fn citations(text: &str, root: &Path, targets: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('`') {
        rest = &rest[start + 1..];
        let Some(end) = rest.find('`') else {
            break;
        };
        let raw = rest[..end].trim();
        rest = &rest[end + 1..];
        if raw.starts_with('"') || raw.starts_with('\'') {
            // Quoted package subpaths such as `"./rpc-entry"` are export-map
            // keys, not filesystem citations.
            continue;
        }
        let value = raw.split_whitespace().next().unwrap_or(raw);
        let value = value.trim_end_matches(['(', ')', ',', ';']);
        let (path, _) = split_line(value);
        let has_extension = Path::new(path)
            .extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| WIKI_FILE_EXTS.contains(&extension.to_ascii_lowercase().as_str()))
            .unwrap_or(false);
        if path.is_empty()
            || path.starts_with('@')
            || path.starts_with("http")
            || path.contains("://")
            || path.contains('*')
            || path.starts_with("~/")
            || path.starts_with("dist/")
            || path.starts_with("build/")
            || path.starts_with("node_modules/")
            || path.starts_with("cwd/")
            || path.starts_with("agentDir/")
            || path.starts_with('<')
        {
            continue;
        }
        if !path.contains('/') && !has_extension {
            continue;
        }
        if path.starts_with("./") && !root.join(path).exists() {
            continue;
        }
        if !path.contains('/') {
            let in_target = targets.iter().any(|target| {
                let target = root.join(target);
                target.is_file()
                    && target
                        .parent()
                        .map(|parent| parent.join(path).exists())
                        .unwrap_or(false)
            });
            if !in_target && !root.join(path).exists() {
                // Bare names such as `models.json` describe configuration
                // outside the repository; they are not path citations.
                continue;
            }
        }
        if path.starts_with(".pi/") || path.starts_with(".cm/") {
            continue;
        }
        if !has_extension
            && !root.join(path).exists()
            && !targets.iter().any(|target| {
                root.join(target)
                    .parent()
                    .map(|parent| parent.join(path).exists())
                    .unwrap_or(false)
            })
        {
            // A slash alone does not make a string a source citation: package
            // subpaths such as `pi-ai/compat` are prose, not repository paths.
            continue;
        }
        out.push(value.to_string());
    }
    // Models do not always wrap citations in backticks. Scan prose for bare
    // `path:line` tokens as well; otherwise those citations escape the gate.
    for line in text.lines() {
        let chars: Vec<char> = line.chars().collect();
        let mut index = 0;
        while index < chars.len() {
            if chars[index] != ':' || index + 1 >= chars.len() || !chars[index + 1].is_ascii_digit()
            {
                index += 1;
                continue;
            }
            let mut start = index;
            while start > 0
                && (chars[start - 1].is_ascii_alphanumeric()
                    || matches!(chars[start - 1], '.' | '/' | '_' | '-'))
            {
                start -= 1;
            }
            let mut end = index + 1;
            while end < chars.len() && (chars[end].is_ascii_digit() || chars[end] == '-') {
                end += 1;
            }
            let token: String = chars[start..end].iter().collect();
            let (path, _) = split_line(&token);
            let has_extension = Path::new(path)
                .extension()
                .and_then(|extension| extension.to_str())
                .map(|extension| WIKI_FILE_EXTS.contains(&extension.to_ascii_lowercase().as_str()))
                .unwrap_or(false);
            if (path.contains('/') || has_extension)
                && !path.starts_with("http")
                && !path.contains("://")
                && !path.starts_with("~/")
                && !path.starts_with(".pi/")
                && !path.starts_with(".cm/")
                && !out.iter().any(|existing| existing == &token)
            {
                out.push(token);
            }
            index = end;
        }
    }
    out
}

fn bare_line_references(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('`') {
        rest = &rest[start + 1..];
        let Some(end) = rest.find('`') else {
            break;
        };
        let raw = rest[..end].trim();
        rest = &rest[end + 1..];
        if raw.len() > 1
            && raw.starts_with(':')
            && raw[1..].chars().all(|ch| ch.is_ascii_digit())
            && !out.iter().any(|existing| existing == raw)
        {
            out.push(raw.to_string());
        }
    }
    out
}

fn max_prose_words(targets: &[String]) -> usize {
    let target = if targets.len() >= 24 {
        2500
    } else if targets.len() >= 16 {
        2000
    } else {
        1500
    };
    target + target / 10
}

fn find_under(dir: &Path, candidate: &str) -> Option<PathBuf> {
    if dir.join(candidate).exists() {
        return Some(dir.join(candidate));
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return None;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            if let Some(found) = find_under(&path, candidate) {
                return Some(found);
            }
        } else if entry.file_name().to_string_lossy() == candidate {
            return Some(path);
        }
    }
    None
}

fn push_unique_path(out: &mut Vec<PathBuf>, path: PathBuf) {
    if path.exists() && !out.iter().any(|existing| existing == &path) {
        out.push(path);
    }
}

fn resolve_citation_all(root: &Path, targets: &[String], candidate: &str) -> Vec<PathBuf> {
    let candidate = candidate.trim_start_matches("./");
    let mut out = Vec::new();
    push_unique_path(&mut out, root.join(candidate));
    for target in targets {
        let target_path = root.join(target);
        if target_path.is_file() {
            if let Some(parent) = target_path.parent() {
                push_unique_path(&mut out, parent.join(candidate));
            }
        } else if let Some(found) = find_under(&target_path, candidate) {
            push_unique_path(&mut out, found);
        }
    }
    // A page may cite `core/sdk.ts` relative to its package instead of the
    // repository root. Try the common package layouts. A bare `src/index.ts`
    // is ambiguous in a monorepo, so keep every candidate and accept the
    // citation only when the requested line exists in one of them.
    if let Ok(entries) = fs::read_dir(root.join("packages")) {
        for entry in entries.flatten() {
            let package = entry.path();
            if !package.is_dir() {
                continue;
            }
            for base in [package.clone(), package.join("src")] {
                push_unique_path(&mut out, base.join(candidate));
            }
        }
    }
    push_unique_path(&mut out, root.join("src").join(candidate));
    out
}

fn line_exists(path: &Path, line: usize) -> bool {
    line > 0
        && fs::read_to_string(path)
            .map(|text| text.lines().count() >= line)
            .unwrap_or(false)
}

fn citation_range(candidate: &str) -> Option<(&str, usize, usize)> {
    let (path, suffix) = candidate.rsplit_once(':')?;
    let (start, end) = suffix.split_once('-')?;
    if path.is_empty()
        || start.is_empty()
        || end.is_empty()
        || !start.chars().all(|ch| ch.is_ascii_digit())
        || !end.chars().all(|ch| ch.is_ascii_digit())
    {
        return None;
    }
    Some((
        path,
        start.parse::<usize>().ok()?,
        end.parse::<usize>().ok()?,
    ))
}

fn line_range_exists(path: &Path, start: usize, end: usize) -> bool {
    start > 0
        && start <= end
        && fs::read_to_string(path)
            .map(|text| text.lines().count() >= end)
            .unwrap_or(false)
}

fn citation_exists(root: &Path, targets: &[String], candidate: &str) -> bool {
    if let Some((path, start, end)) = citation_range(candidate) {
        let path = path.trim_start_matches("./");
        let resolved = resolve_citation_all(root, targets, path);
        return !resolved.is_empty()
            && resolved
                .iter()
                .any(|resolved| line_range_exists(resolved, start, end));
    }
    let (path, line) = split_line(candidate);
    let candidate = path.trim_start_matches("./");
    if matches!(
        candidate,
        "AGENTS.override.md" | "AGENTS.md" | "CLAUDE.md" | "settings.json" | "auth.json"
    ) || candidate.starts_with('/')
    {
        return true;
    }
    let resolved = resolve_citation_all(root, targets, candidate);
    if resolved.is_empty() {
        return false;
    }
    match line {
        Some(line) => resolved.iter().any(|path| line_exists(path, line)),
        None => true,
    }
}

fn wiki_links(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("](") {
        rest = &rest[start + 2..];
        let Some(end) = rest.find(')') else {
            break;
        };
        let link = rest[..end].trim();
        rest = &rest[end + 1..];
        if link.is_empty()
            || link.starts_with('#')
            || link.starts_with("http")
            || link.starts_with("mailto:")
            || link.contains("://")
        {
            continue;
        }
        let link = link.split('#').next().unwrap_or(link);
        if !link.is_empty() {
            out.push(link.to_string());
        }
    }
    out
}

/// Return the next page that needs prose and its prompt. This is the simple
/// loop primitive for an agent: `next` -> write page -> `apply` -> `next`.
pub fn next(out: &Path) -> Result<Option<(WikiPageState, String)>> {
    let status = status(out)?;
    let Some(state) = status
        .pages
        .iter()
        .find(|state| needs_generation(out, state))
        .cloned()
    else {
        return Ok(None);
    };
    let prompt = prompt(out, &state.page.id)?;
    Ok(Some((state, prompt)))
}

/// Run one externally supplied agent command for every pending page. The
/// command receives the page prompt on stdin and must print Markdown on stdout.
/// This keeps cm model-agnostic: pi, Claude Code, Codex, or a local script can
/// drive the whole wiki.
fn parse_review(text: &str) -> Result<WikiReview> {
    let text = text.trim();
    let text = if let Some(rest) = text.strip_prefix("```") {
        rest.split_once('\n')
            .map(|(_, rest)| rest)
            .unwrap_or(rest)
            .trim_end_matches("```")
            .trim()
    } else {
        text
    };
    let start = text
        .find('{')
        .ok_or_else(|| anyhow!("reviewer returned no JSON object"))?;
    let end = text
        .rfind('}')
        .ok_or_else(|| anyhow!("reviewer returned incomplete JSON"))?;
    let mut review: WikiReview = serde_json::from_str(&text[start..=end])
        .context("parse reviewer JSON (expected verdict/score/issues)")?;
    review.verdict = review.verdict.to_ascii_lowercase();
    if review.verdict != "pass" && review.verdict != "fail" {
        bail!(
            "reviewer verdict must be pass or fail, got {:?}",
            review.verdict
        );
    }
    Ok(review)
}

fn review_markdown(
    out: &Path,
    page_id: &str,
    markdown: &str,
    review_cmd: &str,
) -> Result<WikiReview> {
    let base_prompt = prompt(out, page_id)?;
    let prompt = format!(
        "You are an independent, adversarial reviewer of one repository-wiki page.\n\
         Do not modify files. Verify the page against the actual source tree and its targets.\n\
         Check factual accuracy, ownership, citations, public/internal boundaries, lifecycle, failure behavior, and missing important topics.\n\
         Treat these as blocking `issues`, not notes: a bare `:NN` reference; a citation to a guard, catch, function header, or loop header where the claim is about the throw/return; one shared citation for several named exports; a Mermaid edge that is not an actual call/import relationship; an unsupported absolute quantifier (\"only\", \"all\", \"every\", \"never\", \"always\"); a TARGET file or its primary export missing from the module source table; a claim about a whole method that ignores an early-return or exception branch.\n\
         Put other completeness, wording, and style gaps in `notes`.\n\
         Return ONLY one JSON object with this exact shape:\n\
         {{\"verdict\":\"pass|fail\",\"score\":0-100,\"issues\":[\"blocking accuracy/citation issue\",\"...\"],\"notes\":[\"non-blocking completeness or style note\",\"...\"]}}\n\
         Use \"pass\" ONLY when `issues` is empty. `notes` may be non-empty. Accuracy is the first gate; completeness and cost come later.\n\
         \nROOT: {root}\n\
         \nBASE PAGE PROMPT:\n{base_prompt}\n\
         \nPAGE MARKDOWN:\n{markdown}\n",
        root = display_path(Path::new(&load_manifest(out)?.root)),
    );
    let output = run_agent(review_cmd, &prompt, Path::new(&load_manifest(out)?.root))?;
    let review = parse_review(&output)?;
    record_review(out, page_id, &review)?;
    Ok(review)
}

fn record_review(out: &Path, page_id: &str, review: &WikiReview) -> Result<()> {
    let mut manifest = load_manifest(out)?;
    if let Some(state) = manifest
        .pages
        .iter_mut()
        .find(|state| state.page.id == page_id)
    {
        state.review = Some(WikiReviewState {
            verdict: review.verdict.clone(),
            score: review.score,
            issues: review.issues.clone(),
            notes: review.notes.clone(),
            reviewed_at: now_ms(),
        });
        save_manifest(out, &manifest)?;
    }
    Ok(())
}

/// Run an independent LLM reviewer against an already-written page. The
/// reviewer command receives the full page plus its generation prompt on
/// stdin and must print the JSON contract described in `review_markdown`.
pub fn review(out: &Path, page_id: &str, review_cmd: &str) -> Result<WikiReview> {
    let manifest = load_manifest(out)?;
    let state = manifest
        .pages
        .iter()
        .find(|state| state.page.id == page_id)
        .ok_or_else(|| anyhow!("unknown wiki page {page_id:?}"))?;
    let markdown = fs::read_to_string(out.join(&state.file))
        .with_context(|| format!("read {}", out.join(&state.file).display()))?;
    review_markdown(out, page_id, &markdown, review_cmd)
}

pub fn generate(out: &Path, agent_cmd: &str, max_pages: usize) -> Result<WikiGenerateReport> {
    generate_matching(out, agent_cmd, max_pages, None)
}

/// Generate one named page, even when other planned pages precede it.
pub fn generate_page(out: &Path, agent_cmd: &str, page_id: &str) -> Result<WikiGenerateReport> {
    generate_matching(out, agent_cmd, 1, Some(page_id))
}

fn generate_matching(
    out: &Path,
    agent_cmd: &str,
    max_pages: usize,
    page_filter: Option<&str>,
) -> Result<WikiGenerateReport> {
    let _ = status(out)?;
    let manifest = load_manifest(out)?;
    let limit = if max_pages == 0 {
        usize::MAX
    } else {
        max_pages
    };
    let pending: Vec<WikiPageState> = manifest
        .pages
        .iter()
        .filter(|state| needs_generation(out, state))
        .filter(|state| page_filter.is_none_or(|page_id| state.page.id == page_id))
        .take(limit)
        .cloned()
        .collect();
    let mut report = WikiGenerateReport {
        attempted: pending.len(),
        written: 0,
        failed: Vec::new(),
    };
    let output_target = vec![".".to_string()];
    for state in pending {
        let prompt = prompt(out, &state.page.id)?;
        let targets_before = hash_targets(Path::new(&manifest.root), &state.page.targets).ok();
        let output_before = hash_targets(out, &output_target).ok();
        match run_agent(agent_cmd, &prompt, Path::new(&manifest.root)) {
            Ok(output) => {
                let targets_after =
                    hash_targets(Path::new(&manifest.root), &state.page.targets).ok();
                let output_after = hash_targets(out, &output_target).ok();
                let changed = |before: &Option<String>, after: &Option<String>| matches!((before, after), (Some(before), Some(after)) if before != after);
                if changed(&targets_before, &targets_after)
                    || changed(&output_before, &output_after)
                {
                    report.failed.push(format!(
                        "{}: agent modified target files or wiki output; return Markdown on stdout only",
                        state.page.id
                    ));
                    continue;
                }
                let output = clean_agent_output(&output);
                if let Err(error) = apply(out, &state.page.id, &output) {
                    report.failed.push(format!("{}: {error:#}", state.page.id));
                    continue;
                }
                match page_check_issues(out, &state.page.id) {
                    Ok(issues) if !issues.is_empty() => {
                        let _ = mark_stale(out, &state.page.id);
                        report.failed.push(format!(
                            "{}: mechanical check failed: {}",
                            state.page.id,
                            issues.join("; ")
                        ));
                    }
                    Ok(_) => report.written += 1,
                    Err(error) => report.failed.push(format!(
                        "{}: mechanical check failed: {error:#}",
                        state.page.id
                    )),
                }
            }
            Err(error) => report.failed.push(format!("{}: {error:#}", state.page.id)),
        }
    }
    Ok(report)
}

fn mark_stale(out: &Path, page_id: &str) -> Result<()> {
    let mut manifest = load_manifest(out)?;
    if let Some(state) = manifest
        .pages
        .iter_mut()
        .find(|state| state.page.id == page_id)
    {
        state.status = "stale".to_string();
    }
    save_manifest(out, &manifest)?;
    render_index(out, &manifest)?;
    Ok(())
}

/// Generate with an independent reviewer in the loop. The reviewer is
/// separate from the generator: it returns pass/fail plus issues, and failed
/// pages are revised with those issues until the round budget is exhausted.
pub fn generate_reviewed(
    out: &Path,
    agent_cmd: &str,
    review_cmd: Option<&str>,
    review_rounds: usize,
    max_pages: usize,
) -> Result<WikiGenerateReport> {
    generate_reviewed_matching(out, agent_cmd, review_cmd, review_rounds, max_pages, None)
}

/// Reviewed generation for one named page, even when other planned pages precede it.
pub fn generate_reviewed_page(
    out: &Path,
    agent_cmd: &str,
    review_cmd: Option<&str>,
    review_rounds: usize,
    page_id: &str,
) -> Result<WikiGenerateReport> {
    generate_reviewed_matching(out, agent_cmd, review_cmd, review_rounds, 1, Some(page_id))
}

fn generate_reviewed_matching(
    out: &Path,
    agent_cmd: &str,
    review_cmd: Option<&str>,
    review_rounds: usize,
    max_pages: usize,
    page_filter: Option<&str>,
) -> Result<WikiGenerateReport> {
    let Some(review_cmd) = review_cmd else {
        return generate_matching(out, agent_cmd, max_pages, page_filter);
    };
    let _ = status(out)?;
    let manifest = load_manifest(out)?;
    let root = PathBuf::from(&manifest.root);
    let limit = if max_pages == 0 {
        usize::MAX
    } else {
        max_pages
    };
    let pending: Vec<WikiPageState> = manifest
        .pages
        .iter()
        .filter(|state| needs_generation(out, state))
        .filter(|state| page_filter.is_none_or(|page_id| state.page.id == page_id))
        .take(limit)
        .cloned()
        .collect();
    let mut report = WikiGenerateReport {
        attempted: pending.len(),
        written: 0,
        failed: Vec::new(),
    };
    let output_target = vec![".".to_string()];
    let max_rounds = review_rounds.max(1);
    for state in pending {
        let base_prompt = prompt(out, &state.page.id)?;
        let mut prompt = base_prompt.clone();
        let mut passed = false;
        let mut last_issues = Vec::new();
        for round in 0..max_rounds {
            let targets_before = hash_targets(&root, &state.page.targets).ok();
            let output_before = hash_targets(out, &output_target).ok();
            let output = match run_agent(agent_cmd, &prompt, &root) {
                Ok(output) => output,
                Err(error) => {
                    last_issues = vec![format!("{error:#}")];
                    break;
                }
            };
            let targets_after = hash_targets(&root, &state.page.targets).ok();
            let output_after = hash_targets(out, &output_target).ok();
            let changed = |before: &Option<String>, after: &Option<String>| matches!((before, after), (Some(before), Some(after)) if before != after);
            if changed(&targets_before, &targets_after) || changed(&output_before, &output_after) {
                last_issues = vec![
                    "agent modified target files or wiki output; return Markdown on stdout only"
                        .to_string(),
                ];
                break;
            }
            let markdown = clean_agent_output(&output);
            if let Err(error) = apply(out, &state.page.id, &markdown) {
                last_issues = vec![format!("{error:#}")];
                break;
            }
            let mechanical = match page_check_issues(out, &state.page.id) {
                Ok(issues) => issues,
                Err(error) => vec![format!("mechanical check failed: {error:#}")],
            };
            if !mechanical.is_empty() {
                last_issues = mechanical;
                if round + 1 < max_rounds {
                    prompt = format!(
                        "{base_prompt}\n\nPREVIOUS DRAFT:\n{markdown}\n\nMECHANICAL FINDINGS:\n- {}\n\nRewrite the complete page and fix every finding. Return only Markdown.",
                        last_issues.join("\n- ")
                    );
                }
                continue;
            }
            match review_markdown(out, &state.page.id, &markdown, review_cmd) {
                Ok(review) if review.verdict == "pass" && review.issues.is_empty() => {
                    passed = true;
                    break;
                }
                Ok(review) => {
                    last_issues = if review.issues.is_empty() {
                        vec!["reviewer rejected the page without details".to_string()]
                    } else {
                        review.issues
                    };
                }
                Err(error) => {
                    last_issues = vec![format!("reviewer failed: {error:#}")];
                    break;
                }
            }
            if round + 1 < max_rounds {
                prompt = format!(
                    "{base_prompt}\n\nPREVIOUS DRAFT:\n{markdown}\n\nREVIEW FINDINGS:\n- {}\n\nRewrite the complete page and fix every finding. Return only Markdown.",
                    last_issues.join("\n- ")
                );
            }
        }
        if passed {
            report.written += 1;
        } else {
            let _ = mark_stale(out, &state.page.id);
            report.failed.push(format!(
                "{}: review failed: {}",
                state.page.id,
                last_issues.join("; ")
            ));
        }
    }
    Ok(report)
}

fn run_agent(command: &str, prompt: &str, root: &Path) -> Result<String> {
    let mut cmd = if cfg!(windows) {
        let mut cmd = Command::new("cmd");
        cmd.arg("/C").arg(command);
        cmd
    } else {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command);
        cmd
    };
    let mut child = cmd
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("start wiki agent: {command}"))?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(prompt.as_bytes())?;
    }
    let timeout = std::env::var("CM_WIKI_AGENT_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(WIKI_AGENT_TIMEOUT_SECS));
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("wiki agent timed out after {}s: {command}", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!(
            "wiki agent failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

pub(crate) fn clean_agent_output(text: &str) -> String {
    let mut text = text.trim();
    let mut strip_trailing_fence = false;
    if text.starts_with("```") {
        strip_trailing_fence = true;
        if let Some(newline) = text.find('\n') {
            text = &text[newline + 1..];
        }
    }
    // Agents sometimes prepend a sentence despite the prompt. Start at the
    // first real level-1 heading; apply() still verifies it is the right page.
    if !text.starts_with("# ") {
        if let Some((offset, _)) = text.match_indices("\n# ").next() {
            let prefix = &text[..offset];
            strip_trailing_fence |= prefix
                .lines()
                .any(|line| line.trim_start().starts_with("```"));
            text = &text[offset + 1..];
        }
    }
    text = text.trim_end();
    if strip_trailing_fence && text.ends_with("```") {
        text = text[..text.len() - 3].trim_end();
    }
    text.trim().to_string()
}

fn normalize_git_path(path: &str) -> String {
    path.replace('\\', "/")
        .trim_start_matches("./")
        .to_lowercase()
}

fn insert_changed(changed: &mut HashSet<String>, path: &str) {
    let path = normalize_git_path(path.trim());
    // `.cm/wiki` changes on every apply and is output, not source. Keeping it
    // out of the dirty set also lets a root-targeted page (`.`) stay clean.
    if path.is_empty() || path.trim() == "." || path.starts_with(".cm/") {
        return;
    }
    changed.insert(path);
}

/// Record a commit only when every target is clean. If the page was generated
/// from uncommitted work, `HEAD` does not describe the bytes that were read;
/// using it would mark the page stale immediately after the user commits.
fn git_clean_head(root: &Path, targets: &[String]) -> Option<String> {
    if targets.is_empty() {
        return None;
    }
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(["status", "--porcelain=v1", "-z", "-uall", "--"]);
    for target in targets {
        command.arg(target);
    }
    let output = command.output().ok()?;
    if !output.status.success() || !output.stdout.is_empty() {
        return None;
    }
    crate::explore_api::git_head(root).ok()
}

pub(crate) fn parse_porcelain_z(bytes: &[u8]) -> Vec<String> {
    let mut fields = bytes.split(|byte| *byte == 0);
    let mut out = Vec::new();
    while let Some(field) = fields.next() {
        if field.len() < 4 {
            continue;
        }
        let status = &field[..2];
        let path = String::from_utf8_lossy(&field[3..]).to_string();
        if !path.trim().is_empty() {
            out.push(path);
        }
        // With -z a rename/copy is `XY NEW\0OLD\0`: both paths matter for
        // staleness, and the second field has no status prefix.
        if matches!(status[0], b'R' | b'C') {
            if let Some(original) = fields.next() {
                if !original.is_empty() {
                    out.push(String::from_utf8_lossy(original).to_string());
                }
            }
        }
    }
    out
}

fn git_changed_paths(root: &Path, base: &str) -> Option<HashSet<String>> {
    let mut changed = HashSet::new();
    let run = |args: &[String]| -> Option<String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&output.stdout).to_string())
    };
    let head_range = format!("{base}..HEAD");
    for args in [
        vec![
            "diff".to_string(),
            "--name-only".to_string(),
            "--relative".to_string(),
            head_range,
        ],
        vec![
            "diff".to_string(),
            "--name-only".to_string(),
            "--relative".to_string(),
        ],
        vec![
            "diff".to_string(),
            "--cached".to_string(),
            "--name-only".to_string(),
            "--relative".to_string(),
        ],
    ] {
        let output = run(&args)?;
        for line in output.lines() {
            insert_changed(&mut changed, line);
        }
    }
    let status = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["status", "--porcelain=v1", "-z", "-uall"])
        .output()
        .ok()?;
    if !status.status.success() {
        return None;
    }
    for path in parse_porcelain_z(&status.stdout) {
        insert_changed(&mut changed, &path);
    }
    Some(changed)
}

fn target_changed(target: &str, changed: &HashSet<String>) -> bool {
    let target = normalize_git_path(target);
    if target == "." {
        return !changed.is_empty();
    }
    changed
        .iter()
        .any(|path| path == &target || path.starts_with(&format!("{target}/")))
}

fn hash_targets(root: &Path, targets: &[String]) -> Result<String> {
    let mut bytes = Vec::new();
    for target in targets {
        let path = root.join(target);
        bytes.extend_from_slice(target.as_bytes());
        if path.is_file() {
            bytes.extend_from_slice(&fs::read(&path).unwrap_or_default());
        } else if path.is_dir() {
            hash_dir(&path, &mut bytes)?;
        } else {
            bytes.extend_from_slice(b"<missing>");
        }
    }
    Ok(format!("{:016x}", xxh3_64(&bytes)))
}

fn hash_dir(dir: &Path, out: &mut Vec<u8>) -> Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)?.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        out.extend_from_slice(name.as_bytes());
        if path.is_dir() {
            hash_dir(&path, out)?;
        } else if path.is_file() {
            out.extend_from_slice(&fs::read(&path).unwrap_or_default());
        }
    }
    Ok(())
}

fn render_index(out: &Path, manifest: &WikiManifest) -> Result<()> {
    let mut text = String::from("# Project Wiki\n\n");
    text.push_str(&format!(
        "Generated by `cm wiki` from `{}`.\n\n",
        display_path(Path::new(&manifest.root))
    ));
    let order = [
        "overview",
        "architecture",
        "operations",
        "testing",
        "flows",
        "module",
    ];
    for kind in order {
        let mut pages: Vec<&WikiPageState> = manifest
            .pages
            .iter()
            .filter(|state| state.page.kind == kind)
            .collect();
        if pages.is_empty() {
            continue;
        }
        pages.sort_by(|left, right| left.page.title.cmp(&right.page.title));
        text.push_str(&format!("## {}\n\n", title_for_kind(kind)));
        for state in pages {
            text.push_str(&format!(
                "- [{}]({}) — {}\n",
                state.page.title, state.file, state.status
            ));
        }
        text.push('\n');
    }
    write_atomic(&out.join("index.md"), text.as_bytes())?;
    Ok(())
}

fn title_for_kind(kind: &str) -> &'static str {
    match kind {
        "overview" => "Overview",
        "architecture" => "Architecture",
        "operations" => "Operations",
        "testing" => "Testing",
        "flows" => "Flows",
        "module" => "Modules",
        _ => "Other",
    }
}
