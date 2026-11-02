use std::collections::{HashMap, HashSet};

use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::model::{
    build_merged, cmx_line, estimate_tokens, kind_id, memory_payload, memory_snippet,
    minhash_signature, minhash_similarity, new_memory, pair_similarity, parse_ids, preview, Dsu,
    KINDS,
};
use crate::store::{
    append_record, encode_header, find_tail_duplicate, read_header_file, reindex_file, Header,
    Store, HEADER_LEN,
};

const SKILL_TEXT: &str = include_str!("../assets/SKILL.md");

macro_rules! outln {
    ($($arg:tt)*) => {{
        let _ = writeln!(io::stdout(), $($arg)*);
    }};
}

#[derive(Deserialize)]
pub(crate) struct AppendInput {
    pub(crate) kind: String,
    pub(crate) scope: String,
    pub(crate) subject: String,
    pub(crate) text: String,
    #[serde(default)]
    pub(crate) source: String,
    #[serde(default = "default_confidence")]
    pub(crate) confidence: f64,
    #[serde(default)]
    pub(crate) valid_from: u64,
    #[serde(default)]
    pub(crate) valid_to: Option<u64>,
    #[serde(default)]
    pub(crate) supersedes: Vec<String>,
    #[serde(default)]
    pub(crate) conflict_of: Option<String>,
    #[serde(default)]
    pub(crate) trust: Option<String>,
    #[serde(default)]
    pub(crate) source_commit: Option<String>,
}

fn default_confidence() -> f64 {
    0.8
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ImportLine {
    Full(MemoryOut),
    Append(AppendInput),
}

#[derive(Serialize, Deserialize)]
pub(crate) struct MemoryOut {
    id: String,
    kind: String,
    scope: String,
    subject: String,
    text: String,
    source: String,
    confidence: f64,
    #[serde(default)]
    sources: Vec<String>,
    #[serde(default)]
    merged: bool,
    #[serde(default)]
    created_at: u64,
    #[serde(default)]
    strategy: Option<String>,
    #[serde(default)]
    score: Option<f64>,
    #[serde(default)]
    valid_from: u64,
    #[serde(default)]
    valid_to: Option<u64>,
    #[serde(default)]
    supersedes: Vec<String>,
    #[serde(default)]
    conflict_of: Option<String>,
    #[serde(default)]
    trust: Option<String>,
    #[serde(default)]
    source_commit: Option<String>,
}

impl MemoryOut {
    fn into_memory(self) -> crate::model::Memory {
        crate::model::Memory {
            id: self.id,
            kind: self.kind,
            scope: self.scope,
            subject: self.subject,
            text: self.text,
            source: self.source,
            confidence: self.confidence.clamp(0.0, 1.0),
            sources: self.sources,
            created_at: self.created_at,
            strategy: self.strategy,
            score: self.score,
            merged: self.merged,
            valid_from: if self.valid_from == 0 {
                self.created_at
            } else {
                self.valid_from
            },
            valid_to: self.valid_to,
            supersedes: self.supersedes,
            conflict_of: self.conflict_of,
            trust: self.trust.unwrap_or_else(|| "authored".to_string()),
            source_commit: self.source_commit,
        }
    }
}

impl From<&crate::model::Memory> for MemoryOut {
    fn from(memory: &crate::model::Memory) -> Self {
        Self {
            id: memory.id.clone(),
            kind: memory.kind.clone(),
            scope: memory.scope.clone(),
            subject: memory.subject.clone(),
            text: memory.text.clone(),
            source: memory.source.clone(),
            confidence: memory.confidence,
            sources: memory.sources.clone(),
            merged: memory.merged,
            created_at: memory.created_at,
            strategy: memory.strategy.clone(),
            score: memory.score,
            valid_from: memory.valid_from,
            valid_to: memory.valid_to,
            supersedes: memory.supersedes.clone(),
            conflict_of: memory.conflict_of.clone(),
            trust: Some(memory.trust.clone()),
            source_commit: memory.source_commit.clone(),
        }
    }
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
pub(crate) struct SearchHit {
    pub(crate) id: String,
    bm25: f64,
    kind: String,
    scope: String,
    subject: String,
    preview: String,
}

#[derive(Serialize)]
pub(crate) struct SearchOut {
    query: String,
    pub(crate) candidates: Vec<SearchHit>,
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
        "init" => cmd_init(&arg_path(&args), has_flag(&args, "--empty")),
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
            cmd_remember_input(
                &arg_path(&args),
                &input,
                has_flag(&args, "--durable"),
                has_flag(&args, "--force"),
            )
        }
        "recall" => {
            let query =
                arg_value(&args, "--query").ok_or_else(|| anyhow!("recall requires --query"))?;
            cmd_context(
                &arg_path(&args),
                &query,
                arg_usize(&args, "--budget", 800),
                arg_usize(&args, "--top", 16),
                arg_value(&args, "--scope"),
                arg_value(&args, "--task"),
                arg_value(&args, "--kind"),
                Some(arg_value(&args, "--min-trust").unwrap_or_else(|| "authored".to_string())),
                arg_value(&args, "--format").unwrap_or_else(|| "cmx".to_string()),
                arg_usize(&args, "--max-items", 3),
                arg_f64(&args, "--min-score", 0.0),
                !has_flag(&args, "--full"),
            )
        }
        "status" => cmd_verify(&arg_path(&args), false),
        "cache-stats" => cmd_cache_stats(&arg_path(&args)),
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
            cmd_context(
                &arg_path(&args),
                &query,
                arg_usize(&args, "--budget", 2000),
                arg_usize(&args, "--top", 32),
                arg_value(&args, "--scope"),
                arg_value(&args, "--task"),
                arg_value(&args, "--kind"),
                arg_value(&args, "--min-trust"),
                arg_value(&args, "--format").unwrap_or_else(|| "cmx".to_string()),
                arg_usize(&args, "--max-items", 5),
                arg_f64(&args, "--min-score", 0.0),
                has_flag(&args, "--compact"),
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
        "consolidate" => cmd_consolidate(
            &arg_path(&args),
            has_flag(&args, "--apply"),
            arg_usize(&args, "--top", 8),
            arg_usize(&args, "--limit", 50),
            arg_usize(&args, "--offset", 0),
            arg_f64(&args, "--threshold", 0.85),
        ),
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
            outln!("cm 1.0.0");
            Ok(())
        }
        other => bail!("unknown command {other:?}; try cm --help"),
    }
}

