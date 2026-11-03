use std::fs;
#[cfg(not(windows))]
use std::fs::OpenOptions;
#[cfg(not(windows))]
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use xxhash_rust::xxh3::xxh3_64;

const MANIFEST_VERSION: u32 = 1;
#[cfg(not(windows))]
const PROFILE_START: &str = "# >>> cm-memory >>>";
#[cfg(not(windows))]
const PROFILE_END: &str = "# <<< cm-memory <<<";

pub(crate) struct AgentSpec {
    pub(crate) id: &'static str,
    pub(crate) path: &'static str,
}

pub(crate) const AGENTS: &[AgentSpec] = &[
    AgentSpec {
        id: "claude",
        path: ".claude/skills",
    },
    AgentSpec {
        id: "cursor",
        path: ".cursor/skills",
    },
    AgentSpec {
        id: "copilot",
        path: ".copilot/skills",
    },
    AgentSpec {
        id: "codex",
        path: ".codex/skills",
    },
    AgentSpec {
        id: "gemini",
        path: ".gemini/skills",
    },
    AgentSpec {
        id: "openclaw",
        path: ".openclaw/skills",
    },
    AgentSpec {
        id: "windsurf",
        path: ".codeium/windsurf/skills",
    },
    AgentSpec {
        id: "cline",
        path: ".cline/skills",
    },
    AgentSpec {
        id: "qoder",
        path: ".qoder/skills",
    },
    AgentSpec {
        id: "qoder-cn",
        path: ".qoder-cn/skills",
    },
    AgentSpec {
        id: "pi",
        path: ".pi/agent/skills",
    },
    AgentSpec {
        id: "opencode",
        path: ".config/opencode/skills",
    },
    AgentSpec {
        id: "roo",
        path: ".roo/skills",
    },
    AgentSpec {
        id: "kilo",
        path: ".kilocode/skills",
    },
    AgentSpec {
        id: "augment",
        path: ".augment/skills",
    },
    AgentSpec {
        id: "continue",
        path: ".continue/skills",
    },
    AgentSpec {
        id: "droid",
        path: ".factory/skills",
    },
    AgentSpec {
        id: "codebuddy",
        path: ".codebuddy/skills",
    },
    AgentSpec {
        id: "codemaker",
        path: ".codemaker/skills",
    },
    AgentSpec {
        id: "bob",
        path: ".bob/skills",
    },
    AgentSpec {
        id: "forge",
        path: ".forge/skills",
    },
    AgentSpec {
        id: "aiderdesk",
        path: ".aider-desk/skills",
    },
    AgentSpec {
        id: "agents",
        path: ".agents/skills",
    },
    AgentSpec {
        id: "agents-config",
        path: ".config/agents/skills",
    },
    AgentSpec {
        id: "antigravity",
        path: ".gemini/antigravity/skills",
    },
    AgentSpec {
        id: "deepagents",
        path: ".deepagents/agent/skills",
    },
    AgentSpec {
        id: "firebender",
        path: ".firebender/skills",
    },
    AgentSpec {
        id: "crush",
        path: ".config/crush/skills",
    },
    AgentSpec {
        id: "devin",
        path: ".config/devin/skills",
    },
];

