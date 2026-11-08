use std::collections::{HashMap, HashSet};

use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::brief::{
    brief_prompt, collect_feedback_stats, json_tokens, BriefMemory, Feedback, FeedbackStats,
    FEEDBACK_KIND,
};
use crate::explore::scan_repo;
use crate::explore_api::{
    build_explore_prompt, changed_paths, collect_explore_known, collect_explore_targets,
    display_explore_root, ensure_explore_frame, explore_mode, mechanical_explore,
    ExplorePromptInput,
};
use crate::frames::{
    append_values, collect_frames, ensure_frame_owner, frame_json, frame_query,
    frame_query_for_task, frame_subject, latest_frame, parse_frame, save_frame, uncovered_touches,
    TaskFrame, FRAME_KIND,
};
use crate::model::{
    build_merged, cmx_line, content_hash, estimate_tokens, kind_id, memory_from_input,
    memory_payload, minhash_signature, minhash_similarity, pair_similarity, parse_ids, preview,
    validate_input, validate_memory, validate_trust, AppendInput, Dsu, ImportLine, MemoryOut,
};
use crate::mutate::write_feedback;
use crate::search::{active_memories, search_out, select_context, RecallQuery, SearchQuery};
use crate::source::{anchor_text, source_anchor, source_parts, source_snippet};
use crate::store::{
    append_record, create_store, find_tail_duplicate, read_header_file, reindex_file, Store,
};

const SKILL_TEXT: &str = include_str!("../assets/SKILL.md");

macro_rules! outln {
    ($($arg:tt)*) => {{
        let _ = writeln!(io::stdout(), $($arg)*);
    }};
}

#[derive(Serialize)]
struct CandidateOut {
    id: String,
    bm25: f64,
    minhash: f64,
    subject: String,
    preview: String,
}

#[derive(Serialize)]
struct ContextOut {
    query: String,
    budget: usize,
    estimated_tokens: usize,
    compact: bool,
    memories: Vec<MemoryOut>,
}

#[derive(Serialize)]
struct BriefOut {
    frame: Option<serde_json::Value>,
    query: String,
    budget: usize,
    estimated_tokens: usize,
    memories: Vec<BriefMemory>,
}

#[derive(Serialize)]
struct PackMemory {
    id: String,
    kind: String,
    scope: String,
    subject: String,
    text: String,
    source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    snippet: Option<String>,
}

impl From<&crate::model::Memory> for PackMemory {
    fn from(memory: &crate::model::Memory) -> Self {
        Self {
            id: memory.id.clone(),
            kind: memory.kind.clone(),
            scope: memory.scope.clone(),
            subject: memory.subject.clone(),
            text: memory.text.clone(),
            source: memory.source.clone(),
            snippet: None,
        }
    }
}

#[derive(Deserialize, Serialize)]
struct AbsorbOut {
    ok: bool,
    accepted: usize,
    written: usize,
    duplicates: usize,
    forgotten: usize,
    dry_run: bool,
}

#[derive(Serialize)]
struct SimilarOut {
    id: String,
    candidates: Vec<CandidateOut>,
}

#[derive(Serialize)]
struct MergeGroupOut {
    sources: Vec<String>,
    strategy: String,
    score: f64,
    text: String,
}

#[derive(Serialize)]
struct ConsolidateOut {
    groups: Vec<MergeGroupOut>,
    applied: bool,
}

#[derive(Serialize)]
pub(crate) struct ConflictGroupOut {
    pub(crate) scope: String,
    pub(crate) kind: String,
    pub(crate) subject: String,
    pub(crate) records: Vec<MemoryOut>,
}