fn print_usage() {
    println!(
        "cm 1.0.0 - compact memory for AI agents
         
         cm remember --file cm --subject <subject> --text <text> [--kind fact] [--scope repo] [--source <source>] [--trust authored]
         cm recall --file cm --query <text> [--scope <scope>] [--task <task>] [--budget 800]
         cm forget --file cm --id <id>
         cm status [--file cm]
         cm cache-stats [--file cm]
         cm doctor [--strict] [--file cm]
         cm install [--all|--detected|--agent <id>] [--list] [--no-exe] [--no-path] [--dry-run]
         cm uninstall [--all|--agent <id>] [--keep-exe]
         cm skill [--json] [--install] [--check]
         
         Internal/advanced commands: see README."
    );
}

pub(crate) fn cmd_init(path: &Path, empty: bool) -> Result<()> {
    if path.exists() {
        bail!("file already exists: {}", path.display());
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let header = Header {
        created_at: crate::model::now_ms(),
        block_table_off: HEADER_LEN as u64,
        block_count: 0,
        index_off: HEADER_LEN as u64,
        index_len: 0,
        index_raw_len: 0,
        index_hash: 0,
        data_start: HEADER_LEN as u64,
        tail_start: HEADER_LEN as u64,
        doc_count: 0,
        generation: 0,
        flags: 0,
    };
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(&encode_header(&header))?;
    let seeds = [
        (
            "fact",
            "cm identity",
            "cm 是单文件压缩 mmap 记忆库，使用 XXH3 校验记录",
        ),
        ("decision", "write model", "选择追加写，不做随机索引"),
        ("decision", "deletion", "删除采用 tombstone 加按需 purge"),
        (
            "decision",
            "merge strategy",
            "合并使用 BM25 召回候选，MinHash 复核高度相似",
        ),
        ("constraint", "format", "不保留 Markdown 作为记忆"),
        (
            "procedure",
            "read order",
            "读取顺序为 header -> index -> records",
        ),
    ];
    if !empty {
        for (kind, subject, text) in seeds {
            let memory = new_memory(
                kind,
                "cm",
                subject,
                text,
                "init",
                1.0,
                Vec::new(),
                None,
                None,
            );
            file.write_all(&crate::store::encode_record(
                2,
                0,
                &memory_payload(&memory),
            )?)?;
        }
    }
    file.sync_all()?;
    drop(file);
    reindex_file(path)?;
    outln!(
        "{}",
        json!({"ok": true, "file": path.display().to_string(), "records": if empty { 0 } else { seeds.len() }})
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
) -> Result<()> {
    validate_input(input)?;
    read_header_file(path)?;
    let memory = memory_from_input(input)?;
    if !force {
        if let Some(id) = find_tail_duplicate(path, &memory)? {
            outln!("{}", json!({"ok": true, "id": id, "duplicate": true}));
            return Ok(());
        }
    }
    append_record(path, 2, &memory_payload(&memory), durable)?;
    outln!("{}", json!({"ok": true, "id": memory.id}));
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
        query,
        top,
        scope,
        task,
        kind,
        min_score,
        min_trust,
        include_expired,
    )?;
    outln!("{}", serde_json::to_string(&output)?);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn search_out(
    path: &Path,
    query: &str,
    top: usize,
    scope: Option<String>,
    task: Option<String>,
    kind: Option<String>,
    min_score: f64,
    min_trust: Option<String>,
    include_expired: bool,
) -> Result<SearchOut> {
    let mut store = Store::open(path)?;
    store.set_cache_affinity(scope.as_deref(), task.as_deref());
    let tail = store.tail_view()?;
    let scope_id = match scope.as_deref() {
        Some(value) => store.scope_id(value)?,
        None => None,
    };
    let kind_id = match kind.as_deref() {
        Some(value) => match kind_id(value) {
            Some(id) => Some(id),
            None => {
                return Ok(SearchOut {
                    query: query.to_string(),
                    candidates: Vec::new(),
                });
            }
        },
        None => None,
    };
    let min_rank = match min_trust.as_deref() {
        Some(value) => trust_rank(value),
        None => -1,
    };
    if min_trust.is_some() && min_rank < 0 {
        bail!("invalid trust level {:?}", min_trust);
    }
    let now = crate::model::now_ms();
    let mut hidden_docs = HashSet::new();
    for id in &tail.hidden {
        if let Some((doc_idx, _)) = store.find_doc(id)? {
            hidden_docs.insert(doc_idx);
        }
    }
    let mut hits: HashMap<String, (f64, crate::model::Memory)> = HashMap::new();
    if scope.is_none() || scope_id.is_some() {
        for (doc_idx, score) in store.bm25(
            query,
            scope_id,
            kind_id,
            None,
            Some(&hidden_docs),
            top.min(100),
        )? {
            let meta = store.doc_meta(doc_idx)?;
            let memory = store.doc_memory(&meta)?;
            if !tail.hidden.contains(&memory.id) {
                hits.insert(memory.id.clone(), (score, memory));
            }
        }
    }
    for (score, memory) in tail.bm25(query, scope.as_deref(), kind.as_deref(), None, top.min(100)) {
        hits.insert(memory.id.clone(), (score, memory));
    }
    let mut candidates: Vec<SearchHit> = hits
        .into_values()
        .filter(|(bm25, memory)| {
            *bm25 >= min_score
                && (include_expired || memory.valid_to.is_none_or(|end| end > now))
                && trust_rank(&memory.trust) >= min_rank
        })
        .map(|(bm25, memory)| SearchHit {
            id: memory.id.clone(),
            bm25,
            kind: memory.kind.clone(),
            scope: memory.scope.clone(),
            subject: memory.subject.clone(),
            preview: preview(&memory.text, 160),
        })
        .collect();
    candidates.sort_by(|a, b| {
        b.bm25
            .partial_cmp(&a.bm25)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
    candidates.truncate(top);
    let ids: Vec<String> = candidates.iter().map(|hit| hit.id.clone()).collect();
    store.prefetch_memory_ids(&ids)?;
    Ok(SearchOut {
        query: query.to_string(),
        candidates,
    })
}

fn trust_rank(trust: &str) -> i32 {
    match trust {
        "verified" => 3,
        "authored" => 2,
        "imported" => 1,
        "inferred" => 0,
        _ => -1,
    }
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
) -> Result<()> {
    let mut store = Store::open(path)?;
    store.set_cache_affinity(scope.as_deref(), task.as_deref());
    let tail = store.tail_view()?;
    let now = crate::model::now_ms();
    let min_rank = match min_trust.as_deref() {
        Some(value) => trust_rank(value),
        None => -1,
    };
    if min_rank < 0 && min_trust.is_some() {
        bail!("invalid trust level {:?}", min_trust);
    }
    let scope_id = match scope.as_deref() {
        Some(value) => store.scope_id(value)?,
        None => None,
    };
    let kind_id = match kind.as_deref() {
        Some(value) => match kind_id(value) {
            Some(id) => Some(id),
            None => {
                bail!("invalid kind {:?}", value);
            }
        },
        None => None,
    };
    let mut hits: HashMap<String, (f64, crate::model::Memory)> = HashMap::new();
    if scope.is_none() || scope_id.is_some() {
        for (doc_idx, score) in store.bm25(
            query,
            scope_id,
            kind_id,
            None,
            None,
            top.saturating_mul(4).min(256),
        )? {
            let meta = store.doc_meta(doc_idx)?;
            let memory = store.doc_memory(&meta)?;
            if tail.hidden.contains(&memory.id) {
                continue;
            }
            hits.insert(memory.id.clone(), (score, memory));
        }
    }
    for (score, memory) in tail.bm25(
        query,
        scope.as_deref(),
        kind.as_deref(),
        None,
        top.saturating_mul(4).min(256),
    ) {
        hits.insert(memory.id.clone(), (score, memory));
    }
    let mut candidates: Vec<(f64, crate::model::Memory)> = hits
        .into_values()
        .filter(|(score, memory)| {
            let not_expired = memory.valid_to.is_none_or(|end| end > now);
            let trust_ok = trust_rank(&memory.trust) >= min_rank;
            *score >= min_score && not_expired && trust_ok
        })
        .collect();
    candidates.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.id.cmp(&b.1.id))
    });

    let mut selected = Vec::new();
    let mut estimated = 0usize;
    for (_, memory) in candidates {
        if selected.len() >= max_items {
            break;
        }
        let mut projected = memory;
        if compact {
            if !selected.is_empty() {
                let snippet = memory_snippet(&projected, query, 240);
                projected.text = snippet;
            } else {
                let line = cmx_line(&projected);
                let line_tokens = estimate_tokens(&line);
                if estimated + line_tokens > budget {
                    let snippet = memory_snippet(&projected, query, 240);
                    projected.text = snippet;
                }
            }
        }
        let line = cmx_line(&projected);
        let line_tokens = estimate_tokens(&line);
        if estimated + line_tokens > budget && !selected.is_empty() {
            break;
        }
        if estimated + line_tokens > budget {
            continue;
        }
        estimated += line_tokens;
        selected.push(projected);
    }
    let ids: Vec<String> = selected.iter().map(|memory| memory.id.clone()).collect();
    store.prefetch_memory_ids(&ids)?;

    match format.as_str() {
        "cmx" => {
            outln!("CMX/1");
            for memory in &selected {
                outln!("{}", cmx_line(memory));
            }
        }
        "json" => {
            outln!(
                "{}",
                serde_json::to_string(&ContextOut {
                    query: query.to_string(),
                    budget,
                    estimated_tokens: estimated,
                    compact,
                    memories: selected.iter().map(MemoryOut::from).collect(),
                })?
            );
        }
        other => bail!("invalid context format {other:?}; expected cmx or json"),
    }
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
    if source_ids.len() < 2 {
        bail!("merge requires at least two source ids");
    }
    let mut store = Store::open(path)?;
    let tail = store.tail_view()?;
    let mut selected = Vec::new();
    for id in &source_ids {
        if tail.hidden.contains(id) {
            bail!("active memory not found: {id}");
        }
        let memory = if let Some((_, memory)) = store.find_doc(id)? {
            memory
        } else {
            tail.get(id)
                .cloned()
                .ok_or_else(|| anyhow!("active memory not found: {id}"))?
        };
        selected.push(memory);
    }
    selected.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.text.len().cmp(&a.text.len()))
            .then_with(|| a.id.cmp(&b.id))
    });
    if selected
        .iter()
        .any(|memory| memory.kind != selected[0].kind || memory.scope != selected[0].scope)
    {
        bail!("manual merge requires the same kind and scope");
    }
    let merged = build_merged(
        &selected[0],
        &source_ids,
        &text.unwrap_or_else(|| selected[0].text.clone()),
        "manual",
        1.0,
    );
    append_record(path, 3, &memory_payload(&merged), false)?;
    outln!(
        "{}",
        json!({"ok": true, "id": merged.id, "sources": source_ids})
    );
    Ok(())
}

