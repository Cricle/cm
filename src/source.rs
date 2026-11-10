use std::fs;
use std::path::{Path, PathBuf};

use xxhash_rust::xxh3::xxh3_64;

use crate::model::Memory;

pub(crate) fn source_parts(source: &str) -> Option<(PathBuf, Option<String>)> {
    let source = source.trim();
    if source.is_empty() || source.starts_with("agent:") || source.contains("://") {
        return None;
    }
    if let Some(index) = source.char_indices().find_map(|(index, ch)| {
        if ch != ':' {
            return None;
        }
        let left = &source[..index];
        (left.len() > 1 && (left.contains('/') || left.contains('\\') || left.contains('.')))
            .then_some(index)
    }) {
        let path = &source[..index];
        let symbol = &source[index + 1..];
        if !symbol.is_empty() {
            return Some((PathBuf::from(path), Some(symbol.to_string())));
        }
    }
    let path = PathBuf::from(source);
    let pathish = source.contains('/') || source.contains('\\') || source.contains('.');
    if pathish {
        Some((path, None))
    } else {
        None
    }
}

pub(crate) fn anchor_text(text: &str, symbol: Option<&str>) -> Option<(u32, String)> {
    if let Some(symbol) = symbol {
        let lines: Vec<&str> = text.lines().collect();
        let index = find_symbol_line(&lines, symbol)?;
        let start = index.saturating_sub(2);
        let end = (index + 40).min(lines.len());
        let excerpt = lines[start..end].join("\n");
        Some((
            (index + 1) as u32,
            format!("{:016x}", xxh3_64(excerpt.as_bytes())),
        ))
    } else {
        Some((0, format!("{:016x}", xxh3_64(text.as_bytes()))))
    }
}

pub(crate) fn find_symbol_line(lines: &[&str], symbol: &str) -> Option<usize> {
    let mut names = vec![symbol];
    if let Some(last) = symbol.rsplit("::").next().filter(|last| *last != symbol) {
        names.push(last);
    }
    if let Some(last) = symbol.rsplit('.').next().filter(|last| *last != symbol) {
        names.push(last);
    }
    for name in names {
        if let Some(index) = find_symbol_line_exact(lines, name) {
            return Some(index);
        }
    }
    None
}

pub(crate) fn find_symbol_line_exact(lines: &[&str], symbol: &str) -> Option<usize> {
    const KEYWORDS: &[&str] = &[
        "fn",
        "function",
        "def",
        "class",
        "struct",
        "enum",
        "trait",
        "interface",
        "type",
        "const",
        "let",
        "var",
        "impl",
        "module",
        "namespace",
        "func",
        "fun",
        "record",
        "object",
        "protocol",
        "extension",
        "actor",
    ];
    const MODIFIERS: &[&str] = &[
        "export",
        "default",
        "async",
        "pub",
        "public",
        "private",
        "protected",
        "static",
        "readonly",
        "abstract",
        "declare",
        "override",
        "final",
        "open",
        "unsafe",
        "extern",
        "sealed",
        "mut",
    ];
    let mut fallback = None;
    for (index, raw) in lines.iter().enumerate() {
        let line = raw.trim_start();
        if line.is_empty() || is_comment_line(line) {
            continue;
        }
        let Some(start) = identifier_occurrence(line, symbol) else {
            continue;
        };
        fallback.get_or_insert(index);
        let prefix = line[..start].trim_end();
        let previous = previous_word(prefix);
        let after = line[start + symbol.len()..].trim_start();
        if KEYWORDS.contains(&previous.as_str())
            || (prefix
                .split_whitespace()
                .all(|word| MODIFIERS.contains(&word))
                && after.starts_with(['(', '<', '=', '{', ':', '[', '!']))
            || (prefix.starts_with("func ") && after.starts_with('('))
        {
            return Some(index);
        }
    }
    fallback
}

pub(crate) fn is_comment_line(line: &str) -> bool {
    line.starts_with("//")
        || line.starts_with("/*")
        || line.starts_with('*')
        || line.starts_with("--")
}