#[derive(Clone, Serialize, Deserialize)]
struct SkillRecord {
    agent: String,
    path: String,
    #[serde(default)]
    backup: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ExeRecord {
    path: String,
    source: String,
    kind: String,
    installed_hash: String,
    #[serde(default)]
    backup: Option<String>,
    #[serde(default)]
    path_added: bool,
}

#[derive(Default, Serialize, Deserialize)]
pub(crate) struct Manifest {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    skills: Vec<SkillRecord>,
    #[serde(default)]
    pub(crate) exe: Option<ExeRecord>,
}

pub(crate) fn cmd_install(args: &[String], skill: &str) -> Result<()> {
    if has_flag(args, "--list") {
        for spec in AGENTS {
            println!("{}	{}", spec.id, spec.path);
        }
        return Ok(());
    }
    let home = home_arg(args)?;
    let specs = select_specs(args, &home, false)?;
    let dry_run = has_flag(args, "--dry-run");
    let no_exe = has_flag(args, "--no-exe");
    let no_path = has_flag(args, "--no-path");
    let json_output = has_flag(args, "--json");
    let mut manifest = load_manifest(&home)?;
    let mut installed = Vec::new();

    for spec in &specs {
        let path = skill_path(&home, spec);
        if dry_run {
            installed.push(json!({"agent": spec.id, "path": path.display().to_string()}));
            continue;
        }
        let backup = prepare_backup(
            &home,
            &path,
            skill.as_bytes(),
            &format!("{}-skill", spec.id),
        )?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, skill.as_bytes())?;
        upsert_skill(
            &mut manifest,
            SkillRecord {
                agent: spec.id.to_string(),
                path: path.display().to_string(),
                backup,
            },
        );
        installed.push(json!({"agent": spec.id, "path": path.display().to_string()}));
    }

    let mut exe_info = None;
    if !no_exe {
        exe_info = install_exe(&home, dry_run, no_path, &mut manifest)?;
    }
    if !dry_run {
        save_manifest(&home, &manifest)?;
    }

    let result = json!({
        "ok": true,
        "dry_run": dry_run,
        "skills": installed,
        "exe": exe_info,
        "manifest": manifest_path(&home).display().to_string()
    });
    if json_output {
        println!("{}", result);
    } else if dry_run {
        println!(
            "would install {} skill(s) and exe={}",
            installed.len(),
            !no_exe
        );
        for item in &installed {
            println!("  {} -> {}", item["agent"], item["path"]);
        }
    } else {
        println!(
            "installed {} skill(s); exe={}",
            installed.len(),
            if no_exe { "skipped" } else { "registered" }
        );
        for item in &installed {
            println!("  {} -> {}", item["agent"], item["path"]);
        }
    }
    Ok(())
}

pub(crate) fn cmd_uninstall(args: &[String], skill: &str) -> Result<()> {
    let home = home_arg(args)?;
    let dry_run = has_flag(args, "--dry-run");
    let json_output = has_flag(args, "--json");
    let keep_exe = has_flag(args, "--keep-exe");
    let mut manifest = load_manifest(&home)?;
    #[cfg(windows)]
    if !dry_run && !keep_exe && !has_flag(args, "--dispatched") {
        if let Some(record) = &manifest.exe {
            if let Ok(current) = std::env::current_exe() {
                if same_path(Path::new(&record.path), &current) {
                    let helper = std::env::temp_dir()
                        .join(format!("cm-uninstall-{}.exe", crate::model::now_ms()));
                    fs::copy(&current, &helper).context("copy uninstall helper")?;
                    let mut command = std::process::Command::new(&helper);
                    command.args(args).arg("--dispatched");
                    command.spawn().context("spawn uninstall helper")?;
                    if json_output {
                        println!("{}", json!({"ok": true, "dispatched": true}));
                    } else {
                        println!("uninstall dispatched to helper");
                    }
                    return Ok(());
                }
            }
        }
    }
    let selected = select_specs(args, &home, true)?;
    let selected_ids: std::collections::HashSet<&str> =
        selected.iter().map(|spec| spec.id).collect();
    let remove_all = has_flag(args, "--all") || selected_ids.is_empty();
    let mut removed = Vec::new();
    let mut warnings = Vec::new();
    let mut remaining = Vec::new();

    for record in manifest.skills.drain(..) {
        let wanted = remove_all || selected_ids.contains(record.agent.as_str());
        if !wanted {
            remaining.push(record);
            continue;
        }
        let path = PathBuf::from(&record.path);
        if dry_run {
            removed.push(json!({"agent": record.agent, "path": record.path}));
        } else {
            match remove_skill(&path, record.backup.as_deref(), skill.as_bytes()) {
                Ok(()) => removed.push(json!({"agent": record.agent, "path": record.path})),
                Err(error) => warnings.push(format!("{}: {error:#}", record.agent)),
            }
        }
    }
    manifest.skills = remaining;

    let mut exe_removed = false;
    if !keep_exe {
        if let Some(record) = manifest.exe.take() {
            if dry_run {
                exe_removed = true;
            } else {
                match remove_exe(&home, &record) {
                    Ok(()) => exe_removed = true,
                    Err(error) => warnings.push(format!("exe: {error:#}")),
                }
            }
        }
    }
    if !dry_run {
        save_manifest(&home, &manifest)?;
    }

    let result = json!({
        "ok": warnings.is_empty(),
        "dry_run": dry_run,
        "removed_skills": removed,
        "removed_exe": exe_removed,
        "warnings": warnings
    });
    if json_output {
        println!("{}", result);
    } else {
        println!("removed {} skill(s); exe={}", removed.len(), exe_removed);
        for warning in &warnings {
            eprintln!("cm: warning: {warning}");
        }
    }
    if !warnings.is_empty() {
        bail!("uninstall completed with warnings");
    }
    Ok(())
}