pub(crate) fn cmd_forget(path: &Path, id: &str) -> Result<()> {
    let mut store = Store::open(path)?;
    let tail = store.tail_view()?;
    let exists =
        !tail.hidden.contains(id) && (store.find_doc(id)?.is_some() || tail.get(id).is_some());
    if !exists {
        bail!("active memory not found: {id}");
    }
    let mut payload = Vec::new();
    put_text(&mut payload, 1, id);
    put_text(&mut payload, 2, "user_request");
    put_u64(&mut payload, 3, crate::model::now_ms());
    append_record(path, 4, &payload, false)?;
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
    let conflicts = conflict_groups(memories);
    outln!(
        "{}",
        serde_json::to_string(&json!({"ok": true, "conflicts": conflicts}))?
    );
    Ok(())
}

/// Active memories in the canonical verify accounting: index docs flagged
/// active in the bitset minus tail-hidden ids, plus active tail memories.
/// Decodes each active doc once; never scans inactive docs.
fn active_memories(store: &mut Store) -> Result<Vec<crate::model::Memory>> {
    let tail = store.tail_view()?;
    let mut memories = Vec::new();
    for idx in 0..store.doc_count() {
        if !store.is_active(idx)? {
            continue;
        }
        let meta = store.doc_meta(idx)?;
        let memory = store.doc_memory(&meta)?;
        if tail.hidden.contains(&memory.id) {
            continue;
        }
        memories.push(memory);
    }
    memories.extend(tail.memories);
    Ok(memories)
}