pub fn run() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    let Some(command) = args.first().map(String::as_str) else {
        print_usage();
        return Ok(());
    };
    match command {
        "init" => cmd_init(&arg_new_path(&args), has_flag(&args, "--empty")),
        "append" => {
            let input =
                arg_value(&args, "--json").ok_or_else(|| anyhow!("append requires --json"))?;
            cmd_append(&arg_path(&args), &input, has_flag(&args, "--durable"))
        }
        "remember" => {
            let subject = arg_value(&args, "--subject")
                .ok_or_else(|| anyhow!("remember requires --subject"))?;
            let text =
                arg_value(&args, "--text").ok_or_else(|| anyhow!("remember requires --text"))?;
            let input = AppendInput {
                kind: arg_value(&args, "--kind").unwrap_or_else(|| "fact".to_string()),
                scope: arg_value(&args, "--scope").unwrap_or_else(|| "repo".to_string()),
                subject,
                text,
                source: arg_value(&args, "--source").unwrap_or_else(|| "agent".to_string()),
                confidence: arg_f64(&args, "--confidence", 0.9),
                valid_from: 0,
                valid_to: None,
                supersedes: arg_value(&args, "--supersedes")
                    .map(|value| parse_ids(&value))
                    .unwrap_or_default(),
                conflict_of: None,
                trust: arg_value(&args, "--trust"),
                source_commit: arg_value(&args, "--source-commit"),
            };
            let replace = has_flag(&args, "--replace");
            if replace && has_flag(&args, "--force") {
                bail!("--replace and --force are mutually exclusive");
            }
            if has_flag(&args, "--verify-source")
                && source_parts(&input.source).is_some()
                && source_anchor(&input.source).is_none()
            {
                bail!("source does not anchor: {:?}", input.source);
            }
            cmd_remember_input(
                &arg_path(&args),
                &input,
                has_flag(&args, "--durable"),
                has_flag(&args, "--force"),
                replace,
            )
        }
        "recall" => {
            let task = arg_value(&args, "--task");
            let query = match arg_value(&args, "--query") {
                Some(query) => query,
                None => {
                    let task = task
                        .as_deref()
                        .ok_or_else(|| anyhow!("recall requires --query or --task with a frame"))?;
                    frame_query_for_task(&arg_path(&args), task)?
                }
            };
            let snippet_lines = arg_usize(&args, "--snippets", 0);
            let format = arg_value(&args, "--format").unwrap_or_else(|| {
                if snippet_lines > 0 {
                    "json".to_string()
                } else {
                    "cmx".to_string()
                }
            });
            let root = PathBuf::from(arg_value(&args, "--root").unwrap_or_else(|| ".".to_string()));
            cmd_context(
                &arg_path(&args),
                &query,
                arg_usize(&args, "--budget", 800),
                arg_usize(&args, "--top", 16),
                arg_value(&args, "--scope"),
                task,
                arg_value(&args, "--kind"),
                Some(arg_value(&args, "--min-trust").unwrap_or_else(|| "authored".to_string())),
                format,
                arg_usize(&args, "--max-items", 3),
                arg_f64(&args, "--min-score", 0.0),
                !has_flag(&args, "--full"),
                snippet_lines,
                &root,
            )
        }
        "status" => cmd_verify(&arg_path(&args), false),
        "cache-stats" => cmd_cache_stats(&arg_path(&args)),
        "frame" => cmd_frame(&arg_path(&args), &args),
        "claim" => cmd_claim(&arg_path(&args), &args),
        "release" => cmd_release(&arg_path(&args), &args),
        "finish" => cmd_finish(&arg_path(&args), &args),
        "feedback" | "use" => cmd_feedback(&arg_path(&args), &args),
        "govern" => cmd_govern(&arg_path(&args), &args),
        "audit" => cmd_audit(&arg_path(&args), &args),
        "brief" => cmd_brief(&arg_path(&args), &args),
        "explore" => cmd_explore(&arg_path(&args), &args),
        "wiki" => cmd_wiki(&args),
        "pack" => cmd_pack(&arg_path(&args), &args),
        "absorb" => cmd_absorb(&arg_path(&args), &args),
        "checkpoint" => cmd_checkpoint(&arg_path(&args), has_flag(&args, "--in-place")),
        "doctor" => {
            let ok = cmd_doctor(&arg_path(&args))?;
            if !ok && has_flag(&args, "--strict") {
                process::exit(1);
            }
            Ok(())
        }
        "import" => cmd_import(&arg_path(&args)),
        "export" => cmd_export(
            &arg_path(&args),
            arg_value(&args, "--format").unwrap_or_else(|| "cmx".to_string()),
        ),
        "reindex" => cmd_reindex(&arg_path(&args)),
        "skill" => cmd_skill(
            has_flag(&args, "--json"),
            has_flag(&args, "--install"),
            has_flag(&args, "--check"),
            arg_value(&args, "--dir"),
        ),
        "install" => crate::install::cmd_install(&args, SKILL_TEXT),
        "uninstall" => crate::install::cmd_uninstall(&args, SKILL_TEXT),
        "get" => {
            let id = arg_value(&args, "--id").ok_or_else(|| anyhow!("get requires --id"))?;
            cmd_get(&arg_path(&args), &id)
        }
        "scan" => cmd_scan(
            &arg_path(&args),
            arg_usize(&args, "--limit", 100),
            arg_usize(&args, "--offset", 0),
        ),
        "search" => {
            let query =
                arg_value(&args, "--query").ok_or_else(|| anyhow!("search requires --query"))?;
            cmd_search(
                &arg_path(&args),
                &query,
                arg_usize(&args, "--top", 8),
                arg_value(&args, "--scope"),
                arg_value(&args, "--task"),
                arg_value(&args, "--kind"),
                arg_f64(&args, "--min-score", 0.0),
                arg_value(&args, "--min-trust"),
                has_flag(&args, "--include-expired"),
            )
        }
        "context" => {
            let query =
                arg_value(&args, "--query").ok_or_else(|| anyhow!("context requires --query"))?;
            let snippet_lines = arg_usize(&args, "--snippets", 0);
            let format = arg_value(&args, "--format").unwrap_or_else(|| {
                if snippet_lines > 0 {
                    "json".to_string()
                } else {
                    "cmx".to_string()
                }
            });
            let root = PathBuf::from(arg_value(&args, "--root").unwrap_or_else(|| ".".to_string()));
            cmd_context(
                &arg_path(&args),
                &query,
                arg_usize(&args, "--budget", 2000),
                arg_usize(&args, "--top", 32),
                arg_value(&args, "--scope"),
                arg_value(&args, "--task"),
                arg_value(&args, "--kind"),
                arg_value(&args, "--min-trust"),
                format,
                arg_usize(&args, "--max-items", 5),
                arg_f64(&args, "--min-score", 0.0),
                has_flag(&args, "--compact"),
                snippet_lines,
                &root,
            )
        }
        "similar" => {
            let id = arg_value(&args, "--id").ok_or_else(|| anyhow!("similar requires --id"))?;
            cmd_similar(
                &arg_path(&args),
                &id,
                arg_usize(&args, "--top", 16),
                arg_value(&args, "--scope"),
            )
        }
        "merge" => {
            let sources =
                arg_value(&args, "--sources").ok_or_else(|| anyhow!("merge requires --sources"))?;
            cmd_merge(&arg_path(&args), &sources, arg_value(&args, "--text"))
        }
        "forget" => {
            let id = arg_value(&args, "--id").ok_or_else(|| anyhow!("forget requires --id"))?;
            cmd_forget(&arg_path(&args), &id)
        }
        "purge" => cmd_purge(&arg_path(&args)),
        "conflicts" => cmd_conflicts(&arg_path(&args)),
        "verify" => cmd_verify(&arg_path(&args), has_flag(&args, "--full")),
        "help" | "-h" | "--help" => {
            print_usage();
            Ok(())
        }
        "version" | "-V" | "--version" => {
            outln!("cm {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        other => bail!("unknown command {other:?}; try cm --help"),
    }
}

fn print_usage() {
    println!(
        "cm {} - compact memory for AI agents
         
         cm remember --file cm --subject <subject> --text <text> [--kind fact] [--scope repo] [--source <source>] [--trust authored] [--verify-source] [--replace]
         cm recall --file cm --query <text> [--scope <scope>] [--task <task>] [--budget 800] [--snippets <n>] [--root <dir>]
         cm forget --file cm --id <id>
         cm status [--file cm]
         cm cache-stats [--file cm]
         cm frame set --task <id> --goal <text> [--next <text>] [--open <text>]
         cm frame get --task <id>
         cm frame list
         cm brief [--task <id>|current] [--budget 1200] [--format json|prompt] [--snippets <n>] [--root <dir>]
         cm explore [--task <id>|current] [--query <text>] [--mode auto|macro|micro] [--budget 2000] [--max-items <n>] [--root <dir>] [--format prompt|json]
         cm explore --mechanical [--apply] [--format json|jsonl]
         cm explore --apply result.jsonl [--dry-run] [--replace] [--allow-long-text]
         cm wiki plan [--root <dir>] [--out .cm/wiki] [--max-modules <n>]
         cm wiki prompt|apply|review|status|next [--root <dir>] [--out .cm/wiki] [--page <id>] [--input <file|->]
         cm wiki review --page <id> --review-cmd <command> [--root <dir>] [--out .cm/wiki]
         cm wiki check [--root <dir>] [--out .cm/wiki] [--strict]
         cm wiki find --query <text> [--limit 5] [--format text|json]
         cm wiki route --query <text> [--budget 800] [--max-sections 2] [--task-size auto|small|medium|large] [--format text|json]
         cm wiki excerpt --page <id> --query <text> [--budget 600] [--max-sections 2] [--format text|json]
         cm wiki remember [--file cm] [--page <id>] [--scope repo] [--no-replace] [--include-unreviewed]
         cm wiki generate --agent-cmd <command> [--review-cmd <command>] [--review-rounds 2] [--root <dir>] [--out .cm/wiki] [--max-pages <n>] [--page <id>]
         cm pack [--task <id>|current] [--budget 1200] [--snippets <n>] [--root <dir>] > pack.prompt
         cm absorb --input result.jsonl [--dry-run] [--replace] [--allow-long-text]
         cm claim --task <id>|current --owner <id> [--lease 30]
         cm release --task <id>|current [--owner <id>]
         cm finish [--task <id>|current] [--summary <text>] [--root <dir>] [--changed] [--strict] [--apply]
         cm feedback --id <id> [--signal useful|stale|wrong]
         cm govern [--apply] [--merge] [--stale-days 90]
         cm audit [--root <dir>] [--changed] [--scope <scope>] [--limit <n>] [--refresh] [--apply] [--strict]
         cm doctor [--strict] [--file cm]
         cm install [--all|--detected|--agent <id>] [--list] [--no-exe] [--no-path] [--dry-run]
         cm uninstall [--all|--agent <id>] [--keep-exe]
         cm skill [--json] [--install] [--check]
         
         Internal/advanced commands: see README.",
        env!("CARGO_PKG_VERSION")
    );
}

pub(crate) fn cmd_init(path: &Path, empty: bool) -> Result<()> {
    let records = create_store(path, empty)?;
    outln!(
        "{}",
        json!({"ok": true, "file": path.display().to_string(), "records": records})
    );
    Ok(())
}

fn cmd_append(path: &Path, input_json: &str, durable: bool) -> Result<()> {
    let input: AppendInput = serde_json::from_str(input_json).context("parse --json input")?;
    cmd_append_input(path, &input, durable)
}

pub(crate) fn cmd_append_input(path: &Path, input: &AppendInput, durable: bool) -> Result<()> {
    validate_input(input)?;
    read_header_file(path)?;
    let memory = memory_from_input(input)?;
    append_record(path, 2, &memory_payload(&memory), durable)?;
    outln!("{}", json!({"ok": true, "id": memory.id}));
    Ok(())
}

pub(crate) fn cmd_remember_input(
    path: &Path,
    input: &AppendInput,
    durable: bool,
    force: bool,
    replace: bool,
) -> Result<()> {
    validate_input(input)?;
    read_header_file(path)?;
    let mut memory = memory_from_input(input)?;
    if replace && memory.supersedes.is_empty() {
        if let Some(existing) =
            crate::store::find_subject_memory(path, &memory.scope, &memory.kind, &memory.subject)?
        {
            memory.supersedes = vec![existing.id.clone()];
        }
    }
    if !force {
        if let Some(id) = find_tail_duplicate(path, &memory)? {
            outln!("{}", json!({"ok": true, "id": id, "duplicate": true}));
            return Ok(());
        }
    }
    append_record(path, 2, &memory_payload(&memory), durable)?;
    outln!(
        "{}",
        json!({
            "ok": true,
            "id": memory.id,
            "supersedes": memory.supersedes
        })
    );
    Ok(())
}

fn cmd_export(path: &Path, format: String) -> Result<()> {
    let mut store = Store::open(path)?;
    let memories = store.scan_page(usize::MAX, 0)?;
    match format.as_str() {
        "cmx" => {
            for memory in &memories {
                outln!("{}", cmx_line(memory));
            }
        }
        "jsonl" => {
            for memory in &memories {
                outln!("{}", serde_json::to_string(&MemoryOut::from(memory))?);
            }
        }
        other => bail!("invalid export format {other:?}; expected cmx or jsonl"),
    }
    Ok(())
}

fn cmd_import(path: &Path) -> Result<()> {
    read_header_file(path)?;
    let store_lock = crate::store::acquire_store_lock(path)?;
    // The store lock file is the writer mutex; a byte-range lock on the store
    // itself would break concurrent readers on Windows.
    let file = OpenOptions::new().read(true).append(true).open(path)?;
    let mut writer = BufWriter::with_capacity(1024 * 1024, file);
    let mut count = 0u64;
    for line in io::stdin().lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let parsed: ImportLine = serde_json::from_str(&line).context("parse import line")?;
        let memory = match parsed {
            ImportLine::Full(full) => {
                if full.id.trim().is_empty() {
                    bail!("imported record has empty id");
                }
                validate_memory(&full.kind, &full.scope, &full.subject, &full.text)?;
                if let Some(trust) = &full.trust {
                    validate_trust(trust)?;
                }
                full.into_memory()
            }
            ImportLine::Append(input) => memory_from_input(&input)?,
        };
        let record_type = if memory.merged { 3 } else { 2 };
        writer.write_all(&crate::store::encode_record(
            record_type,
            0,
            &memory_payload(&memory),
        )?)?;
        count += 1;
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    let file = writer
        .into_inner()
        .map_err(|error| anyhow!("flush import: {error}"))?;
    drop(file);
    drop(store_lock);
    reindex_file(path)?;
    outln!(
        "{}",
        json!({"ok": true, "imported": count, "reindexed": true})
    );
    Ok(())
}

fn cmd_skill(json_output: bool, install: bool, check: bool, dir: Option<String>) -> Result<()> {
    let explicit_dir = dir.is_some();
    let dir = PathBuf::from(dir.unwrap_or_else(|| ".agents/skills/cm-memory".to_string()));
    let path = if check && !explicit_dir && Path::new("assets/SKILL.md").exists() {
        PathBuf::from("assets/SKILL.md")
    } else {
        dir.join("SKILL.md")
    };

    if install {
        fs::create_dir_all(&dir)?;
        fs::write(&path, SKILL_TEXT.as_bytes())?;
        let result = json!({
            "ok": true,
            "installed": path.display().to_string(),
            "bytes": SKILL_TEXT.len()
        });
        if json_output {
            outln!("{}", result);
        } else {
            outln!("installed {}", path.display());
        }
        return Ok(());
    }

    if check {
        let existing = fs::read(&path).ok();
        let ok = existing.as_deref() == Some(SKILL_TEXT.as_bytes());
        outln!(
            "{}",
            json!({
                "ok": ok,
                "path": path.display().to_string(),
                "embedded_bytes": SKILL_TEXT.len(),
                "file_bytes": existing.as_ref().map(|bytes| bytes.len())
            })
        );
        if !ok {
            bail!("skill file differs from embedded skill: {}", path.display());
        }
        return Ok(());
    }

    if json_output {
        outln!(
            "{}",
            json!({
                "name": "cm-memory",
                "description": "cm binary memory recall/write protocol",
                "skill": SKILL_TEXT
            })
        );
    } else {
        let mut stdout = io::stdout();
        let _ = stdout.write_all(SKILL_TEXT.as_bytes());
        let _ = stdout.flush();
    }
    Ok(())
}

pub(crate) fn cmd_checkpoint(path: &Path, in_place: bool) -> Result<()> {
    let header = crate::store::checkpoint_file(path, in_place)?;
    outln!(
        "{}",
        json!({
            "ok": true,
            "mode": if in_place { "in-place" } else { "compact" },
            "generation": header.generation,
            "doc_count": header.doc_count,
            "blocks": header.block_count,
            "index_bytes": header.index_len,
            "tail_start": header.tail_start
        })
    );
    Ok(())
}

pub(crate) fn cmd_reindex(path: &Path) -> Result<()> {
    let header = reindex_file(path)?;
    outln!(
        "{}",
        json!({
            "ok": true,
            "doc_count": header.doc_count,
            "index_bytes": header.index_len,
            "blocks": header.block_count
        })
    );
    Ok(())
}

pub(crate) fn cmd_get(path: &Path, id: &str) -> Result<()> {
    let mut store = Store::open(path)?;
    let tail = store.tail_view()?;
    if tail.hidden.contains(id) {
        bail!("memory not found: {id}");
    }
    let memory = if let Some((_, memory)) = store.find_doc(id)? {
        memory
    } else {
        tail.get(id)
            .cloned()
            .ok_or_else(|| anyhow!("memory not found: {id}"))?
    };
    outln!("{}", serde_json::to_string(&MemoryOut::from(&memory))?);
    Ok(())
}

fn cmd_scan(path: &Path, limit: usize, offset: usize) -> Result<()> {
    let mut store = Store::open(path)?;
    for memory in store.scan_page(limit.min(5000), offset)? {
        outln!("{}", cmx_line(&memory));
    }
    Ok(())
}

fn cmd_cache_stats(path: &Path) -> Result<()> {
    let store = Store::open(path)?;
    let stats = store.cache_stats()?;
    outln!(
        "{}",
        json!({
            "ok": true,
            "bytes": stats.bytes,
            "capacity": stats.capacity,
            "entries": stats.entries,
            "data_entries": stats.data_entries,
            "index_entries": stats.index_entries,
            "hits": stats.hits,
            "misses": stats.misses,
            "evictions": stats.evictions
        })
    );
    Ok(())
}

fn cmd_frame(path: &Path, args: &[String]) -> Result<()> {
    match args.get(1).map(String::as_str) {
        Some("set") => cmd_frame_set(path, args),
        Some("get") => cmd_frame_get(path, args),
        Some("list") => cmd_frame_list(path),
        other => bail!("frame expects set, get, or list; got {other:?}"),
    }
}

fn frame_task(args: &[String]) -> Result<String> {
    arg_value(args, "--task").ok_or_else(|| anyhow!("frame requires --task"))
}

fn cmd_frame_set(path: &Path, args: &[String]) -> Result<()> {
    let task = frame_task(args)?;
    let _lock = crate::store::acquire_store_lock(path)?;
    let mut store = Store::open(path)?;
    let existing = collect_frames(&mut store, 64)?
        .into_iter()
        .find(|memory| memory.subject == frame_subject(&task));
    let existing_id = existing.as_ref().map(|memory| memory.id.clone());
    if let Some(existing_frame) = existing.as_ref().and_then(parse_frame) {
        ensure_frame_owner(
            &existing_frame,
            arg_value(args, "--owner").as_deref(),
            "update",
        )?;
    }
    let mut frame = existing
        .as_ref()
        .and_then(parse_frame)
        .unwrap_or_else(|| TaskFrame {
            task: task.clone(),
            goal: String::new(),
            status: "active".to_string(),
            constraints: Vec::new(),
            next: Vec::new(),
            open: Vec::new(),
            touches: Vec::new(),
            notes: Vec::new(),
            owner: None,
            lease_until: None,
            updated_at: 0,
        });
    frame.task = task.clone();
    if let Some(goal) = arg_value(args, "--goal") {
        frame.goal = goal;
    }
    if frame.goal.trim().is_empty() {
        bail!("frame requires --goal for a new task");
    }
    if let Some(status) = arg_value(args, "--status") {
        if !matches!(status.as_str(), "active" | "paused" | "done") {
            bail!("invalid frame status {status:?}; expected active/paused/done");
        }
        frame.status = status;
        if frame.status == "done" {
            frame.owner = None;
            frame.lease_until = None;
        }
    }
    append_values(&mut frame.constraints, arg_values(args, "--constraint"));
    append_values(&mut frame.next, arg_values(args, "--next"));
    append_values(&mut frame.open, arg_values(args, "--open"));
    append_values(&mut frame.touches, arg_values(args, "--touch"));
    append_values(&mut frame.notes, arg_values(args, "--note"));
    frame.updated_at = crate::model::now_ms();

    let source = arg_value(args, "--source").unwrap_or_else(|| "agent".to_string());
    let memory = save_frame(
        path,
        &frame,
        existing_id,
        &source,
        arg_value(args, "--source-commit"),
        true,
    )?;
    outln!(
        "{}",
        json!({"ok": true, "id": memory.id, "task": task, "status": frame.status})
    );
    Ok(())
}

fn cmd_frame_get(path: &Path, args: &[String]) -> Result<()> {
    let task = frame_task(args)?;
    let mut store = Store::open(path)?;
    let memory = collect_frames(&mut store, 64)?
        .into_iter()
        .find(|memory| memory.subject == frame_subject(&task));
    if let Some(memory) = memory {
        let frame = parse_frame(&memory)
            .ok_or_else(|| anyhow!("frame record {} has invalid payload", memory.id))?;
        outln!("{}", serde_json::to_string(&frame_json(&memory, &frame))?);
    } else {
        outln!("{}", json!({"task": task, "found": false}));
    }
    Ok(())
}

fn cmd_frame_list(path: &Path) -> Result<()> {
    let mut store = Store::open(path)?;
    let frames: Vec<_> = collect_frames(&mut store, 512)?
        .iter()
        .filter_map(|memory| parse_frame(memory).map(|frame| frame_json(memory, &frame)))
        .collect();
    outln!("{}", serde_json::to_string(&frames)?);
    Ok(())
}

fn cmd_claim(path: &Path, args: &[String]) -> Result<()> {
    let owner = arg_value(args, "--owner").ok_or_else(|| anyhow!("claim requires --owner"))?;
    let task = arg_value(args, "--task").unwrap_or_else(|| "current".to_string());
    let lease_minutes = arg_usize(args, "--lease", 30).max(1) as u64;
    let _lock = crate::store::acquire_store_lock(path)?;
    let mut store = Store::open(path)?;
    let (memory, mut frame) = latest_frame(&mut store, Some(&task))?
        .ok_or_else(|| anyhow!("no frame for task {task:?}"))?;
    let now = crate::model::now_ms();
    if let Some(existing) = frame.owner.as_deref() {
        if existing != owner && frame.lease_until.is_some_and(|end| end > now) {
            bail!(
                "task {:?} is held by {} until {}",
                frame.task,
                existing,
                frame.lease_until.unwrap_or(0)
            );
        }
    }
    let lease_until = now.saturating_add(lease_minutes.saturating_mul(60_000));
    frame.owner = Some(owner.clone());
    frame.lease_until = Some(lease_until);
    frame.updated_at = now;
    let saved = save_frame(path, &frame, Some(memory.id), "agent:claim", None, true)?;
    outln!(
        "{}",
        json!({
            "ok": true,
            "id": saved.id,
            "task": frame.task,
            "owner": owner,
            "lease_until": lease_until
        })
    );
    Ok(())
}

fn cmd_release(path: &Path, args: &[String]) -> Result<()> {
    let task = arg_value(args, "--task").unwrap_or_else(|| "current".to_string());
    let owner = arg_value(args, "--owner");
    let _lock = crate::store::acquire_store_lock(path)?;
    let mut store = Store::open(path)?;
    let (memory, mut frame) = latest_frame(&mut store, Some(&task))?
        .ok_or_else(|| anyhow!("no frame for task {task:?}"))?;
    let now = crate::model::now_ms();
    if let (Some(requested), Some(existing)) = (owner.as_deref(), frame.owner.as_deref()) {
        if requested != existing && frame.lease_until.is_some_and(|end| end > now) {
            bail!("task {:?} is held by {}", frame.task, existing);
        }
    }
    frame.owner = None;
    frame.lease_until = None;
    frame.updated_at = now;
    let saved = save_frame(path, &frame, Some(memory.id), "agent:release", None, true)?;
    outln!(
        "{}",
        json!({"ok": true, "id": saved.id, "task": frame.task, "released": true})
    );
    Ok(())
}

fn cmd_finish(path: &Path, args: &[String]) -> Result<()> {
    let task = arg_value(args, "--task").unwrap_or_else(|| "current".to_string());
    let explicit_summary = arg_value(args, "--summary");
    let root = PathBuf::from(arg_value(args, "--root").unwrap_or_else(|| ".".to_string()));
    let apply = has_flag(args, "--apply");
    let strict = has_flag(args, "--strict");
    let changed_only = has_flag(args, "--changed");
    let owner = arg_value(args, "--owner");
    let _lock = crate::store::acquire_store_lock(path)?;
    let mut store = Store::open(path)?;
    let (memory, mut frame) = latest_frame(&mut store, Some(&task))?
        .ok_or_else(|| anyhow!("no frame for task {task:?}"))?;
    ensure_frame_owner(&frame, owner.as_deref(), "finish")?;
    if strict && !frame.open.is_empty() {
        bail!(
            "task {:?} still has {} open question(s); resolve them or omit --strict",
            frame.task,
            frame.open.len()
        );
    }
    if strict {
        if !frame.touches.is_empty() {
            let memories = active_memories(&mut store)?;
            let uncovered = uncovered_touches(&frame, &memories);
            if !uncovered.is_empty() {
                bail!(
                    "task {:?} touches {} path(s) with no cited memory: {}",
                    frame.task,
                    uncovered.len(),
                    uncovered.join(", ")
                );
            }
        }
        let audit = run_audit(path, &root, false, false, changed_only, None, None)?;
        let issue_count = audit["issue_count"].as_u64().unwrap_or(0);
        if issue_count > 0 {
            bail!(
                "task {:?} has {} source issue(s); repair or omit --strict",
                frame.task,
                issue_count
            );
        }
    }
    let summary = explicit_summary
        .clone()
        .unwrap_or_else(|| format!("Completed: {}", frame.goal));
    frame.status = "done".to_string();
    frame.owner = None;
    frame.lease_until = None;
    frame.updated_at = crate::model::now_ms();
    let note = format!("finished: {summary}");
    if !frame.notes.contains(&note) {
        frame.notes.push(note);
    }
    let frame_memory = save_frame(path, &frame, Some(memory.id), "agent:finish", None, true)?;

    let input = AppendInput {
        kind: "episode".to_string(),
        scope: arg_value(args, "--scope").unwrap_or_else(|| "repo".to_string()),
        subject: format!("task:{} summary", frame.task),
        text: summary.clone(),
        source: "agent:finish".to_string(),
        confidence: 0.9,
        valid_from: 0,
        valid_to: None,
        supersedes: Vec::new(),
        conflict_of: None,
        trust: Some("authored".to_string()),
        source_commit: None,
    };
    let summary_memory = memory_from_input(&input)?;
    crate::store::append_record_unlocked(
        path,
        crate::store::T_MEMORY,
        &memory_payload(&summary_memory),
        true,
    )?;
    let summary_id = summary_memory.id;

    drop(store);
    drop(_lock);
    let audit = run_audit(path, &root, apply, false, changed_only, None, None)?;
    let govern = run_govern(path, apply, false, 90, 2, 0.85)?;
    outln!(
        "{}",
        serde_json::to_string(&json!({
            "ok": true,
            "task": frame.task,
            "status": frame.status,
            "frame_id": frame_memory.id,
            "summary_id": summary_id,
            "strict": strict,
            "changed_only": changed_only,
            "audit": audit,
            "govern": govern
        }))?
    );
    Ok(())
}

fn cmd_feedback(path: &Path, args: &[String]) -> Result<()> {
    let ids = arg_values(args, "--id");
    if ids.is_empty() {
        bail!("feedback requires at least one --id");
    }
    let signal = arg_value(args, "--signal").unwrap_or_else(|| "useful".to_string());
    if !matches!(signal.as_str(), "useful" | "stale" | "wrong") {
        bail!("invalid feedback signal {signal:?}; expected useful/stale/wrong");
    }
    let note = arg_value(args, "--note");
    let source = arg_value(args, "--source").unwrap_or_else(|| "agent".to_string());
    for id in &ids {
        write_feedback(path, id, &signal, note.as_deref(), &source)?;
    }
    outln!(
        "{}",
        json!({"ok": true, "count": ids.len(), "signal": signal})
    );
    Ok(())
}

fn cmd_govern(path: &Path, args: &[String]) -> Result<()> {
    let report = run_govern(
        path,
        has_flag(args, "--apply"),
        has_flag(args, "--merge"),
        arg_usize(args, "--stale-days", 90) as u64,
        arg_usize(args, "--wrong-threshold", 2).max(1) as u32,
        arg_f64(args, "--merge-threshold", 0.85),
    )?;
    outln!("{}", serde_json::to_string(&report)?);
    Ok(())
}

fn run_govern(
    path: &Path,
    apply: bool,
    merge: bool,
    stale_days: u64,
    wrong_threshold: u32,
    merge_threshold: f64,
) -> Result<serde_json::Value> {
    let merged_groups = if merge {
        if apply {
            // Consolidation walks the index; fold tail records in first so a
            // governance pass also sees decisions written since the last
            // reindex.
            reindex_file(path)?;
        }
        run_consolidate(path, apply, 8, 5000, 0, merge_threshold)?
            .groups
            .len()
    } else {
        0
    };
    let mut store = Store::open(path)?;
    let memories = active_memories(&mut store)?;
    let mut feedback: HashMap<String, FeedbackStats> = HashMap::new();
    for memory in &memories {
        if memory.kind != FEEDBACK_KIND {
            continue;
        }
        let Ok(item) = serde_json::from_str::<Feedback>(&memory.text) else {
            continue;
        };
        let stats = feedback.entry(item.target).or_default();
        match item.signal.as_str() {
            "useful" => stats.useful += 1,
            "stale" => stats.stale += 1,
            "wrong" => stats.wrong += 1,
            _ => {}
        }
        stats.last = stats.last.max(item.updated_at);
    }
    let now = crate::model::now_ms();
    let mut duplicate_targets: HashMap<String, String> = HashMap::new();
    let mut by_content: HashMap<(String, String, String, String), Vec<&crate::model::Memory>> =
        HashMap::new();
    for memory in &memories {
        if matches!(memory.kind.as_str(), FRAME_KIND | FEEDBACK_KIND) {
            continue;
        }
        by_content
            .entry((
                memory.scope.clone(),
                memory.kind.clone(),
                memory.subject.clone(),
                content_hash(memory),
            ))
            .or_default()
            .push(memory);
    }
    for mut group in by_content.into_values() {
        if group.len() < 2 {
            continue;
        }
        group.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        for duplicate in &group[1..] {
            duplicate_targets.insert(
                duplicate.id.clone(),
                format!("exact duplicate of {}", group[0].id),
            );
        }
    }
    let conflicts = conflict_groups(&memories);
    let mut suggestions = Vec::new();
    for memory in &memories {
        if matches!(memory.kind.as_str(), FRAME_KIND | FEEDBACK_KIND) {
            continue;
        }
        let stats = feedback.get(&memory.id);
        let useful = stats.map_or(0, |s| s.useful);
        let stale = stats.map_or(0, |s| s.stale);
        let wrong = stats.map_or(0, |s| s.wrong);
        let age_days = now.saturating_sub(memory.created_at) / 86_400_000;
        let score = i64::from(useful) * 2 - i64::from(stale) - i64::from(wrong) * 3;
        let (action, reason) = if let Some(reason) = duplicate_targets.get(&memory.id) {
            ("forget", reason.as_str())
        } else if wrong >= wrong_threshold || score <= -3 {
            ("forget", "negative feedback")
        } else if stale >= 2 || (age_days >= stale_days && useful == 0) {
            ("verify", "stale or unverified")
        } else {
            continue;
        };
        suggestions.push(json!({
            "id": memory.id,
            "kind": memory.kind,
            "scope": memory.scope,
            "subject": memory.subject,
            "action": action,
            "reason": reason,
            "age_days": age_days,
            "score": score,
            "useful": useful,
            "stale": stale,
            "wrong": wrong
        }));
    }
    suggestions.sort_by(|a, b| {
        a["action"]
            .as_str()
            .cmp(&b["action"].as_str())
            .then_with(|| a["id"].as_str().cmp(&b["id"].as_str()))
    });
    suggestions.truncate(200);
    let mut forgotten = Vec::new();
    if apply {
        for suggestion in &suggestions {
            if suggestion["action"] == "forget" {
                if let Some(id) = suggestion["id"].as_str() {
                    if !forgotten.iter().any(|value| value == id) {
                        cmd_forget(path, id)?;
                        forgotten.push(id.to_string());
                    }
                }
            }
        }
    }
    Ok(json!({
        "ok": true,
        "apply": apply,
        "conflicts": conflicts.len(),
        "merged_groups": merged_groups,
        "suggestions": suggestions,
        "forgotten": forgotten
    }))
}

/// Relative paths reported by `git status --porcelain=v1 -z -uall`, normalized
/// to lowercase forward slashes. Paths are relative to the git repository root,
/// so `audit --changed` expects `--root` to be that root.
fn audit_source_issue(
    memory: &crate::model::Memory,
    full: &Path,
    symbol: Option<&str>,
) -> Option<&'static str> {
    if !full.exists() {
        return Some("missing_source");
    }
    if full.is_dir() {
        return None;
    }
    let text = match fs::read_to_string(full) {
        Ok(text) => text,
        Err(_) => return Some("unreadable_source"),
    };
    if let Some(symbol) = symbol {
        let mut present = text.contains(symbol);
        for separator in ["::", "."] {
            if let Some(last) = symbol
                .rsplit(separator)
                .next()
                .filter(|last| *last != symbol)
            {
                present = present || text.contains(last);
            }
        }
        if !present {
            return Some("missing_symbol");
        }
    }
    if let Some(expected) = &memory.source_hash {
        let Some((_, actual)) = anchor_text(&text, symbol) else {
            return Some("missing_symbol");
        };
        if actual != *expected {
            return Some("source_changed");
        }
    }
    None
}