pub(crate) fn identifier_occurrence(line: &str, symbol: &str) -> Option<usize> {
    let mut search = 0usize;
    while let Some(found) = line[search..].find(symbol) {
        let start = search + found;
        let end = start + symbol.len();
        let before_ok = line[..start]
            .chars()
            .next_back()
            .is_none_or(|ch| !is_identifier_char(ch));
        let after_ok = line[end..]
            .chars()
            .next()
            .is_none_or(|ch| !is_identifier_char(ch));
        if before_ok && after_ok {
            return Some(start);
        }
        search = end;
    }
    None
}

pub(crate) fn is_identifier_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_' || ch == '$'
}

pub(crate) fn previous_word(prefix: &str) -> String {
    prefix
        .split_whitespace()
        .next_back()
        .unwrap_or_default()
        .trim_matches(|ch: char| matches!(ch, '(' | ')' | '<' | '>' | '*' | '&' | '?' | '!'))
        .to_string()
}

pub(crate) fn source_anchor(source: &str) -> Option<(u32, String)> {
    let cwd = std::env::current_dir().ok()?;
    let mut bases = vec![cwd.clone()];
    let mut current = cwd.as_path();
    while let Some(parent) = current.parent() {
        bases.push(parent.to_path_buf());
        current = parent;
    }
    bases.iter().find_map(|base| source_anchor_at(source, base))
}

/// Resolve a repository-relative source path under `root`.
///
/// Rejects absolute paths, `..`, and empty paths, then resolves symlinks with
/// `canonicalize` and verifies the result still lives under the canonical
/// root. This is the single join used by anchors, snippets and audit reads, so
/// an imported record cannot point the reader outside the project.
pub(crate) fn confined_source_path(root: &Path, relative: &Path) -> Option<PathBuf> {
    if relative.as_os_str().is_empty() {
        return None;
    }
    let mut clean = PathBuf::new();
    for component in relative.components() {
        match component {
            std::path::Component::Normal(part) => clean.push(part),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => return None,
        }
    }
    if clean.as_os_str().is_empty() {
        return None;
    }
    let candidate = root.join(&clean);
    let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let canonical_candidate = fs::canonicalize(&candidate).ok()?;
    path_within(&canonical_candidate, &canonical_root).then_some(canonical_candidate)
}

fn path_within(candidate: &Path, root: &Path) -> bool {
    if cfg!(windows) {
        let candidate = candidate
            .to_string_lossy()
            .replace('/', "\\")
            .to_ascii_lowercase();
        let root = root
            .to_string_lossy()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_ascii_lowercase();
        candidate == root || candidate.starts_with(&(root + "\\"))
    } else {
        candidate == root || candidate.starts_with(root)
    }
}

/// Anchor a source against an explicit project root. A directory anchor is
/// valid for mechanically generated area records; audit treats directories as
/// structural, not content, citations.
pub(crate) fn source_anchor_at(source: &str, root: &Path) -> Option<(u32, String)> {
    let (relative, symbol) = source_parts(source)?;
    let path = confined_source_path(root, &relative)?;
    if path.is_dir() {
        return Some((
            0,
            format!("{:016x}", xxh3_64(relative.to_string_lossy().as_bytes())),
        ));
    }
    let text = fs::read_to_string(path).ok()?;
    anchor_text(&text, symbol.as_deref())
}

pub(crate) fn source_snippet(memory: &Memory, root: &Path, lines: usize) -> Option<String> {
    let (relative, _) = source_parts(&memory.source)?;
    let line = memory.source_line? as usize;
    let path = confined_source_path(root, &relative)?;
    let text = fs::read_to_string(path).ok()?;
    let all: Vec<&str> = text.lines().collect();
    if all.is_empty() || line > all.len() {
        return None;
    }
    let start = line.saturating_sub(1);
    let end = (start + lines.max(1)).min(all.len());
    Some(all[start..end].join("\n"))
}