fn conflict_groups(memories: Vec<crate::model::Memory>) -> Vec<ConflictGroupOut> {
    let mut groups: HashMap<(String, String, String), Vec<crate::model::Memory>> = HashMap::new();
    for memory in memories {
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
                records: records.iter().map(MemoryOut::from).collect(),
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

                let groups = conflict_groups(memories);
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

fn cmd_consolidate(
    path: &Path,
    apply: bool,
    top: usize,
    limit: usize,
    offset: usize,
    threshold: f64,
) -> Result<()> {
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
        if !tail.hidden.contains(&memory.id) {
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
    outln!(
        "{}",
        serde_json::to_string(&ConsolidateOut {
            groups: output,
            applied: apply
        })?
    );
    Ok(())
}

fn validate_trust(trust: &str) -> Result<()> {
    if matches!(trust, "authored" | "verified" | "imported" | "inferred") {
        Ok(())
    } else {
        bail!("invalid trust {trust:?}; expected authored/verified/imported/inferred")
    }
}

fn memory_from_input(input: &AppendInput) -> Result<crate::model::Memory> {
    validate_input(input)?;
    let trust = input.trust.as_deref().unwrap_or("authored");
    validate_trust(trust)?;
    let mut memory = new_memory(
        &input.kind,
        &input.scope,
        &input.subject,
        &input.text,
        &input.source,
        input.confidence.clamp(0.0, 1.0),
        Vec::new(),
        None,
        None,
    );
    memory.valid_from = if input.valid_from == 0 {
        crate::model::now_ms()
    } else {
        input.valid_from
    };
    memory.valid_to = input.valid_to;
    memory.supersedes = input.supersedes.clone();
    memory.conflict_of = input.conflict_of.clone();
    memory.trust = trust.to_string();
    memory.source_commit = input.source_commit.clone();
    Ok(memory)
}

fn validate_memory(kind: &str, scope: &str, subject: &str, text: &str) -> Result<()> {
    if subject.trim().is_empty() || text.trim().is_empty() {
        bail!("subject and text must not be empty");
    }
    if scope.trim().is_empty() {
        bail!("scope must not be empty");
    }
    if !KINDS.contains(&kind) {
        bail!("invalid kind {:?}", kind);
    }
    Ok(())
}

fn validate_input(input: &AppendInput) -> Result<()> {
    validate_memory(&input.kind, &input.scope, &input.subject, &input.text)
}

fn put_text(out: &mut Vec<u8>, id: u8, value: &str) {
    out.push(id);
    out.push(1);
    out.extend_from_slice(&(value.len() as u32).to_le_bytes());
    out.extend_from_slice(value.as_bytes());
}
fn put_u64(out: &mut Vec<u8>, id: u8, value: u64) {
    out.push(id);
    out.push(2);
    out.extend_from_slice(&8u32.to_le_bytes());
    out.extend_from_slice(&value.to_le_bytes());
}
fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|arg| arg == flag)
        .and_then(|index| args.get(index + 1))
        .cloned()
}
fn arg_path(args: &[String]) -> PathBuf {
    PathBuf::from(arg_value(args, "--file").unwrap_or_else(|| "cm".to_string()))
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