fn select_specs(args: &[String], home: &Path, uninstall: bool) -> Result<Vec<&'static AgentSpec>> {
    let all = has_flag(args, "--all");
    let detected =
        has_flag(args, "--detected") || (!all && values(args, "--agent").is_empty() && !uninstall);
    let mut ids = Vec::new();
    for value in values(args, "--agent") {
        for id in value.split(',') {
            if !id.trim().is_empty() {
                ids.push(id.trim().to_string());
            }
        }
    }
    let selected: Vec<&AgentSpec> = if all {
        AGENTS.iter().collect()
    } else if !ids.is_empty() {
        let mut out = Vec::new();
        for id in ids {
            let spec = AGENTS
                .iter()
                .find(|spec| spec.id == id)
                .ok_or_else(|| anyhow::anyhow!("unknown agent {id:?}; use cm install --list"))?;
            out.push(spec);
        }
        out
    } else if detected {
        AGENTS
            .iter()
            .filter(|spec| {
                home.join(spec.path)
                    .parent()
                    .is_some_and(|parent| parent.exists())
            })
            .collect()
    } else {
        Vec::new()
    };
    let mut unique = Vec::new();
    for spec in selected {
        if !unique.iter().any(|item: &&AgentSpec| item.id == spec.id) {
            unique.push(spec);
        }
    }
    if unique.is_empty() && !uninstall {
        bail!("no known agent detected; use --agent <id> or --all");
    }
    Ok(unique)
}

fn install_exe(
    home: &Path,
    dry_run: bool,
    no_path: bool,
    manifest: &mut Manifest,
) -> Result<Option<serde_json::Value>> {
    let bin_dir = home.join(".local").join("bin");
    let target = bin_dir.join(if cfg!(windows) { "cm.exe" } else { "cm" });
    let source = std::env::current_exe().context("locate current executable")?;
    if dry_run {
        return Ok(Some(
            json!({"path": target.display().to_string(), "dry_run": true}),
        ));
    }
    if target.exists() && same_path(&target, &source) {
        let path_added = manifest
            .exe
            .as_ref()
            .is_some_and(|record| record.path_added);
        manifest.exe = Some(ExeRecord {
            path: target.display().to_string(),
            source: source.display().to_string(),
            kind: if cfg!(windows) { "copy" } else { "symlink" }.to_string(),
            installed_hash: hash_file(&target)?,
            backup: None,
            path_added,
        });
        return Ok(Some(json!({
            "path": target.display().to_string(),
            "kind": "already-installed",
            "path_added": path_added
        })));
    }
    fs::create_dir_all(&bin_dir)?;
    let backup = if target.exists() {
        let backup = backup_path(home, "exe")?;
        fs::rename(&target, &backup)?;
        Some(backup.display().to_string())
    } else {
        None
    };
    let kind = if cfg!(windows) {
        fs::copy(&source, &target).context("copy cm executable")?;
        "copy"
    } else {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&source, &target)
                .or_else(|_| fs::copy(&source, &target).map(|_| ()))?;
            if target.is_symlink() {
                "symlink"
            } else {
                "copy"
            }
        }
        #[cfg(not(unix))]
        {
            fs::copy(&source, &target)?;
            "copy"
        }
    };
    let installed_hash = if kind == "copy" {
        hash_file(&target)?
    } else {
        String::new()
    };
    let mut path_added = false;
    let mut warning = None;
    if !no_path {
        match set_path(home, &bin_dir, true) {
            Ok(added) => path_added = added,
            Err(error) => warning = Some(format!("PATH registration failed: {error:#}")),
        }
    }
    manifest.exe = Some(ExeRecord {
        path: target.display().to_string(),
        source: source.display().to_string(),
        kind: kind.to_string(),
        installed_hash,
        backup,
        path_added,
    });
    Ok(Some(json!({
        "path": target.display().to_string(),
        "kind": kind,
        "path_added": path_added,
        "warning": warning
    })))
}

