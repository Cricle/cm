use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::Path;
use std::process::Command;

const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "dist",
    "build",
    "vendor",
    "coverage",
    "out",
    "bin",
    "obj",
    ".git",
    ".hg",
    ".svn",
    ".venv",
    "venv",
    "__pycache__",
];

const SKIP_FILES: &[&str] = &["cm", "cm.cache", "cm.lock", "Cargo.lock", "pnpm-lock.yaml"];

const SOURCE_EXTS: &[&str] = &[
    "rs", "ts", "tsx", "js", "jsx", "mjs", "cjs", "py", "go", "java", "kt", "kts", "c", "h", "cc",
    "cpp", "cxx", "hpp", "cs", "rb", "php", "swift", "scala", "sh", "bash", "sql", "lua", "dart",
    "ex", "exs", "erl", "hs", "ml", "clj", "vue", "svelte",
];

const MANIFEST_NAMES: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "pnpm-workspace.yaml",
    "pyproject.toml",
    "requirements.txt",
    "go.mod",
    "Makefile",
    "CMakeLists.txt",
    "build.gradle",
    "settings.gradle",
    "pom.xml",
    "composer.json",
    "Gemfile",
    "mix.exs",
    "deno.json",
    "tsconfig.json",
];

const ENTRY_NAMES: &[&str] = &[
    "main.rs",
    "lib.rs",
    "main.ts",
    "main.tsx",
    "index.ts",
    "index.tsx",
    "main.js",
    "index.js",
    "app.py",
    "main.py",
    "manage.py",
    "main.go",
];

pub(crate) fn is_explore_input(path: &str) -> bool {
    if path
        .split('/')
        .any(|part| matches!(part, ".cm" | ".git" | ".pi" | ".zcode"))
    {
        return false;
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    if SKIP_FILES.contains(&name) {
        return false;
    }
    let extension = name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_lowercase())
        .unwrap_or_default();
    SOURCE_EXTS.contains(&extension.as_str())
        || matches!(
            extension.as_str(),
            "toml" | "yaml" | "yml" | "json" | "md" | "txt" | "lock"
        )
}

#[derive(Clone, Debug)]
pub(crate) struct DirStat {
    pub path: String,
    pub files: usize,
    pub sources: usize,
}

pub(crate) struct RepoScan {
    pub files: usize,
    pub source_files: usize,
    /// Repository-relative paths admitted by the scan. Callers that need
    /// precise targets (for example wiki module pages) can reuse them instead
    /// of walking the tree a second time.
    pub file_paths: Vec<String>,
    pub top_dirs: Vec<DirStat>,
    /// Nested source-bearing directories (up to four levels) so a monorepo
    /// area such as `pkg/wiki-search/src/mes` is a usable explore target.
    pub areas: Vec<DirStat>,
    pub manifests: Vec<String>,
    pub entrypoints: Vec<String>,
}

const MAX_AREA_DEPTH: usize = 4;

pub(crate) fn scan_repo(root: &Path) -> RepoScan {
    // Prefer git's file walk: it honors .gitignore and returns the complete
    // recursive tree, not just the top level. Fall back to a breadth-first
    // filesystem walk for non-git directories.
    if let Ok(paths) = git_paths(root) {
        return scan_paths(paths);
    }
    scan_filesystem(root)
}

fn git_paths(root: &Path) -> Result<Vec<String>, ()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .output()
        .map_err(|_| ())?;
    if !output.status.success() {
        return Err(());
    }
    let mut paths = Vec::new();
    for raw in output.stdout.split(|byte| *byte == 0) {
        if raw.is_empty() {
            continue;
        }
        let path = String::from_utf8_lossy(raw).replace('\\', "/");
        if !path.is_empty() {
            paths.push(path);
        }
    }
    Ok(paths)
}

fn scan_paths(paths: Vec<String>) -> RepoScan {
    let mut scan = RepoScan {
        files: 0,
        source_files: 0,
        file_paths: Vec::new(),
        top_dirs: Vec::new(),
        areas: Vec::new(),
        manifests: Vec::new(),
        entrypoints: Vec::new(),
    };
    let mut dirs: HashMap<String, (usize, usize)> = HashMap::new();
    let mut areas: HashMap<String, (usize, usize)> = HashMap::new();
    for relative in paths {
        let relative = relative.trim_start_matches("./").to_string();
        let components: Vec<&str> = relative.split('/').collect();
        let Some(name) = components.last() else {
            continue;
        };
        if components
            .iter()
            .any(|part| part.is_empty() || part.starts_with('.') || SKIP_DIRS.contains(part))
            || SKIP_FILES.contains(name)
        {
            continue;
        }
        add_scan_file(&mut scan, &mut dirs, &mut areas, &relative, name);
    }
    finish_scan(scan, dirs, areas)
}

