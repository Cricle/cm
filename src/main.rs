use std::collections::HashMap;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::model::{
    build_merged, cmx_line, kind_id, memory_payload, minhash_signature, minhash_similarity,
    new_memory, pair_similarity, parse_ids, preview, Dsu, KINDS,
};
use crate::store::{
    append_record, encode_header, read_header_file, reindex_file, Header, Store, HEADER_LEN,
};

mod cache;
mod model;
mod store;

const SKILL_TEXT: &str = include_str!("../assets/SKILL.md");

macro_rules! outln {
    ($($arg:tt)*) => {{
        let _ = writeln!(io::stdout(), $($arg)*);
    }};
}

#[derive(Deserialize)]
struct AppendInput {
    kind: String,
    scope: String,
    subject: String,
    text: String,
    #[serde(default)]
    source: String,
    #[serde(default = "default_confidence")]
    confidence: f64,
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
struct MemoryOut {
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
struct SearchHit {
    id: String,
    bm25: f64,
    kind: String,
    scope: String,
    subject: String,
    preview: String,
}

#[derive(Serialize)]
struct SearchOut {
    query: String,
    candidates: Vec<SearchHit>,
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

fn main() {
    if let Err(error) = run() {
        eprintln!("cm: {error:#}");
        process::exit(1);
    }
}

fn run() -> Result<()> {
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
            cmd_append(&arg_path(&args), &input)
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
                arg_value(&args, "--kind"),
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
        "verify" => cmd_verify(&arg_path(&args)),
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
        "cm 1.0.0 - single-file compressed mmap memory\n\
         \n\
         cm init [--file cm] [--empty]\n\
         cm append --file cm --json '<json>'\n\
         cm import --file cm < records.jsonl\n\
         cm export --file cm [--format cmx|jsonl]\n\
         cm reindex [--file cm]\n\
         cm skill [--json] [--install] [--check] [--dir <path>]\n\
         cm get --file cm --id <id>\n\
         cm scan [--file cm] [--limit 100] [--offset 0]\n\
         cm search --file cm --query <text> [--top 8] [--scope <scope>] [--kind <kind>]\n\
         cm similar --file cm --id <id> [--top 16] [--scope <scope>]\n\
         cm merge --file cm --sources <id1,id2> [--text <text>]\n\
         cm consolidate --file cm [--apply] [--top 8] [--limit 50] [--offset 0] [--threshold 0.85]\n\
         cm forget --file cm --id <id>\n\
         cm purge --file cm\n\
         cm verify --file cm"
    );
}

fn cmd_init(path: &Path, empty: bool) -> Result<()> {
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

fn cmd_append(path: &Path, input_json: &str) -> Result<()> {
    let input: AppendInput = serde_json::from_str(input_json).context("parse --json input")?;
    validate_input(&input)?;
    read_header_file(path)?;
    let memory = new_memory(
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
    append_record(path, 2, &memory_payload(&memory))?;
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
    let file = OpenOptions::new().append(true).open(path)?;
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
                full.into_memory()
            }
            ImportLine::Append(input) => {
                validate_input(&input)?;
                new_memory(
                    &input.kind,
                    &input.scope,
                    &input.subject,
                    &input.text,
                    &input.source,
                    input.confidence.clamp(0.0, 1.0),
                    Vec::new(),
                    None,
                    None,
                )
            }
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
    drop(writer);
    reindex_file(path)?;
    outln!(
        "{}",
        json!({"ok": true, "imported": count, "reindexed": true})
    );
    Ok(())
}

fn cmd_skill(json_output: bool, install: bool, check: bool, dir: Option<String>) -> Result<()> {
    let dir = PathBuf::from(dir.unwrap_or_else(|| ".agents/skills/cm-memory".to_string()));
    let path = dir.join("SKILL.md");

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

fn cmd_reindex(path: &Path) -> Result<()> {
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

fn cmd_get(path: &Path, id: &str) -> Result<()> {
    let mut store = Store::open(path)?;
    let tail = store.tail_view()?;
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

fn cmd_search(
    path: &Path,
    query: &str,
    top: usize,
    scope: Option<String>,
    kind: Option<String>,
) -> Result<()> {
    let mut store = Store::open(path)?;
    let tail = store.tail_view()?;
    let scope_id = scope.as_deref().and_then(|value| store.scope_id(value));
    let kind_id = match kind.as_deref() {
        Some(value) => match kind_id(value) {
            Some(id) => Some(id),
            None => {
                outln!(
                    "{}",
                    serde_json::to_string(&SearchOut {
                        query: query.to_string(),
                        candidates: Vec::new()
                    })?
                );
                return Ok(());
            }
        },
        None => None,
    };
    let mut hits: HashMap<String, (f64, crate::model::Memory)> = HashMap::new();
    if scope.is_none() || scope_id.is_some() {
        for (doc_idx, score) in store.bm25(query, scope_id, kind_id, None, top.min(100))? {
            let meta = store.doc_meta(doc_idx)?;
            let memory = store.doc_memory(&meta)?;
            if !tail.hidden.contains(&memory.id) {
                hits.insert(memory.id.clone(), (score, memory));
            }
        }
    }
    for (score, memory) in tail.bm25(query, scope.as_deref(), None, None, top.min(100)) {
        hits.insert(memory.id.clone(), (score, memory));
    }
    let mut candidates: Vec<SearchHit> = hits
        .into_values()
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
    });
    candidates.truncate(top);
    outln!(
        "{}",
        serde_json::to_string(&SearchOut {
            query: query.to_string(),
            candidates
        })?
    );
    Ok(())
}

fn cmd_similar(path: &Path, id: &str, top: usize, scope: Option<String>) -> Result<()> {
    let mut store = Store::open(path)?;
    let tail = store.tail_view()?;
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
    let scope_id = store.scope_id(scope);
    let mut hits: HashMap<String, (f64, crate::model::Memory)> = HashMap::new();
    if let Some(scope_id) = scope_id {
        for (doc_idx, score) in store.bm25(
            &format!("{} {}", target.subject, target.text),
            Some(scope_id),
            None,
            target_idx,
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
    append_record(path, 3, &memory_payload(&merged))?;
    outln!(
        "{}",
        json!({"ok": true, "id": merged.id, "sources": source_ids})
    );
    Ok(())
}

fn cmd_forget(path: &Path, id: &str) -> Result<()> {
    let mut store = Store::open(path)?;
    let tail = store.tail_view()?;
    let exists = store.find_doc(id)?.is_some() || tail.get(id).is_some();
    if !exists {
        bail!("active memory not found: {id}");
    }
    let mut payload = Vec::new();
    put_text(&mut payload, 1, id);
    put_text(&mut payload, 2, "user_request");
    put_u64(&mut payload, 3, crate::model::now_ms());
    append_record(path, 4, &payload)?;
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

fn cmd_verify(path: &Path) -> Result<()> {
    let mut store = Store::open(path)?;
    let tail = store.tail_view()?;
    let mut active_index = store.active_count();
    for id in &tail.hidden {
        if let Some((idx, _)) = store.find_doc(id)? {
            if store.is_active(idx)? {
                active_index = active_index.saturating_sub(1);
            }
        }
    }
    outln!(
        "{}",
        json!({
            "ok": true,
            "bytes": fs::metadata(path)?.len(),
            "version": 1,
            "blocks": store.header.block_count,
            "index_docs": store.header.doc_count,
            "active_index_docs": active_index,
            "tail_memories": tail.memories.len(),
            "active_total": active_index + tail.memories.len() as u64,
            "index_bytes": store.header.index_len,
            "index_raw_bytes": store.header.index_raw_len
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
        let scope_id = store.scope_id(&memory.scope);
        let kind = kind_id(&memory.kind);
        for (j, _) in store.bm25(
            &format!("{} {}", memory.subject, memory.text),
            scope_id,
            kind,
            Some(*doc_idx),
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
            append_record(path, 3, &memory_payload(merged))?;
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
