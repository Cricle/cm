use std::collections::HashMap;
use std::fs;
use std::path::Path;

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

pub(crate) struct DirStat {
    pub path: String,
    pub files: usize,
    pub sources: usize,
}

pub(crate) struct RepoScan {
    pub files: usize,
    pub source_files: usize,
    pub top_dirs: Vec<DirStat>,
    pub manifests: Vec<String>,
    pub entrypoints: Vec<String>,
}

pub(crate) fn scan_repo(root: &Path) -> RepoScan {
    let mut scan = RepoScan {
        files: 0,
        source_files: 0,
        top_dirs: Vec::new(),
        manifests: Vec::new(),
        entrypoints: Vec::new(),
    };
    let mut dirs: HashMap<String, (usize, usize)> = HashMap::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > 8 || scan.files >= 20_000 {
            break;
        }
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
                stack.push((path, depth + 1));
                continue;
            }
            if !file_type.is_file() || name.starts_with('.') || SKIP_FILES.contains(&name.as_str())
            {
                continue;
            }
            scan.files += 1;
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let extension = path
                .extension()
                .map(|extension| extension.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            let is_source = SOURCE_EXTS.contains(&extension.as_str());
            if let Some((top, _)) = relative.split_once('/') {
                let stats = dirs.entry(top.to_string()).or_default();
                stats.0 += 1;
                if is_source {
                    stats.1 += 1;
                }
            }
            if is_source {
                scan.source_files += 1;
            }
            if depth <= 3 && MANIFEST_NAMES.contains(&name.as_str()) {
                scan.manifests.push(relative.clone());
            }
            if is_source
                && depth <= 6
                && ENTRY_NAMES.contains(&name.as_str())
                && scan.entrypoints.len() < 24
            {
                scan.entrypoints.push(relative);
            }
        }
    }
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
    scan
}