fn scan_filesystem(root: &Path) -> RepoScan {
    let mut scan = RepoScan {
        files: 0,
        source_files: 0,
        file_paths: Vec::new(),
        top_dirs: Vec::new(),
        areas: Vec::new(),
        manifests: Vec::new(),
        entrypoints: Vec::new(),
    };
    let mut dirs: HashMap<String, (usize, usize)> = HashMap::new();
    let mut areas: HashMap<String, (usize, usize)> = HashMap::new();
    // Breadth-first keeps every top-level area visible before descending into
    // a deep branch. Walk until the queue is empty: no depth or file-count
    // cutoff, because a snapshot must cover the whole repository.
    let mut queue = VecDeque::from([root.to_path_buf()]);
    while let Some(dir) = queue.pop_front() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let path = entry.path();
            if file_type.is_dir() {
                if name.starts_with('.') || SKIP_DIRS.contains(&name.as_str()) {
                    continue;
                }
                queue.push_back(path);
                continue;
            }
            if !file_type.is_file() || name.starts_with('.') || SKIP_FILES.contains(&name.as_str())
            {
                continue;
            }
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            let relative = relative.to_string_lossy().replace('\\', "/");
            add_scan_file(&mut scan, &mut dirs, &mut areas, &relative, &name);
        }
    }
    finish_scan(scan, dirs, areas)
}

fn add_scan_file(
    scan: &mut RepoScan,
    dirs: &mut HashMap<String, (usize, usize)>,
    areas: &mut HashMap<String, (usize, usize)>,
    relative: &str,
    name: &str,
) {
    scan.files += 1;
    scan.file_paths.push(relative.to_string());
    let extension = Path::new(relative)
        .extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let is_source = SOURCE_EXTS.contains(&extension.as_str());
    let components: Vec<&str> = relative.split('/').collect();
    if let Some(top) = components.first().filter(|_| components.len() > 1) {
        let stats = dirs.entry(top.to_string()).or_default();
        stats.0 += 1;
        if is_source {
            stats.1 += 1;
        }
    }
    // Track nested source-bearing directories as candidate exploration areas.
    // Ancestor duplicates are fine; finish_scan keeps the most specific and
    // source-heavy paths first.
    if is_source {
        let max_depth = components.len().saturating_sub(1).min(MAX_AREA_DEPTH);
        for depth in 2..=max_depth {
            let area = components[..depth].join("/");
            let stats = areas.entry(area).or_default();
            stats.0 += 1;
            stats.1 += 1;
        }
        scan.source_files += 1;
    }
    let depth = components.len().saturating_sub(1);
    if depth <= 3 && MANIFEST_NAMES.contains(&name) {
        scan.manifests.push(relative.to_string());
    }
    if is_source && depth <= 6 && ENTRY_NAMES.contains(&name) && scan.entrypoints.len() < 24 {
        scan.entrypoints.push(relative.to_string());
    }
}

fn finish_scan(
    mut scan: RepoScan,
    dirs: HashMap<String, (usize, usize)>,
    areas: HashMap<String, (usize, usize)>,
) -> RepoScan {
    scan.manifests.sort();
    scan.manifests.dedup();
    scan.entrypoints.sort();
    scan.entrypoints.dedup();
    scan.top_dirs = dirs
        .into_iter()
        .map(|(path, (files, sources))| DirStat {
            path,
            files,
            sources,
        })
        .collect();
    scan.top_dirs.sort_by(|left, right| {
        right
            .sources
            .cmp(&left.sources)
            .then_with(|| right.files.cmp(&left.files))
            .then_with(|| left.path.cmp(&right.path))
    });
    scan.top_dirs.truncate(20);
    let areas: Vec<DirStat> = areas
        .into_iter()
        .map(|(path, (files, sources))| DirStat {
            path,
            files,
            sources,
        })
        .filter(|area| area.sources >= 2)
        .collect();
    let all_paths: HashSet<String> = areas.iter().map(|area| area.path.clone()).collect();
    // Keep only leaf areas: a package root is less actionable than its source
    // subdirectories, and the target budget must not be spent on ancestors.
    let mut selected_areas: Vec<DirStat> = areas
        .into_iter()
        .filter(|area| {
            !all_paths.iter().any(|path| {
                path.len() > area.path.len() && path.starts_with(&format!("{}/", area.path))
            })
        })
        .collect();
    selected_areas.sort_by(|left, right| {
        right
            .sources
            .cmp(&left.sources)
            .then_with(|| right.files.cmp(&left.files))
            .then_with(|| left.path.cmp(&right.path))
    });
    selected_areas.truncate(40);
    scan.areas = selected_areas;
    scan
}