fn remove_exe(home: &Path, record: &ExeRecord) -> Result<()> {
    let target = PathBuf::from(&record.path);
    let backup = record.backup.as_ref().map(PathBuf::from);
    let mut warning = None;
    if target.exists() || target.is_symlink() {
        let matches = if record.kind == "symlink" {
            fs::read_link(&target)
                .ok()
                .and_then(|value| fs::canonicalize(value).ok())
                .zip(fs::canonicalize(&record.source).ok())
                .is_some_and(|(a, b)| a == b)
        } else {
            hash_file(&target)
                .map(|hash| hash == record.installed_hash)
                .unwrap_or(false)
        };
        if matches {
            fs::remove_file(&target)?;
        } else {
            warning = Some(format!(
                "refusing to remove modified executable {}",
                target.display()
            ));
        }
    }
    if let Some(backup) = backup {
        if backup.exists() && target.symlink_metadata().is_err() {
            fs::rename(&backup, &target)?;
        }
    }
    if record.path_added {
        let bin_dir = target.parent().unwrap_or(Path::new("."));
        set_path(home, bin_dir, false)?;
    }
    if let Some(warning) = warning {
        bail!(warning);
    }
    Ok(())
}

fn set_path(home: &Path, bin_dir: &Path, add: bool) -> Result<bool> {
    let _ = home;
    #[cfg(not(windows))]
    let _ = bin_dir;
    #[cfg(windows)]
    {
        let script = if add {
            "$p=[Environment]::GetEnvironmentVariable('Path','User'); if($null -eq $p){$p=''}; $parts=@($p -split ';' | Where-Object {$_ -ne ''}); if($parts -contains $env:CM_BIN){'present'} else {$parts += $env:CM_BIN; [Environment]::SetEnvironmentVariable('Path',($parts -join ';'),'User'); 'added'}"
        } else {
            "$p=[Environment]::GetEnvironmentVariable('Path','User'); if($null -eq $p){$p=''}; $parts=@($p -split ';' | Where-Object {$_ -ne ''}); if($parts -notcontains $env:CM_BIN){'absent'} else {$parts=@($parts | Where-Object {$_ -ne $env:CM_BIN}); [Environment]::SetEnvironmentVariable('Path',($parts -join ';'),'User'); 'removed'}"
        };
        let output = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .env("CM_BIN", bin_dir)
            .output()
            .context("run PowerShell PATH update")?;
        if !output.status.success() {
            bail!(
                "PowerShell PATH update failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let text = String::from_utf8_lossy(&output.stdout);
        Ok(if add {
            text.contains("added")
        } else {
            text.contains("removed")
        })
    }
    #[cfg(not(windows))]
    {
        let profile = home.join(".profile");
        let content = fs::read_to_string(&profile).unwrap_or_default();
        if add {
            if content.contains(PROFILE_START) {
                return Ok(false);
            }
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&profile)?;
            writeln!(
                file,
                "\n{PROFILE_START}\nexport PATH=\"$HOME/.local/bin:$PATH\"\n{PROFILE_END}"
            )?;
            Ok(true)
        } else if content.contains(PROFILE_START) {
            let start = content.find(PROFILE_START).unwrap();
            let end = content.find(PROFILE_END).unwrap() + PROFILE_END.len();
            let mut out = String::new();
            out.push_str(&content[..start]);
            out.push_str(content[end..].trim_start_matches('\n'));
            fs::write(&profile, out)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

fn home_arg(args: &[String]) -> Result<PathBuf> {
    if let Some(value) = arg_value(args, "--home") {
        return Ok(PathBuf::from(value));
    }
    home_dir()
}

fn home_dir() -> Result<PathBuf> {
    let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(key)
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("{key} is not set"))
}

pub(crate) fn skill_path(home: &Path, spec: &AgentSpec) -> PathBuf {
    home.join(spec.path).join("cm-memory").join("SKILL.md")
}

fn manifest_path(home: &Path) -> PathBuf {
    home.join(".cm").join("install.json")
}

fn backup_dir(home: &Path) -> PathBuf {
    home.join(".cm").join("backups")
}

fn backup_path(home: &Path, name: &str) -> Result<PathBuf> {
    let dir = backup_dir(home);
    fs::create_dir_all(&dir)?;
    let stamp = crate::model::now_ms();
    Ok(dir.join(format!("{name}-{stamp}.bak")))
}

fn prepare_backup(home: &Path, path: &Path, expected: &[u8], name: &str) -> Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }
    if fs::read(path).ok().as_deref() == Some(expected) {
        return Ok(None);
    }
    let backup = backup_path(home, name)?;
    fs::copy(path, &backup)?;
    Ok(Some(backup.display().to_string()))
}

pub(crate) fn load_manifest(home: &Path) -> Result<Manifest> {
    let path = manifest_path(home);
    if !path.exists() {
        return Ok(Manifest {
            version: MANIFEST_VERSION,
            ..Manifest::default()
        });
    }
    let bytes = fs::read(&path)?;
    let manifest: Manifest = serde_json::from_slice(&bytes).context("parse install manifest")?;
    Ok(manifest)
}

fn save_manifest(home: &Path, manifest: &Manifest) -> Result<()> {
    let path = manifest_path(home);
    if manifest.skills.is_empty() && manifest.exe.is_none() {
        if path.exists() {
            fs::remove_file(&path).ok();
        }
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(manifest)?)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

fn upsert_skill(manifest: &mut Manifest, record: SkillRecord) {
    if let Some(existing) = manifest
        .skills
        .iter_mut()
        .find(|item| item.agent == record.agent)
    {
        *existing = record;
    } else {
        manifest.skills.push(record);
    }
}

fn remove_skill(path: &Path, backup: Option<&str>, expected: &[u8]) -> Result<()> {
    if path.exists() {
        let current = fs::read(path)?;
        if current != expected {
            bail!("refusing to remove modified skill {}", path.display());
        }
        fs::remove_file(path)?;
    }
    if let Some(backup) = backup {
        let backup = PathBuf::from(backup);
        if backup.exists() {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&backup, path)?;
            fs::remove_file(backup).ok();
        }
    }
    if let Some(parent) = path.parent() {
        fs::remove_dir(parent).ok();
    }
    Ok(())
}

fn same_path(left: &Path, right: &Path) -> bool {
    let (Ok(left), Ok(right)) = (fs::canonicalize(left), fs::canonicalize(right)) else {
        return false;
    };
    #[cfg(windows)]
    {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

fn hash_file(path: &Path) -> Result<String> {
    Ok(format!("{:016x}", xxh3_64(&fs::read(path)?)))
}

fn values(args: &[String], flag: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut index = 0;
    while index + 1 < args.len() {
        if args[index] == flag {
            out.push(args[index + 1].clone());
            index += 2;
        } else {
            index += 1;
        }
    }
    out
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    values(args, flag).into_iter().next()
}

fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|arg| arg == flag)
}
