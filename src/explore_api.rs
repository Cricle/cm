use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::brief::FEEDBACK_KIND;
use crate::explore::{scan_repo, RepoScan};
use crate::frames::{frame_query, latest_frame, save_frame, TaskFrame, FRAME_KIND};
use crate::model::{estimate_tokens, memory_snippet, Memory};
use crate::mutate::{absorb, AbsorbOptions, AbsorbReport};
use crate::search::{active_memories, select_context, RecallQuery};
use crate::source::source_parts;
use crate::store::Store;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExploreMode {
    #[default]
    Auto,
    Macro,
    Micro,
}

impl ExploreMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Macro => "macro",
            Self::Micro => "micro",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ExploreOptions {
    pub root: PathBuf,
    pub mode: ExploreMode,
    pub task: Option<String>,
    pub query: Option<String>,
    pub changed: Option<Vec<String>>,
    pub max_targets: usize,
}

impl Default for ExploreOptions {
    fn default() -> Self {
        Self {
            root: PathBuf::from("."),
            mode: ExploreMode::Auto,
            task: None,
            query: None,
            changed: None,
            max_targets: 28,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExplorePlan {
    pub mode: String,
    pub evidence_id: String,
    pub prompt: String,
    pub targets: Vec<ExploreTarget>,
}

/// Deterministic, zero-LLM repository map produced from manifests and the
/// file scan. `ops` is strict JSONL accepted by `cm absorb`.
#[derive(Clone, Debug, Serialize)]
pub struct MechanicalPlan {
    pub mode: String,
    pub frame_id: String,
    pub records: usize,
    pub ops: String,
    pub report: Option<AbsorbReport>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExploreCursor {
    pub git_head: String,
    pub built_at: u64,
    pub mode: String,
    pub memories: usize,
}

pub(crate) fn git_head(root: &Path) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .context("run git rev-parse HEAD")?;
    if !output.status.success() {
        bail!(
            "git rev-parse HEAD failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn default_explore_query(root: &Path) -> String {
    format!(
        "repository architecture, entry points, build/test commands, invariants, and non-obvious behavior in {}",
        display_explore_root(root)
    )
}

/// Build a dynamic exploration plan. This is the library entry point behind
/// `cm explore`: it chooses macro/micro, selects targets, and renders the
/// JSONL contract prompt without touching the CLI.
pub(crate) fn explore_plan(path: &Path, options: &ExploreOptions) -> Result<ExplorePlan> {
    let root = if options.root.is_dir() {
        options
            .root
            .canonicalize()
            .unwrap_or_else(|_| options.root.clone())
    } else {
        bail!(
            "explore root is not a directory: {}",
            options.root.display()
        );
    };
    let requested_mode = options.mode.as_str();
    let (evidence_id, frame) = ensure_explore_frame(path, &root, options.task.as_deref())?;
    let query = options
        .query
        .clone()
        .or_else(|| {
            frame
                .as_ref()
                .filter(|frame| !frame.task.starts_with("explore:"))
                .and_then(frame_query)
        })
        .unwrap_or_else(|| default_explore_query(&root));
    let mut store = Store::open(path)?;
    let active = active_memories(&mut store)?;
    let user_memories: Vec<Memory> = active
        .iter()
        .filter(|memory| !matches!(memory.kind.as_str(), FRAME_KIND | FEEDBACK_KIND))
        .cloned()
        .collect();
    let anchored_count = user_memories
        .iter()
        .filter(|memory| memory.source_hash.is_some())
        .count();
    let mut changed: Vec<String> = match &options.changed {
        Some(changed) => changed.clone(),
        None => changed_paths(&root)
            .unwrap_or_default()
            .into_iter()
            .collect(),
    };
    changed.retain(|path| crate::explore::is_explore_input(path));
    changed.sort();
    let mode = explore_mode(
        requested_mode,
        user_memories.len(),
        &changed,
        frame.as_ref(),
        options.query.is_some(),
    )?;
    let task = frame.as_ref().map(|frame| frame.task.as_str());
    let known = collect_explore_known(&mut store, &query, task, &active)?;
    let scan = scan_repo(&root);
    let max_items = if mode == "macro" {
        (8 + scan.source_files / 250).clamp(8, 24)
    } else {
        (4 + known.len() / 4).clamp(6, 16)
    };
    let budget = if mode == "macro" {
        2000 + max_items * 40
    } else {
        1200 + max_items * 40
    };
    let mut targets = collect_explore_targets(
        mode,
        &scan,
        &changed,
        frame.as_ref(),
        &known,
        &user_memories,
    );
    targets.truncate(options.max_targets.clamp(1, 28));
    if targets.is_empty() {
        targets.push(ExploreTarget {
            path: ".".to_string(),
            reason: "repository root".to_string(),
        });
    }
    let mut prompt_known = known;
    let mut prompt = build_explore_prompt(ExplorePromptInput {
        mode,
        root: &root,
        task,
        query: &query,
        evidence_id: &evidence_id,
        scan: &scan,
        changed: &changed,
        known: &prompt_known,
        active_count: user_memories.len(),
        anchored_count,
        targets: &targets,
        max_items,
    });
    while estimate_tokens(&prompt) > budget && !prompt_known.is_empty() {
        prompt_known.pop();
        prompt = build_explore_prompt(ExplorePromptInput {
            mode,
            root: &root,
            task,
            query: &query,
            evidence_id: &evidence_id,
            scan: &scan,
            changed: &changed,
            known: &prompt_known,
            active_count: user_memories.len(),
            anchored_count,
            targets: &targets,
            max_items,
        });
    }
    Ok(ExplorePlan {
        mode: mode.to_string(),
        evidence_id,
        prompt,
        targets,
    })
}

fn mechanical_op(
    subject: impl Into<String>,
    text: impl Into<String>,
    source: impl Into<String>,
    evidence_id: &str,
) -> serde_json::Value {
    json!({
        "op": "remember",
        "kind": "fact",
        "scope": "repo",
        "subject": subject.into(),
        "text": text.into(),
        "source": source.into(),
        "evidence": [evidence_id],
        "confidence": 0.9
    })
}

/// Build deterministic memory operations from the repository snapshot. No
/// model is called and no source file is read into a prompt.
pub(crate) fn mechanical_explore(
    path: &Path,
    options: &ExploreOptions,
    apply: bool,
) -> Result<MechanicalPlan> {
    let root = if options.root.is_dir() {
        options
            .root
            .canonicalize()
            .unwrap_or_else(|_| options.root.clone())
    } else {
        bail!(
            "explore root is not a directory: {}",
            options.root.display()
        );
    };
    let (frame_id, _) = ensure_explore_frame(path, &root, options.task.as_deref())?;
    let scan = scan_repo(&root);
    let default_source = scan
        .manifests
        .first()
        .cloned()
        .unwrap_or_else(|| ".".to_string());
    let mut ops = Vec::new();
    let top: Vec<String> = scan
        .top_dirs
        .iter()
        .take(8)
        .map(|dir| format!("{} ({} source files)", dir.path, dir.sources))
        .collect();
    let layout = if top.is_empty() {
        format!(
            "Repository snapshot: {} files, {} source files; no top-level source areas found.",
            scan.files, scan.source_files
        )
    } else {
        format!(
            "Repository snapshot: {} files, {} source files. Top areas: {}.",
            scan.files,
            scan.source_files,
            top.join("; ")
        )
    };
    ops.push(mechanical_op(
        "repo layout",
        layout,
        &default_source,
        &frame_id,
    ));

    let mut commands = Vec::new();
    let mut command_source = default_source.clone();
    if root.join("package.json").is_file() {
        command_source = "package.json".to_string();
        if let Ok(text) = fs::read_to_string(root.join("package.json")) {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(scripts) = value.get("scripts").and_then(|value| value.as_object()) {
                    for name in ["build", "test", "check"] {
                        if scripts.contains_key(name) {
                            commands.push(format!("npm run {name}"));
                        }
                    }
                }
            }
        }
    }
    if root.join("Cargo.toml").is_file() {
        if command_source == default_source {
            command_source = "Cargo.toml".to_string();
        }
        commands.push("cargo build".to_string());
        commands.push("cargo test".to_string());
    }
    if !commands.is_empty() {
        ops.push(mechanical_op(
            "build and test commands",
            format!(
                "Mechanical manifest scan suggests: {}.",
                commands.join("; ")
            ),
            command_source,
            &frame_id,
        ));
    }

    if !scan.entrypoints.is_empty() {
        ops.push(mechanical_op(
            "entry points",
            format!(
                "Detected entry points: {}.",
                scan.entrypoints
                    .iter()
                    .take(12)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
            scan.entrypoints[0].clone(),
            &frame_id,
        ));
    }

    if scan.areas.is_empty() {
        for dir in scan.top_dirs.iter().take(8) {
            ops.push(mechanical_op(
                format!("area: {}", dir.path),
                format!(
                    "{} source files across {} files in {}.",
                    dir.sources, dir.files, dir.path
                ),
                dir.path.clone(),
                &frame_id,
            ));
        }
    } else {
        for area in scan.areas.iter().take(12) {
            ops.push(mechanical_op(
                format!("area: {}", area.path),
                format!(
                    "{} source files across {} files in {}.",
                    area.sources, area.files, area.path
                ),
                area.path.clone(),
                &frame_id,
            ));
        }
    }

    let mut lines = Vec::with_capacity(ops.len());
    for op in &ops {
        lines.push(serde_json::to_string(op)?);
    }
    let ops_text = lines.join("\n");
    let report = if apply {
        Some(absorb(
            path,
            &ops_text,
            &AbsorbOptions {
                replace: true,
                dry_run: false,
                allow_long_text: true,
            },
            None,
            Some(&root),
        )?)
    } else {
        None
    };
    Ok(MechanicalPlan {
        mode: "mechanical".to_string(),
        frame_id,
        records: ops.len(),
        ops: ops_text,
        report,
    })
}

pub(crate) fn changed_paths(root: &Path) -> Result<HashSet<String>> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("status")
        .arg("--porcelain=v1")
        .arg("-z")
        .arg("-uall")
        .output()
        .context("run git status")?;
    if !output.status.success() {
        bail!(
            "git status failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let data = String::from_utf8_lossy(&output.stdout);
    let mut paths = HashSet::new();
    for entry in data.split('\0').filter(|entry| !entry.is_empty()) {
        // porcelain v1 -z stores `XY PATH` with exactly two status bytes.
        let Some(path) = entry.get(3..) else {
            continue;
        };
        let path = path.rsplit_once(" -> ").map(|(_, new)| new).unwrap_or(path);
        if path.is_empty() {
            continue;
        }
        paths.insert(path.replace('\\', "/").to_lowercase());
    }
    Ok(paths)
}

pub(crate) struct ExploreKnown {
    pub(crate) memory: crate::model::Memory,
    pub(crate) preview: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExploreTarget {
    pub path: String,
    pub reason: String,
}

pub(crate) fn ensure_explore_frame(
    path: &Path,
    root: &Path,
    requested_task: Option<&str>,
) -> Result<(String, Option<TaskFrame>)> {
    let repo = root
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "repo".to_string());
    let default_task = format!("explore:{repo}");
    let mut store = Store::open(path)?;
    match requested_task {
        Some("current") => {
            if let Some((memory, frame)) = latest_frame(&mut store, Some("current"))? {
                return Ok((memory.id, Some(frame)));
            }
        }
        Some(task) => {
            if let Some((memory, frame)) = latest_frame(&mut store, Some(task))? {
                return Ok((memory.id, Some(frame)));
            }
        }
        None => {
            // Reuse an active task instead of hijacking `current` with an
            // exploration-only frame. If no task is active, reuse the repo's
            // exploration frame when one exists.
            if let Some((memory, frame)) = latest_frame(&mut store, Some("current"))? {
                if frame.status == "active" {
                    return Ok((memory.id, Some(frame)));
                }
            }
            if let Some((memory, frame)) = latest_frame(&mut store, Some(&default_task))? {
                return Ok((memory.id, Some(frame)));
            }
        }
    }
    drop(store);
    let task = requested_task
        .filter(|task| *task != "current")
        .map(str::to_string)
        .unwrap_or(default_task);
    let frame = TaskFrame {
        task,
        goal: format!("Explore repository {repo} and record high-signal cm memory"),
        status: "active".to_string(),
        constraints: Vec::new(),
        next: vec!["follow the explore protocol and absorb the result".to_string()],
        open: Vec::new(),
        touches: Vec::new(),
        notes: Vec::new(),
        owner: None,
        lease_until: None,
        updated_at: crate::model::now_ms(),
    };
    let memory = save_frame(path, &frame, None, "agent:explore", None, false)?;
    Ok((memory.id, Some(frame)))
}

pub(crate) fn display_explore_root(root: &Path) -> String {
    let text = root.to_string_lossy().replace('\\', "/");
    text.strip_prefix("//?/").unwrap_or(&text).to_string()
}

pub(crate) fn explore_source_path(memory: &crate::model::Memory) -> Option<String> {
    source_parts(&memory.source).map(|(path, _)| {
        path.to_string_lossy()
            .replace('\\', "/")
            .trim_start_matches("./")
            .to_string()
    })
}

pub(crate) fn collect_explore_known(
    store: &mut Store,
    query: &str,
    task: Option<&str>,
    active: &[crate::model::Memory],
) -> Result<Vec<ExploreKnown>> {
    let mut known = Vec::new();
    let mut seen = HashSet::new();
    let q = RecallQuery {
        query: query.to_string(),
        budget_tokens: 1000,
        top: 32,
        max_items: 12,
        scope: None,
        task: task.map(str::to_string),
        kind: None,
        min_trust: Some("imported".to_string()),
        min_score: 0.0,
        compact: true,
        snippet_lines: 0,
    };
    let (selected, _) = select_context(store, &q)?;
    for memory in selected {
        seen.insert(memory.id.clone());
        known.push(ExploreKnown {
            preview: memory_snippet(&memory, query, 180),
            memory,
        });
    }
    let mut recent: Vec<crate::model::Memory> = active
        .iter()
        .filter(|memory| !matches!(memory.kind.as_str(), FRAME_KIND | FEEDBACK_KIND))
        .filter(|memory| !seen.contains(&memory.id))
        .cloned()
        .collect();
    recent.sort_by(|left, right| {
        right
            .created_at
            .cmp(&left.created_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    for memory in recent {
        if known.len() >= 24 {
            break;
        }
        seen.insert(memory.id.clone());
        known.push(ExploreKnown {
            preview: memory_snippet(&memory, query, 180),
            memory,
        });
    }
    Ok(known)
}

pub(crate) fn explore_coverage(memories: &[crate::model::Memory], path: &str) -> usize {
    memories
        .iter()
        .filter(|memory| {
            explore_source_path(memory).is_some_and(|source| {
                source == path
                    || source
                        .strip_prefix(path)
                        .is_some_and(|rest| rest.starts_with('/'))
            })
        })
        .count()
}

pub(crate) fn collect_explore_targets(
    mode: &str,
    scan: &RepoScan,
    changed: &[String],
    frame: Option<&TaskFrame>,
    known: &[ExploreKnown],
    active: &[crate::model::Memory],
) -> Vec<ExploreTarget> {
    let mut targets = Vec::new();
    let mut seen = HashSet::new();
    let mut push = |path: String, reason: String| {
        let path = path.replace('\\', "/").trim_start_matches("./").to_string();
        if path.is_empty() || !seen.insert(path.clone()) {
            return;
        }
        targets.push(ExploreTarget { path, reason });
    };
    if mode == "macro" {
        for manifest in scan.manifests.iter().take(10) {
            push(
                manifest.clone(),
                "manifest: build, test, or workspace boundary".to_string(),
            );
        }
        for entrypoint in scan.entrypoints.iter().take(8) {
            push(
                entrypoint.clone(),
                "entry point: trace a runtime path".to_string(),
            );
        }
        for area in scan.areas.iter().take(12) {
            let covered = explore_coverage(active, &area.path);
            push(
                area.path.clone(),
                format!(
                    "nested area: {} source files; {} active memories cite this area",
                    area.sources, covered
                ),
            );
        }
        for dir in &scan.top_dirs {
            let covered = explore_coverage(active, &dir.path);
            push(
                dir.path.clone(),
                format!(
                    "{} source files / {} files; {} active memories cite this area",
                    dir.sources, dir.files, covered
                ),
            );
        }
    } else {
        for path in changed {
            push(path.clone(), "git-changed file".to_string());
        }
        if let Some(frame) = frame {
            for touch in &frame.touches {
                push(touch.clone(), "task frame touch".to_string());
            }
        }
        for known in known {
            if let Some(path) = explore_source_path(&known.memory) {
                push(path, "existing memory source: verify or update".to_string());
            }
        }
        for area in scan.areas.iter().take(12) {
            if explore_coverage(active, &area.path) == 0 {
                push(
                    area.path.clone(),
                    format!("uncovered nested area: {} source files", area.sources),
                );
            }
        }
        for dir in &scan.top_dirs {
            if explore_coverage(active, &dir.path) == 0 {
                push(
                    dir.path.clone(),
                    format!("uncovered area: {} source files", dir.sources),
                );
            }
        }
    }
    drop(push);
    targets.truncate(28);
    if targets.is_empty() {
        targets.push(ExploreTarget {
            path: ".".to_string(),
            reason: "repository root".to_string(),
        });
    }
    targets
}

pub(crate) fn explore_mode(
    requested: &str,
    active_count: usize,
    changed: &[String],
    frame: Option<&TaskFrame>,
    explicit_query: bool,
) -> Result<&'static str> {
    match requested {
        "macro" => Ok("macro"),
        "micro" => Ok("micro"),
        "auto" => {
            if active_count == 0 {
                return Ok("macro");
            }
            let touches = frame.is_some_and(|frame| !frame.touches.is_empty());
            if !changed.is_empty() || touches || explicit_query {
                Ok("micro")
            } else {
                Ok("macro")
            }
        }
        other => bail!("invalid explore mode {other:?}; expected auto, macro, or micro"),
    }
}

pub(crate) fn explore_protocol(mode: &str) -> &'static str {
    if mode == "macro" {
        "1. Read manifests and entry points first; do not walk the whole repository.\n\
         2. Map the top-level packages/modules and the boundary between them.\n\
         3. Identify build, test, and release commands, including required environment.\n\
         4. Trace one or two critical runtime paths from an entry point to storage/network.\n\
         5. Record durable architecture facts, constraints, procedures, and non-obvious edge cases.\n\
         6. Skip README marketing, file-by-file inventories, and generic summaries."
    } else {
        "1. Start with TARGETS: changed files, frame touches, and existing memory sources.\n\
         2. Read only those files and their direct callers, callees, and tests.\n\
         3. Compare existing memories on those paths; supersede stale behavior and forget only proven dead claims.\n\
         4. Record delta facts: changed invariants, edge cases, commands, and caller contracts.\n\
         5. Do not re-summarize unchanged modules or repeat the whole architecture."
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ExplorePromptInput<'a> {
    pub(crate) mode: &'a str,
    pub(crate) root: &'a Path,
    pub(crate) task: Option<&'a str>,
    pub(crate) query: &'a str,
    pub(crate) evidence_id: &'a str,
    pub(crate) scan: &'a RepoScan,
    pub(crate) changed: &'a [String],
    pub(crate) known: &'a [ExploreKnown],
    pub(crate) active_count: usize,
    pub(crate) anchored_count: usize,
    pub(crate) targets: &'a [ExploreTarget],
    pub(crate) max_items: usize,
}

pub(crate) fn build_explore_prompt(input: ExplorePromptInput<'_>) -> String {
    let ExplorePromptInput {
        mode,
        root,
        task,
        query,
        evidence_id,
        scan,
        changed,
        known,
        active_count,
        anchored_count,
        targets,
        max_items,
    } = input;
    let mut out = String::new();
    let mut line = |text: &str| {
        out.push_str(text);
        out.push('\n');
    };
    line("You are exploring a repository to build cm, a compact source-grounded memory for coding agents.");
    line("Return only strict JSONL memory operations. No markdown, no prose, no analysis.");
    line("");
    line("EXPLORATION");
    line(&format!("MODE: {mode}"));
    line(&format!("ROOT: {}", display_explore_root(root)));
    line(&format!("TASK: {}", task.unwrap_or("(none)")));
    line(&format!("QUERY: {query}"));
    line(&format!("EVIDENCE_ID: {evidence_id}"));
    line(&format!("MAX_RECORDS: {max_items}"));
    line("");
    line("REPOSITORY SNAPSHOT");
    line(&format!(
        "files: {} (source files: {})",
        scan.files, scan.source_files
    ));
    if !scan.manifests.is_empty() {
        line("manifests:");
        for manifest in &scan.manifests {
            line(&format!("- {manifest}"));
        }
    }
    if !scan.entrypoints.is_empty() {
        line("entry points:");
        for entrypoint in &scan.entrypoints {
            line(&format!("- {entrypoint}"));
        }
    }
    if !scan.top_dirs.is_empty() {
        line("top-level areas:");
        for dir in &scan.top_dirs {
            line(&format!(
                "- {}: {} files, {} source files",
                dir.path, dir.files, dir.sources
            ));
        }
    }
    if !scan.areas.is_empty() {
        line("nested source areas:");
        for area in scan.areas.iter().take(16) {
            line(&format!("- {}: {} source files", area.path, area.sources));
        }
    }
    if !changed.is_empty() {
        line("git changes:");
        for path in changed.iter().take(40) {
            line(&format!("- {path}"));
        }
    }
    line("");
    line("EXISTING MEMORY");
    line(&format!(
        "active records: {active_count}; source-anchored: {anchored_count}"
    ));
    if known.is_empty() {
        line("none; this is a first exploration.");
    } else {
        line("known records (use their ids for supersede/forget/evidence):");
        for known in known {
            line(&format!(
                "- {} | {} | {} | {} | {}",
                known.memory.id,
                known.memory.kind,
                known.memory.subject,
                known.memory.source,
                known.preview.replace('\n', " ")
            ));
        }
    }
    line("");
    line("TARGETS");
    for target in targets {
        line(&format!("- {}: {}", target.path, target.reason));
    }
    line("");
    line("COVERAGE");
    if mode == "macro" {
        line("- Cover every top-level area that has durable behavior with at least one record; do not spend the whole budget on one package.");
        line("- Use the full MAX_RECORDS budget for distinct high-value claims, not variations of one claim.");
    } else {
        line("- Cover each changed or touched target that has a durable delta; do not re-summarize untouched modules.");
        line("- Prefer several precise deltas over one broad module summary.");
    }
    line("");
    line("PROTOCOL");
    for step in explore_protocol(mode).lines() {
        line(step);
    }
    line("");
    line("QUALITY BAR");
    line("- One claim per record; 1-3 sentences; text <=800 characters.");
    line("- Prefer exact identifiers, paths, commands, invariants, and edge cases; no generic advice.");
    line("- source must point at the line that proves the claim: path:symbol. Use a bare path only when the whole file is the claim.");
    line("- Do not restate README/docs, invent symbols, or record file-by-file inventories.");
    line("- Do not duplicate a known subject: use supersede with the existing id instead.");
    line("- If nothing durable is learned, return an empty response. Prefer 0 records over low-value noise.");
    line("");
    line("OUTPUT");
    line("Exactly one JSON object per line, one of:");
    line("{\"op\":\"remember\",\"kind\":\"fact|decision|preference|constraint|episode|procedure\",\"scope\":\"...\",\"subject\":\"...\",\"text\":\"<=800 chars\",\"source\":\"path:symbol\",\"evidence\":[\"<id>\"],\"confidence\":0.0}");
    line("{\"op\":\"supersede\",\"kind\":\"...\",\"scope\":\"...\",\"subject\":\"...\",\"text\":\"...\",\"source\":\"path:symbol\",\"supersedes\":[\"<existing-id>\"],\"evidence\":[\"<id>\"]}");
    line("{\"op\":\"forget\",\"id\":\"<existing-id>\",\"reason\":\"...\"}");
    line("");
    line(&format!(
        "For every new observation, evidence must contain \"{evidence_id}\" or a specific known-memory id."
    ));
    line("Never create kind=frame or kind=feedback. Never invent a source. Never use a reserved __cm_* scope.");
    out
}