fn cmd_audit(path: &Path, args: &[String]) -> Result<()> {
    let root = PathBuf::from(arg_value(args, "--root").unwrap_or_else(|| ".".to_string()));
    let apply = has_flag(args, "--apply");
    let refresh = has_flag(args, "--refresh");
    let changed = has_flag(args, "--changed");
    let scope = arg_value(args, "--scope");
    let limit = arg_value(args, "--limit").and_then(|value| value.parse::<usize>().ok());
    let strict = has_flag(args, "--strict");
    let report = run_audit(
        path,
        &root,
        apply,
        refresh,
        changed,
        scope.as_deref(),
        limit,
    )?;
    let unresolved = report["unresolved"].as_u64().unwrap_or(0);
    outln!("{}", serde_json::to_string(&report)?);
    if strict && unresolved > 0 {
        bail!("audit found {unresolved} unresolved source issue(s)");
    }
    Ok(())
}

fn run_audit(
    path: &Path,
    root: &Path,
    apply: bool,
    refresh: bool,
    changed_only: bool,
    scope: Option<&str>,
    limit: Option<usize>,
) -> Result<serde_json::Value> {
    let mut store = Store::open(path)?;
    let mut memories = active_memories(&mut store)?;
    if changed_only {
        let changed = changed_paths(root)?;
        memories.retain(|memory| {
            source_parts(&memory.source).is_some_and(|(relative, _)| {
                changed.contains(&relative.to_string_lossy().replace('\\', "/").to_lowercase())
            })
        });
    }
    if let Some(scope) = scope {
        memories.retain(|memory| memory.scope == scope);
    }
    if let Some(limit) = limit {
        memories.truncate(limit);
    }
    let now = crate::model::now_ms();
    let threads = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(1)
        .clamp(1, 16);
    let chunk_size = memories.len().div_ceil(threads).max(1);
    let parts: Vec<(usize, Vec<serde_json::Value>)> = if memories.is_empty() {
        Vec::new()
    } else {
        std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for chunk in memories.chunks(chunk_size) {
                let root = root.to_path_buf();
                handles.push(scope.spawn(move || {
                    let mut checked = 0usize;
                    let mut issues = Vec::new();
                    for memory in chunk {
                        if matches!(memory.kind.as_str(), FRAME_KIND | FEEDBACK_KIND) {
                            continue;
                        }
                        let Some((relative, symbol)) = source_parts(&memory.source) else {
                            continue;
                        };
                        checked += 1;
                        let full = root.join(&relative);
                        let Some(issue) = audit_source_issue(memory, &full, symbol.as_deref())
                        else {
                            continue;
                        };
                        let age_days = now.saturating_sub(memory.created_at) / 86_400_000;
                        issues.push(json!({
                            "id": memory.id,
                            "source": memory.source,
                            "path": full.display().to_string(),
                            "issue": issue,
                            "source_line": memory.source_line,
                            "age_days": age_days
                        }));
                    }
                    (checked, issues)
                }));
            }
            handles
                .into_iter()
                .map(|handle| handle.join().expect("audit worker panicked"))
                .collect()
        })
    };
    let checked: usize = parts.iter().map(|(checked, _)| checked).sum();
    let mut issues: Vec<serde_json::Value> =
        parts.into_iter().flat_map(|(_, issues)| issues).collect();
    issues.sort_by(|a, b| {
        a["id"]
            .as_str()
            .cmp(&b["id"].as_str())
            .then_with(|| a["issue"].as_str().cmp(&b["issue"].as_str()))
    });
    let mut refreshed = Vec::new();
    let mut refreshed_old = HashSet::new();
    if refresh {
        for issue in &issues {
            if issue["issue"] != "source_changed" {
                continue;
            }
            let Some(id) = issue["id"].as_str() else {
                continue;
            };
            let Some(memory) = memories.iter().find(|memory| memory.id == id) else {
                continue;
            };
            let Some((relative, symbol)) = source_parts(&memory.source) else {
                continue;
            };
            let full = root.join(relative);
            let Ok(text) = fs::read_to_string(&full) else {
                continue;
            };
            let Some((line, hash)) = anchor_text(&text, symbol.as_deref()) else {
                continue;
            };
            let mut updated = memory.clone();
            updated.id = crate::model::new_id();
            updated.source_hash = Some(hash);
            updated.source_line = (line > 0).then_some(line);
            updated.supersedes = vec![memory.id.clone()];
            append_record(
                path,
                crate::store::T_MEMORY,
                &memory_payload(&updated),
                true,
            )?;
            refreshed_old.insert(memory.id.clone());
            refreshed.push(updated.id);
        }
    }
    if apply {
        for issue in &issues {
            let Some(id) = issue["id"].as_str() else {
                continue;
            };
            if refreshed_old.contains(id) {
                continue;
            }
            let reason = issue["issue"].as_str().unwrap_or("source drift");
            write_feedback(path, id, "stale", Some(reason), "audit")?;
        }
    }
    let total = issues.len();
    let unresolved = total.saturating_sub(refreshed.len());
    issues.truncate(500);
    Ok(json!({
        "ok": true,
        "unresolved": unresolved,
        "root": root.display().to_string(),
        "checked": checked,
        "issues": issues,
        "issue_count": total,
        "apply": apply,
        "refreshed": refreshed,
        "changed_only": changed_only,
        "scope": scope,
        "limit": limit
    }))
}

#[allow(clippy::too_many_arguments)]
fn cmd_search(
    path: &Path,
    query: &str,
    top: usize,
    scope: Option<String>,
    task: Option<String>,
    kind: Option<String>,
    min_score: f64,
    min_trust: Option<String>,
    include_expired: bool,
) -> Result<()> {
    let output = search_out(
        path,
        &SearchQuery {
            query: query.to_string(),
            top,
            scope,
            task,
            kind,
            min_score,
            min_trust,
            include_expired,
        },
    )?;
    outln!("{}", serde_json::to_string(&output)?);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_context(
    path: &Path,
    query: &str,
    budget: usize,
    top: usize,
    scope: Option<String>,
    task: Option<String>,
    kind: Option<String>,
    min_trust: Option<String>,
    format: String,
    max_items: usize,
    min_score: f64,
    compact: bool,
    snippet_lines: usize,
    root: &Path,
) -> Result<()> {
    let mut store = Store::open(path)?;
    let q = RecallQuery {
        query: query.to_string(),
        budget_tokens: budget,
        top,
        max_items,
        scope,
        task,
        kind,
        min_trust,
        min_score,
        compact,
        snippet_lines,
    };
    let (selected, estimated) = select_context(&mut store, &q)?;

    match format.as_str() {
        "cmx" => {
            outln!("CMX/1");
            for memory in &selected {
                outln!("{}", cmx_line(memory));
            }
        }
        "json" => {
            let mut memories: Vec<MemoryOut> = selected.iter().map(MemoryOut::from).collect();
            if snippet_lines > 0 {
                for (memory, out) in selected.iter().zip(memories.iter_mut()) {
                    out.snippet = source_snippet(memory, root, snippet_lines);
                }
            }
            outln!(
                "{}",
                serde_json::to_string(&ContextOut {
                    query: query.to_string(),
                    budget,
                    estimated_tokens: estimated,
                    compact,
                    memories,
                })?
            );
        }
        other => bail!("invalid context format {other:?}; expected cmx or json"),
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_brief(path: &Path, args: &[String]) -> Result<()> {
    let budget = arg_usize(args, "--budget", 1200);
    let format = arg_value(args, "--format").unwrap_or_else(|| "json".to_string());
    let snippet_lines = arg_usize(args, "--snippets", 0);
    let root = PathBuf::from(arg_value(args, "--root").unwrap_or_else(|| ".".to_string()));
    let task_arg = arg_value(args, "--task").unwrap_or_else(|| "current".to_string());
    let explicit_query = arg_value(args, "--query");
    let mut store = Store::open(path)?;
    let (frame, query, frame_tokens, task) = match latest_frame(&mut store, Some(&task_arg))? {
        Some((memory, frame)) => {
            let query = explicit_query
                .or_else(|| frame_query(&frame))
                .ok_or_else(|| {
                    anyhow!(
                        "frame for task {:?} has no goal or next steps; pass --query",
                        frame.task
                    )
                })?;
            let value = frame_json(&memory, &frame);
            let tokens = estimate_tokens(&serde_json::to_string(&value)?);
            (Some(value), query, tokens, Some(frame.task))
        }
        None => {
            let query = explicit_query.ok_or_else(|| {
                anyhow!("no frame for this workspace; run cm frame set or pass --query")
            })?;
            (None, query, 0, (task_arg != "current").then_some(task_arg))
        }
    };
    let memory_budget = budget.saturating_sub(frame_tokens);
    let q = RecallQuery {
        query: query.clone(),
        budget_tokens: memory_budget,
        top: arg_usize(args, "--top", 16),
        max_items: arg_usize(args, "--max-items", 5),
        scope: arg_value(args, "--scope"),
        task,
        kind: None,
        min_trust: Some(arg_value(args, "--min-trust").unwrap_or_else(|| "imported".to_string())),
        min_score: 0.0,
        compact: true,
        snippet_lines,
    };
    let (mut selected, _) = select_context(&mut store, &q)?;
    let feedback = collect_feedback_stats(&mut store, 1024)?;
    selected.retain(|memory| {
        feedback
            .get(&memory.id)
            .is_none_or(|stats| !(stats.wrong >= 2 || (stats.stale >= 2 && stats.useful == 0)))
    });
    let mut memories: Vec<BriefMemory> = selected.iter().map(BriefMemory::from).collect();
    if snippet_lines > 0 {
        let mut seen = std::collections::HashSet::new();
        for (memory, brief) in selected.iter().zip(memories.iter_mut()) {
            let key = format!("{}\0{}", memory.source, memory.source_line.unwrap_or(0));
            if seen.insert(key) {
                brief.snippet = source_snippet(memory, &root, snippet_lines);
            }
        }
    }
    let mut memory_tokens = json_tokens(&memories)?;
    while !memories.is_empty() && frame_tokens + memory_tokens > budget {
        memories.pop();
        memory_tokens = json_tokens(&memories)?;
    }
    match format.as_str() {
        "json" => outln!(
            "{}",
            serde_json::to_string(&BriefOut {
                frame,
                query,
                budget,
                estimated_tokens: frame_tokens + memory_tokens,
                memories,
            })?
        ),
        "prompt" | "text" => outln!("{}", brief_prompt(&frame, &query, &memories)),
        other => bail!("invalid brief format {other:?}; expected json or prompt"),
    }
    Ok(())
}

fn cmd_wiki(args: &[String]) -> Result<()> {
    let sub = args.get(1).map(String::as_str).ok_or_else(|| {
        anyhow!(
            "wiki requires plan, prompt, apply, review, status, check, find, route, excerpt, remember, next, or generate"
        )
    })?;
    let root = PathBuf::from(arg_value(args, "--root").unwrap_or_else(|| ".".to_string()));
    if !root.is_dir() {
        bail!("wiki root is not a directory: {}", root.display());
    }
    let root = root.canonicalize().unwrap_or(root);
    let out = match arg_value(args, "--out") {
        Some(value) => {
            let path = PathBuf::from(value);
            if path.is_absolute() {
                path
            } else {
                root.join(path)
            }
        }
        None => root.join(crate::wiki::WIKI_DIR),
    };
    match sub {
        "plan" => {
            let max_modules = arg_usize(args, "--max-modules", crate::wiki::DEFAULT_MAX_MODULES);
            let plan = crate::wiki::plan_with(&root, &out, max_modules)?;
            outln!("{}", serde_json::to_string(&plan)?);
        }
        "prompt" => {
            let page = arg_value(args, "--page")
                .ok_or_else(|| anyhow!("wiki prompt requires --page <id>"))?;
            outln!("{}", crate::wiki::prompt(&out, &page)?);
        }
        "apply" => {
            let page = arg_value(args, "--page")
                .ok_or_else(|| anyhow!("wiki apply requires --page <id>"))?;
            let input = arg_value(args, "--input").unwrap_or_else(|| "-".to_string());
            let content = if input == "-" {
                io::read_to_string(io::stdin())?
            } else {
                fs::read_to_string(&input)?
            };
            let state = crate::wiki::apply(&out, &page, &content)?;
            outln!(
                "{}",
                json!({"ok": true, "page": state.page.id, "file": out.join(state.file).display().to_string(), "status": state.status})
            );
        }
        "review" => {
            let page = arg_value(args, "--page")
                .ok_or_else(|| anyhow!("wiki review requires --page <id>"))?;
            let command = arg_value(args, "--review-cmd")
                .or_else(|| env::var("CM_WIKI_REVIEW_AGENT").ok())
                .ok_or_else(|| {
                    anyhow!(
                        "wiki review requires --review-cmd \"<command>\" or CM_WIKI_REVIEW_AGENT"
                    )
                })?;
            let review = crate::wiki::review(&out, &page, &command)?;
            outln!("{}", serde_json::to_string(&review)?);
            if review.verdict != "pass" || !review.issues.is_empty() {
                process::exit(1);
            }
        }
        "status" | "list" => {
            let status = crate::wiki::status(&out)?;
            outln!("{}", serde_json::to_string(&status)?);
        }
        "check" => {
            let report = crate::wiki::check_with(&out, has_flag(args, "--strict"))?;
            outln!("{}", serde_json::to_string(&report)?);
            if has_flag(args, "--strict") && !report.issues.is_empty() {
                process::exit(1);
            }
        }
        "find" => {
            let query = arg_value(args, "--query")
                .ok_or_else(|| anyhow!("wiki find requires --query <text>"))?;
            let matches = crate::wiki::find(&out, &query, arg_usize(args, "--limit", 5))?;
            match arg_value(args, "--format").as_deref() {
                Some("json") => outln!("{}", serde_json::to_string(&matches)?),
                Some("text") | None => {
                    for item in matches {
                        outln!(
                            "{:.2}\t{}\t{}\t{}",
                            item.score,
                            item.page,
                            item.title,
                            item.headings.join(" | ")
                        );
                    }
                }
                Some(other) => bail!("invalid wiki find format {other:?}; expected text or json"),
            }
        }
        "route" => {
            let query = arg_value(args, "--query")
                .ok_or_else(|| anyhow!("wiki route requires --query <text>"))?;
            let task_size = arg_value(args, "--task-size").unwrap_or_else(|| "auto".to_string());
            let plan = crate::wiki::route(
                &out,
                &query,
                arg_usize(args, "--budget", 800),
                arg_usize(args, "--max-sections", 2),
                &task_size,
            )?;
            match arg_value(args, "--format").as_deref() {
                Some("json") => outln!("{}", serde_json::to_string(&plan)?),
                Some("text") | None => {
                    if plan.text.is_empty() {
                        outln!("# no wiki context: {}", plan.reasons.join("; "));
                    } else {
                        outln!("{}", plan.text);
                    }
                }
                Some(other) => bail!("invalid wiki route format {other:?}; expected text or json"),
            }
        }
        "excerpt" => {
            let page = arg_value(args, "--page")
                .ok_or_else(|| anyhow!("wiki excerpt requires --page <id>"))?;
            let query = arg_value(args, "--query")
                .ok_or_else(|| anyhow!("wiki excerpt requires --query <text>"))?;
            let excerpt = crate::wiki::excerpt(
                &out,
                &page,
                &query,
                arg_usize(args, "--budget", 600),
                arg_usize(args, "--max-sections", 2),
            )?;
            match arg_value(args, "--format").as_deref() {
                Some("json") => outln!("{}", serde_json::to_string(&excerpt)?),
                Some("text") | None => outln!("{}", excerpt.text),
                Some(other) => {
                    bail!("invalid wiki excerpt format {other:?}; expected text or json")
                }
            }
        }
        "remember" => {
            let file = PathBuf::from(arg_value(args, "--file").unwrap_or_else(|| "cm".to_string()));
            let scope = arg_value(args, "--scope").unwrap_or_else(|| "repo".to_string());
            let report = crate::wiki::remember_pages(
                &file,
                &out,
                arg_value(args, "--page").as_deref(),
                &scope,
                !has_flag(args, "--no-replace"),
                !has_flag(args, "--include-unreviewed"),
            )?;
            outln!("{}", serde_json::to_string(&report)?);
        }
        "generate" => {
            let command = arg_value(args, "--agent-cmd")
                .or_else(|| env::var("CM_WIKI_AGENT").ok())
                .ok_or_else(|| {
                    anyhow!("wiki generate requires --agent-cmd \"<command>\" or CM_WIKI_AGENT")
                })?;
            let review_cmd =
                arg_value(args, "--review-cmd").or_else(|| env::var("CM_WIKI_REVIEW_AGENT").ok());
            let rounds = arg_usize(args, "--review-rounds", 2);
            let report = if let Some(page) = arg_value(args, "--page") {
                crate::wiki::generate_reviewed_page(
                    &out,
                    &command,
                    review_cmd.as_deref(),
                    rounds,
                    &page,
                )?
            } else {
                crate::wiki::generate_reviewed(
                    &out,
                    &command,
                    review_cmd.as_deref(),
                    rounds,
                    arg_usize(args, "--max-pages", 0),
                )?
            };
            outln!("{}", serde_json::to_string(&report)?);
        }
        "next" => {
            let next = crate::wiki::next(&out)?;
            match next {
                Some((state, prompt)) => outln!(
                    "{}",
                    serde_json::to_string(&json!({
                        "done": false,
                        "page": state.page.id,
                        "status": state.status,
                        "prompt": prompt
                    }))?
                ),
                None => outln!("{}", json!({"done": true})),
            }
        }
        other => {
            bail!("invalid wiki command {other:?}; expected plan, prompt, apply, review, status, check, find, route, excerpt, remember, next, or generate")
        }
    }
    Ok(())
}

fn cmd_explore_mechanical(path: &Path, root: &Path, args: &[String]) -> Result<()> {
    let options = crate::explore_api::ExploreOptions {
        root: root.to_path_buf(),
        mode: crate::explore_api::ExploreMode::Auto,
        task: arg_value(args, "--task"),
        query: None,
        changed: None,
        max_targets: 28,
    };
    let apply = has_flag(args, "--apply");
    let plan = mechanical_explore(path, &options, apply)?;
    let format = arg_value(args, "--format").unwrap_or_else(|| "json".to_string());
    match format.as_str() {
        "json" => outln!(
            "{}",
            serde_json::to_string(&json!({
                "ok": true,
                "mode": plan.mode,
                "frame_id": plan.frame_id,
                "records": plan.records,
                "dry_run": !apply,
                "report": plan.report,
                "ops": (!apply).then_some(plan.ops)
            }))?
        ),
        "jsonl" => outln!("{}", plan.ops),
        other => bail!("invalid mechanical format {other:?}; expected json or jsonl"),
    }
    Ok(())
}

fn cmd_explore(path: &Path, args: &[String]) -> Result<()> {
    let root = PathBuf::from(arg_value(args, "--root").unwrap_or_else(|| ".".to_string()));
    if !root.is_dir() {
        bail!("explore root is not a directory: {}", root.display());
    }
    let root = root.canonicalize().unwrap_or(root);
    if has_flag(args, "--mechanical") {
        return cmd_explore_mechanical(path, &root, args);
    }
    if let Some(input) = arg_value(args, "--apply") {
        let mut forwarded = vec![
            "--input".to_string(),
            input,
            "--root".to_string(),
            root.to_string_lossy().to_string(),
        ];
        for flag in ["--dry-run", "--replace", "--allow-long-text"] {
            if has_flag(args, flag) {
                forwarded.push(flag.to_string());
            }
        }
        return cmd_absorb(path, &forwarded);
    }
    let requested_mode = arg_value(args, "--mode").unwrap_or_else(|| "auto".to_string());
    let requested_task = arg_value(args, "--task");
    let explicit_query = arg_value(args, "--query");
    let (evidence_id, frame) = ensure_explore_frame(path, &root, requested_task.as_deref())?;
    let query = explicit_query
        .clone()
        .or_else(|| {
            frame
                .as_ref()
                .filter(|frame| !frame.task.starts_with("explore:"))
                .and_then(frame_query)
        })
        .unwrap_or_else(|| {
            format!(
                "repository architecture, entry points, build/test commands, invariants, and non-obvious behavior in {}",
                display_explore_root(&root)
            )
        });
    let mut store = Store::open(path)?;
    let active = active_memories(&mut store)?;
    let user_memories: Vec<crate::model::Memory> = active
        .iter()
        .filter(|memory| !matches!(memory.kind.as_str(), FRAME_KIND | FEEDBACK_KIND))
        .cloned()
        .collect();
    let anchored_count = user_memories
        .iter()
        .filter(|memory| memory.source_hash.is_some())
        .count();
    let changed: Vec<String> = changed_paths(&root)
        .unwrap_or_default()
        .into_iter()
        .collect();
    let mut changed = changed;
    changed.retain(|path| crate::explore::is_explore_input(path));
    changed.sort();
    let mode = explore_mode(
        &requested_mode,
        user_memories.len(),
        &changed,
        frame.as_ref(),
        explicit_query.is_some(),
    )?;
    let task = frame.as_ref().map(|frame| frame.task.as_str());
    let known = collect_explore_known(&mut store, &query, task, &active)?;
    let scan = scan_repo(&root);
    let default_max_items = if mode == "macro" {
        (8 + scan.source_files / 250).clamp(8, 24)
    } else {
        (4 + known.len() / 4).clamp(6, 16)
    };
    let max_items = arg_value(args, "--max-items")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(default_max_items)
        .clamp(1, 32);
    let default_budget = if mode == "macro" {
        2000 + max_items * 40
    } else {
        1200 + max_items * 40
    };
    let budget = arg_usize(args, "--budget", default_budget).max(300);
    let targets = collect_explore_targets(
        mode,
        &scan,
        &changed,
        frame.as_ref(),
        &known,
        &user_memories,
    );
    let mut prompt = build_explore_prompt(ExplorePromptInput {
        mode,
        root: &root,
        task,
        query: &query,
        evidence_id: &evidence_id,
        scan: &scan,
        changed: &changed,
        known: &known,
        active_count: user_memories.len(),
        anchored_count,
        targets: &targets,
        max_items,
    });
    let mut prompt_known = known;
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
    let known = prompt_known;
    let repo_json = json!({
        "files": scan.files,
        "source_files": scan.source_files,
        "top_dirs": scan.top_dirs.iter().map(|dir| json!({
            "path": dir.path,
            "files": dir.files,
            "sources": dir.sources
        })).collect::<Vec<_>>(),
        "areas": scan.areas.iter().map(|area| json!({
            "path": area.path,
            "sources": area.sources
        })).collect::<Vec<_>>(),
        "manifests": scan.manifests,
        "entrypoints": scan.entrypoints,
        "changed": changed
    });
    let memories_json = json!({
        "active": user_memories.len(),
        "source_anchored": user_memories.iter().filter(|memory| memory.source_hash.is_some()).count(),
        "known": known.iter().map(|known| json!({
            "id": known.memory.id,
            "kind": known.memory.kind,
            "subject": known.memory.subject,
            "source": known.memory.source,
            "preview": known.preview
        })).collect::<Vec<_>>()
    });
    let targets_json = targets
        .iter()
        .map(|target| json!({"path": target.path, "reason": target.reason}))
        .collect::<Vec<_>>();
    let format = arg_value(args, "--format").unwrap_or_else(|| "prompt".to_string());
    match format.as_str() {
        "json" => outln!(
            "{}",
            serde_json::to_string(&json!({
                "ok": true,
                "mode": mode,
                "root": display_explore_root(&root),
                "task": task,
                "query": query,
                "evidence_id": evidence_id,
                "budget": budget,
                "max_items": max_items,
                "repo": repo_json,
                "memories": memories_json,
                "targets": targets_json,
                "prompt": prompt
            }))?
        ),
        "prompt" | "text" => outln!("{}", prompt),
        other => bail!("invalid explore format {other:?}; expected prompt or json"),
    }
    Ok(())
}

fn cmd_pack(path: &Path, args: &[String]) -> Result<()> {
    let budget = arg_usize(args, "--budget", 1200);
    let snippet_lines = arg_usize(args, "--snippets", 0);
    let root = PathBuf::from(arg_value(args, "--root").unwrap_or_else(|| ".".to_string()));
    let task_arg = arg_value(args, "--task").unwrap_or_else(|| "current".to_string());
    let explicit_query = arg_value(args, "--query");
    let mut store = Store::open(path)?;
    let (frame, query, frame_tokens, task) = match latest_frame(&mut store, Some(&task_arg))? {
        Some((memory, frame)) => {
            let query = explicit_query
                .or_else(|| frame_query(&frame))
                .ok_or_else(|| {
                    anyhow!(
                        "frame for task {:?} has no goal or next steps; pass --query",
                        frame.task
                    )
                })?;
            let value = frame_json(&memory, &frame);
            let tokens = estimate_tokens(&serde_json::to_string(&value)?);
            (Some(value), query, tokens, Some(frame.task))
        }
        None => {
            let query = explicit_query.ok_or_else(|| {
                anyhow!("no frame for this workspace; run cm frame set or pass --query")
            })?;
            (None, query, 0, (task_arg != "current").then_some(task_arg))
        }
    };
    let memory_budget = budget.saturating_sub(frame_tokens);
    let q = RecallQuery {
        query: query.clone(),
        budget_tokens: memory_budget,
        top: arg_usize(args, "--top", 16),
        max_items: arg_usize(args, "--max-items", 8),
        scope: arg_value(args, "--scope"),
        task,
        kind: None,
        min_trust: Some(arg_value(args, "--min-trust").unwrap_or_else(|| "imported".to_string())),
        min_score: 0.0,
        compact: true,
        snippet_lines,
    };
    let (mut selected, _) = select_context(&mut store, &q)?;
    let feedback = collect_feedback_stats(&mut store, 1024)?;
    selected.retain(|memory| {
        feedback
            .get(&memory.id)
            .is_none_or(|stats| !(stats.wrong >= 2 || (stats.stale >= 2 && stats.useful == 0)))
    });
    let mut seen_sources = std::collections::HashSet::new();
    let memories: Vec<PackMemory> = selected
        .iter()
        .map(|memory| {
            let mut pack = PackMemory::from(memory);
            if snippet_lines > 0 {
                let key = format!("{}\0{}", memory.source, memory.source_line.unwrap_or(0));
                if seen_sources.insert(key) {
                    pack.snippet = source_snippet(memory, &root, snippet_lines);
                }
            }
            pack
        })
        .collect();
    outln!("You are compressing cm memory context for a coding agent.");
    outln!("Return only JSONL, one object per line. No markdown, no prose.");
    outln!("Use the supplied memories as evidence. Do not invent sources.");
    outln!("If nothing is worth storing, return an empty response.");
    outln!("");
    outln!("CONTEXT");
    outln!("QUERY: {query}", query = query);
    if let Some(frame) = &frame {
        outln!("TASK");
        outln!("{}", serde_json::to_string(frame)?);
    }
    outln!("MEMORIES");
    for memory in &memories {
        outln!("{}", serde_json::to_string(memory)?);
    }
    outln!("");
    outln!("OUTPUT");
    outln!("One JSONL object per line, exactly one of:");
    outln!("{{\"op\":\"remember\",\"kind\":\"fact|decision|preference|constraint|episode|procedure\",\"scope\":\"...\",\"subject\":\"...\",\"text\":\"<=800 chars\",\"source\":\"path:symbol|agent:<name>\",\"evidence\":[\"<memory-id>\"],\"confidence\":0.0}}");
    outln!("{{\"op\":\"supersede\",\"kind\":\"...\",\"scope\":\"...\",\"subject\":\"...\",\"text\":\"...\",\"source\":\"...\",\"supersedes\":[\"<memory-id>\"],\"evidence\":[\"<memory-id>\"]}}");
    outln!("{{\"op\":\"forget\",\"id\":\"<memory-id>\",\"reason\":\"...\"}}");
    outln!("");
    outln!("Rules: evidence is required for remember and supersede. Prefer path:symbol over a bare path; use a bare path only when the whole file is the claim. Never create kind=frame or kind=feedback. Never invent a source path or symbol.");
    Ok(())
}

fn cmd_absorb(path: &Path, args: &[String]) -> Result<()> {
    let input = arg_value(args, "--input").unwrap_or_else(|| "-".to_string());
    let dry_run = has_flag(args, "--dry-run");
    let replace = has_flag(args, "--replace");
    let allow_long_text = has_flag(args, "--allow-long-text");
    let root = arg_value(args, "--root").map(PathBuf::from);
    let content = if input == "-" {
        io::read_to_string(io::stdin())?
    } else {
        fs::read_to_string(&input)?
    };
    let options = crate::mutate::AbsorbOptions {
        replace,
        dry_run,
        allow_long_text,
    };
    let report = crate::mutate::absorb(path, &content, &options, None, root.as_deref())?;
    let accepted = report.written + report.duplicated + report.forgotten;
    outln!(
        "{}",
        serde_json::to_string(&AbsorbOut {
            ok: true,
            accepted,
            written: report.written,
            duplicates: report.duplicated,
            forgotten: report.forgotten,
            dry_run,
        })?
    );
    Ok(())
}

fn cmd_similar(path: &Path, id: &str, top: usize, scope: Option<String>) -> Result<()> {
    let mut store = Store::open(path)?;
    let tail = store.tail_view()?;
    if tail.hidden.contains(id) {
        bail!("memory not found: {id}");
    }
    let (target, target_idx) = if let Some((idx, memory)) = store.find_doc(id)? {
        (memory, Some(idx))
    } else {
        (
            tail.get(id)
                .cloned()
                .ok_or_else(|| anyhow!("memory not found: {id}"))?,
            None,
        )
    };
    let scope = scope.as_deref().unwrap_or(&target.scope);
    let scope_id = store.scope_id(scope)?;
    let mut hits: HashMap<String, (f64, crate::model::Memory)> = HashMap::new();
    if let Some(scope_id) = scope_id {
        for (doc_idx, score) in store.bm25(
            &format!("{} {}", target.subject, target.text),
            Some(scope_id),
            None,
            target_idx,
            None,
            top.min(100),
        )? {
            let meta = store.doc_meta(doc_idx)?;
            let memory = store.doc_memory(&meta)?;
            if !tail.hidden.contains(&memory.id) {
                hits.insert(memory.id.clone(), (score, memory));
            }
        }
    }
    for (score, memory) in tail.bm25(
        &format!("{} {}", target.subject, target.text),
        Some(scope),
        None,
        Some(&target.id),
        top.min(100),
    ) {
        hits.insert(memory.id.clone(), (score, memory));
    }
    let signature = minhash_signature(&target);
    let mut candidates: Vec<CandidateOut> = hits
        .into_values()
        .map(|(bm25, memory)| CandidateOut {
            id: memory.id.clone(),
            bm25,
            minhash: minhash_similarity(&signature, &minhash_signature(&memory)),
            subject: memory.subject.clone(),
            preview: preview(&memory.text, 120),
        })
        .collect();
    candidates.sort_by(|a, b| {
        b.bm25
            .partial_cmp(&a.bm25)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
    candidates.truncate(top);
    outln!(
        "{}",
        serde_json::to_string(&SimilarOut {
            id: target.id,
            candidates
        })?
    );
    Ok(())
}

fn cmd_merge(path: &Path, sources: &str, text: Option<String>) -> Result<()> {
    let source_ids = parse_ids(sources);
    let merged = crate::mutate::merge(path, &source_ids, text.as_deref(), None)?;
    outln!(
        "{}",
        json!({"ok": true, "id": merged.id, "sources": source_ids})
    );
    Ok(())
}

pub(crate) fn cmd_forget(path: &Path, id: &str) -> Result<()> {
    crate::mutate::forget(path, id, "user_request", None)?;
    outln!("{}", json!({"ok": true, "target": id}));
    Ok(())
}

fn cmd_purge(path: &Path) -> Result<()> {
    let header = reindex_file(path)?;
    outln!(
        "{}",
        json!({"ok": true, "doc_count": header.doc_count, "blocks": header.block_count})
    );
    Ok(())
}

fn cmd_conflicts(path: &Path) -> Result<()> {
    let mut store = Store::open(path)?;
    let memories = active_memories(&mut store)?;
    let conflicts = conflict_groups(&memories);
    outln!(
        "{}",
        serde_json::to_string(&json!({"ok": true, "conflicts": conflicts}))?
    );
    Ok(())
}

/// Active memories in the canonical verify accounting: index docs flagged
/// active in the bitset minus tail-hidden ids, plus active tail memories.
/// Decodes each active doc once; never scans inactive docs.
fn conflict_groups(memories: &[crate::model::Memory]) -> Vec<ConflictGroupOut> {
    let mut groups: HashMap<(String, String, String), Vec<&crate::model::Memory>> = HashMap::new();
    for memory in memories {
        if memory.kind == FRAME_KIND || memory.kind == FEEDBACK_KIND {
            continue;
        }
        groups
            .entry((
                memory.scope.clone(),
                memory.kind.clone(),
                memory.subject.clone(),
            ))
            .or_default()
            .push(memory);
    }
    let mut conflicts = Vec::new();
    for ((scope, kind, subject), records) in groups {
        let mut hashes = HashSet::new();
        for record in &records {
            hashes.insert(crate::model::content_hash(record));
        }
        if hashes.len() > 1 {
            conflicts.push(ConflictGroupOut {
                scope,
                kind,
                subject,
                records: records
                    .iter()
                    .map(|record| MemoryOut::from(*record))
                    .collect(),
            });
        }
    }
    conflicts.sort_by(|a, b| {
        a.scope
            .cmp(&b.scope)
            .then_with(|| a.kind.cmp(&b.kind))
            .then_with(|| a.subject.cmp(&b.subject))
    });
    conflicts
}

pub(crate) const DOCTOR_WARNING_CAP: usize = 200;

pub(crate) fn append_conflict_warnings(warnings: &mut Vec<String>, groups: &[ConflictGroupOut]) {
    for group in groups.iter().take(DOCTOR_WARNING_CAP) {
        warnings.push(format!(
            "conflict: scope={} kind={} subject={} records={}",
            group.scope,
            group.kind,
            group.subject,
            group.records.len()
        ));
    }
    if groups.len() > DOCTOR_WARNING_CAP {
        warnings.push(format!(
            "conflict: {} more not shown",
            groups.len() - DOCTOR_WARNING_CAP
        ));
    }
}

pub(crate) fn cmd_doctor(path: &Path) -> Result<bool> {
    let full = crate::store::verify_full(path);
    let (full_checksum, full_records, checksum_error) = match full {
        Ok(records) => (true, Some(records), None),
        Err(error) => (
            false,
            None,
            Some(format!("full checksum failed: {error:#}")),
        ),
    };

    let mut warnings = Vec::new();
    let mut store_ok = true;
    let mut active_total = 0u64;
    let mut expired = 0u64;
    let mut conflicts = 0usize;

    match Store::open(path) {
        Ok(mut store) => match active_memories(&mut store) {
            Ok(memories) => {
                active_total = memories.len() as u64;
                let now = crate::model::now_ms();

                let mut expired_warnings = 0usize;
                for memory in &memories {
                    if let Some(end) = memory.valid_to {
                        if end <= now {
                            expired += 1;
                            if expired_warnings < DOCTOR_WARNING_CAP {
                                warnings.push(format!(
                                    "expired: {} scope={} kind={} subject={} valid_to={end}",
                                    memory.id, memory.scope, memory.kind, memory.subject
                                ));
                                expired_warnings += 1;
                            }
                        }
                    }
                }
                if expired > expired_warnings as u64 {
                    warnings.push(format!(
                        "expired: {} more not shown",
                        expired - expired_warnings as u64
                    ));
                }

                let groups = conflict_groups(&memories);
                conflicts = groups.len();
                append_conflict_warnings(&mut warnings, &groups);
            }
            Err(error) => {
                store_ok = false;
                active_total = store.active_count().unwrap_or(0);
                warnings.push(format!("active scan failed: {error:#}"));
            }
        },
        Err(error) => {
            store_ok = false;
            warnings.push(format!("store open failed: {error:#}"));
        }
    }
    if let Some(error) = checksum_error {
        warnings.push(error);
    }

    let cache_bytes = fs::metadata(crate::cache::cache_path(path))
        .map(|meta| meta.len())
        .unwrap_or(0);
    let ok = store_ok && conflicts == 0 && expired == 0 && full_checksum;
    outln!(
        "{}",
        serde_json::to_string(&json!({
            "ok": ok,
            "active_total": active_total,
            "conflicts": conflicts,
            "expired": expired,
            "full_checksum": full_checksum,
            "full_records": full_records,
            "cache_bytes": cache_bytes,
            "warnings": warnings
        }))?
    );
    Ok(ok)
}

fn active_metrics(store: &mut Store) -> Result<(u64, usize)> {
    let tail = store.tail_view()?;
    let mut active_index = store.active_count()?;
    for id in &tail.hidden {
        if let Some((idx, _)) = store.find_doc(id)? {
            if store.is_active(idx)? {
                active_index = active_index.saturating_sub(1);
            }
        }
    }
    Ok((active_index, tail.memories.len()))
}

pub(crate) fn cmd_verify(path: &Path, full: bool) -> Result<()> {
    let full_records = if full {
        Some(crate::store::verify_full(path)?)
    } else {
        None
    };
    let mut store = Store::open(path)?;
    let (active_index, tail_memories) = active_metrics(&mut store)?;
    outln!(
        "{}",
        json!({
            "ok": true,
            "bytes": fs::metadata(path)?.len(),
            "version": 1,
            "blocks": store.header.block_count,
            "index_docs": store.header.doc_count,
            "active_index_docs": active_index,
            "tail_memories": tail_memories,
            "active_total": active_index + tail_memories as u64,
            "index_bytes": store.header.index_len,
            "index_raw_bytes": store.header.index_raw_len,
            "full_checksum": full,
            "full_records": full_records
        })
    );
    Ok(())
}

fn run_consolidate(
    path: &Path,
    apply: bool,
    top: usize,
    limit: usize,
    offset: usize,
    threshold: f64,
) -> Result<ConsolidateOut> {
    let mut store = Store::open(path)?;
    let tail = store.tail_view()?;
    let doc_count = store.doc_count();
    let mut memories = Vec::new();
    let mut skipped = offset;
    for idx in 0..doc_count {
        if !store.is_active(idx)? {
            continue;
        }
        if skipped > 0 {
            skipped -= 1;
            continue;
        }
        if memories.len() >= limit {
            break;
        }
        let meta = store.doc_meta(idx)?;
        let memory = store.doc_memory(&meta)?;
        if !tail.hidden.contains(&memory.id)
            && memory.kind != FRAME_KIND
            && memory.kind != FEEDBACK_KIND
        {
            memories.push((idx, memory));
        }
    }
    let signatures: Vec<Option<Vec<u64>>> =
        memories.iter().map(|(_, m)| minhash_signature(m)).collect();
    let mut dsu = Dsu::new(memories.len());
    let mut pair_scores: HashMap<(usize, usize), (&'static str, f64)> = HashMap::new();
    for (i, (doc_idx, memory)) in memories.iter().enumerate() {
        let scope_id = store.scope_id(&memory.scope)?;
        let kind = kind_id(&memory.kind);
        for (j, _) in store.bm25(
            &format!("{} {}", memory.subject, memory.text),
            scope_id,
            kind,
            Some(*doc_idx),
            None,
            top.min(100),
        )? {
            let Some(pos) = memories.iter().position(|(idx, _)| *idx == j) else {
                continue;
            };
            if i == pos {
                continue;
            }
            if let Some((strategy, score)) = pair_similarity(
                &memories[i].1,
                &memories[pos].1,
                &signatures[i],
                &signatures[pos],
                threshold,
            ) {
                dsu.union(i, pos);
                let key = if i < pos { (i, pos) } else { (pos, i) };
                pair_scores
                    .entry(key)
                    .and_modify(|entry| {
                        if score > entry.1 {
                            *entry = (strategy, score);
                        }
                    })
                    .or_insert((strategy, score));
            }
        }
    }
    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..memories.len() {
        groups.entry(dsu.find(i)).or_default().push(i);
    }
    let mut output = Vec::new();
    let mut pending = Vec::new();
    for mut members in groups.into_values() {
        if members.len() < 2 {
            continue;
        }
        members.sort_by(|a, b| {
            memories[*b]
                .1
                .confidence
                .partial_cmp(&memories[*a].1.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| memories[*b].1.text.len().cmp(&memories[*a].1.text.len()))
                .then_with(|| memories[*a].1.id.cmp(&memories[*b].1.id))
        });
        let canonical = &memories[members[0]].1;
        let sources: Vec<String> = members.iter().map(|i| memories[*i].1.id.clone()).collect();
        let (strategy, score) = members
            .iter()
            .flat_map(|a| members.iter().map(move |b| (*a, *b)))
            .filter(|(a, b)| a < b)
            .filter_map(|(a, b)| pair_scores.get(&(a, b)).copied())
            .max_by(|left, right| {
                left.1
                    .partial_cmp(&right.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap_or(("minhash", 0.0));
        output.push(MergeGroupOut {
            sources: sources.clone(),
            strategy: strategy.to_string(),
            score,
            text: preview(&canonical.text, 200),
        });
        if apply {
            pending.push(build_merged(
                canonical,
                &sources,
                &canonical.text,
                strategy,
                score,
            ));
        }
    }
    if apply {
        for merged in &pending {
            append_record(path, 3, &memory_payload(merged), false)?;
        }
    }
    Ok(ConsolidateOut {
        groups: output,
        applied: apply,
    })
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|arg| arg == flag)
        .and_then(|index| args.get(index + 1))
        .cloned()
}

fn arg_values(args: &[String], flag: &str) -> Vec<String> {
    args.iter()
        .enumerate()
        .filter(|(_, arg)| *arg == flag)
        .filter_map(|(index, _)| args.get(index + 1))
        .cloned()
        .collect()
}
fn arg_path(args: &[String]) -> PathBuf {
    if let Some(file) = arg_value(args, "--file") {
        return PathBuf::from(file);
    }
    find_cm_file().unwrap_or_else(|| PathBuf::from("cm"))
}

/// Commands that create a new store use the local default; existing-store
/// commands may discover a `cm` file in an enclosing workspace directory.
fn arg_new_path(args: &[String]) -> PathBuf {
    PathBuf::from(arg_value(args, "--file").unwrap_or_else(|| "cm".to_string()))
}

fn find_cm_file() -> Option<PathBuf> {
    let cwd = env::current_dir().ok()?;
    cwd.ancestors()
        .map(|dir| dir.join("cm"))
        .find(|candidate| candidate.is_file())
}
fn arg_usize(args: &[String], flag: &str, default: usize) -> usize {
    arg_value(args, flag)
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}
fn arg_f64(args: &[String], flag: &str, default: f64) -> f64 {
    arg_value(args, flag)
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}
fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|arg| arg == flag)
}
