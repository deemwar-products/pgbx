//! The agent skill (skills/pgbx-skill) ships inside the binary; `pgbx skill install` unpacks it.
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

include!(concat!(env!("OUT_DIR"), "/skill_files.rs"));

pub const NAME: &str = "pgbx-skill";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const MARKER: &str = ".pgbx-installed";

#[derive(Debug, PartialEq)]
pub struct Paths {
    pub data_root: PathBuf,       // ~/.local/share/pgbx/skill
    pub claude: PathBuf,          // ~/.claude/skills/pgbx-skill
    pub agents: Option<PathBuf>,  // ~/.agents/skills/pgbx-skill (codex)
}

impl Paths {
    pub fn version_dir(&self) -> PathBuf {
        self.data_root.join(VERSION)
    }
}

/// Same rule as apl-skill's install.sh: Claude always; agents dir when $AGENTS_SKILLS_DIR is set or codex is on PATH.
pub fn resolve(env: &dyn Fn(&str) -> Option<String>, codex_on_path: bool, no_codex: bool) -> Result<Paths, String> {
    // Windows: %USERPROFILE% when HOME is unset; skill data under %LOCALAPPDATA%
    let home = env("HOME").or_else(|| env("USERPROFILE")).filter(|h| !h.is_empty()).ok_or("HOME (or USERPROFILE) is not set")?;
    let data = env("XDG_DATA_HOME").or_else(|| env("LOCALAPPDATA")).filter(|s| !s.is_empty()).unwrap_or(format!("{home}/.local/share"));
    let claude = env("CLAUDE_SKILLS_DIR").filter(|s| !s.is_empty()).unwrap_or(format!("{home}/.claude/skills"));
    let agents_env = env("AGENTS_SKILLS_DIR").filter(|s| !s.is_empty());
    let agents = if no_codex {
        None
    } else if let Some(a) = agents_env {
        Some(PathBuf::from(a).join(NAME))
    } else if codex_on_path {
        Some(PathBuf::from(format!("{home}/.agents/skills")).join(NAME))
    } else {
        None
    };
    Ok(Paths { data_root: PathBuf::from(data).join("pgbx/skill"), claude: PathBuf::from(claude).join(NAME), agents })
}

pub fn system_paths(no_codex: bool) -> Result<Paths, String> {
    let codex = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| ["codex", "codex.exe", "codex.cmd"].iter().any(|n| d.join(n).is_file())))
        .unwrap_or(false);
    resolve(&|k| std::env::var(k).ok(), codex, no_codex)
}

/// `<!-- version: X -->` from a SKILL.md
pub fn skill_md_version(s: &str) -> Option<String> {
    let i = s.find("<!-- version:")? + "<!-- version:".len();
    let rest = &s[i..];
    Some(rest[..rest.find("-->")?].trim().to_string())
}

pub fn embedded_skill_md() -> &'static str {
    SKILL_FILES.iter().find(|f| f.0 == "SKILL.md").map(|f| std::str::from_utf8(f.1).unwrap_or("")).unwrap_or("")
}

/// A directory pgbx wrote as a COPY of the skill (Windows without symlink rights): it carries the marker.
fn is_our_copy(d: &Path) -> bool {
    fs::symlink_metadata(d).map(|m| m.is_dir()).unwrap_or(false) && d.join(MARKER).exists()
}

fn remove_link(d: &Path) -> std::io::Result<()> {
    // a Windows directory symlink is removed with remove_dir; a unix symlink with remove_file
    fs::remove_file(d).or_else(|_| fs::remove_dir(d))
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for e in fs::read_dir(from)? {
        let e = e?;
        let t = to.join(e.file_name());
        if e.file_type()?.is_dir() { copy_dir(&e.path(), &t)? } else { fs::copy(e.path(), &t).map(|_| ())? }
    }
    Ok(())
}

#[cfg(unix)]
fn symlink_dir(target: &Path, dest: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, dest)
}
#[cfg(windows)]
fn symlink_dir(target: &Path, dest: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(target, dest)
}
#[cfg(not(any(unix, windows)))]
fn symlink_dir(_: &Path, _: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other("no symlinks on this platform"))
}

fn link(dest: &Path, target: &Path) -> Result<String, String> {
    if let Some(p) = dest.parent() {
        fs::create_dir_all(p).map_err(|e| format!("{}: {e}", p.display()))?;
    }
    match fs::symlink_metadata(dest) {
        Ok(m) if m.file_type().is_symlink() => remove_link(dest).map_err(|e| format!("{}: {e}", dest.display()))?,
        Ok(_) if is_our_copy(dest) => fs::remove_dir_all(dest).map_err(|e| format!("{}: {e}", dest.display()))?,
        Ok(_) => {
            return Err(format!(
                "{} already exists and is a real directory pgbx did not create; refusing to clobber it. \
                 Remove it yourself (rm -rf '{}') and re-run `pgbx skill install`",
                dest.display(), dest.display()))
        }
        Err(_) => {}
    }
    // Windows needs Developer Mode (or admin) for symlinks: fall back to a marked copy
    if symlink_dir(target, dest).is_err() {
        copy_dir(target, dest).map_err(|e| format!("copy skill to {}: {e}", dest.display()))?;
        return Ok(format!("{} (copy)", dest.display()));
    }
    Ok(dest.display().to_string())
}

pub fn install(p: &Paths) -> Result<Value, String> {
    // refuse BEFORE writing anything if a link location is a real directory
    for d in std::iter::once(&p.claude).chain(p.agents.iter()) {
        if let Ok(m) = fs::symlink_metadata(d) {
            if !m.file_type().is_symlink() && !is_our_copy(d) {
                return Err(format!(
                    "{} already exists and is a real directory pgbx did not create; refusing to clobber it. \
                     Remove it yourself (rm -rf '{}') and re-run `pgbx skill install`", d.display(), d.display()));
            }
        }
    }
    let vd = p.version_dir();
    if vd.exists() {
        if !vd.join(MARKER).exists() {
            return Err(format!("{} exists but was not created by pgbx; remove it and re-run", vd.display()));
        }
        fs::remove_dir_all(&vd).map_err(|e| format!("{}: {e}", vd.display()))?;
    }
    #[allow(unused_variables)]
    for (rel, bytes, exec) in SKILL_FILES {
        let f = vd.join(rel);
        fs::create_dir_all(f.parent().unwrap()).map_err(|e| e.to_string())?;
        fs::write(&f, bytes).map_err(|e| format!("{}: {e}", f.display()))?;
        #[cfg(unix)]
        if *exec {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&f, fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
        }
    }
    fs::write(vd.join(MARKER), VERSION).map_err(|e| e.to_string())?;
    let mut links = vec![link(&p.claude, &vd)?];
    if let Some(a) = &p.agents {
        links.push(link(a, &vd)?);
    }
    Ok(json!({"ok": true, "version": VERSION, "installed_to": vd.display().to_string(), "files": SKILL_FILES.len(),
              "links": links, "next": "restart your agent session to pick up the skill"}))
}

pub fn uninstall(p: &Paths) -> Result<Value, String> {
    let mut removed = vec![];
    let mut skipped = vec![];
    for d in std::iter::once(&p.claude).chain(p.agents.iter()) {
        if let Ok(m) = fs::symlink_metadata(d) {
            let ours = m.file_type().is_symlink() && fs::read_link(d).map(|t| t.starts_with(&p.data_root)).unwrap_or(false);
            if is_our_copy(d) {
                fs::remove_dir_all(d).map_err(|e| e.to_string())?;
                removed.push(d.display().to_string());
            } else if ours {
                remove_link(d).map_err(|e| e.to_string())?;
                removed.push(d.display().to_string());
            } else {
                skipped.push(format!("{} (not created by pgbx; left alone)", d.display()));
            }
        }
    }
    if let Ok(rd) = fs::read_dir(&p.data_root) {
        for e in rd.flatten() {
            if e.path().join(MARKER).exists() {
                fs::remove_dir_all(e.path()).map_err(|e| e.to_string())?;
                removed.push(e.path().display().to_string());
            }
        }
        let _ = fs::remove_dir(&p.data_root); // only if now empty
    }
    Ok(json!({"ok": true, "removed": removed, "skipped": skipped}))
}

/// Installed version as seen through the Claude link (None = not installed).
pub fn installed_version(p: &Paths) -> Option<String> {
    skill_md_version(&fs::read_to_string(p.claude.join("SKILL.md")).ok()?)
}

pub fn where_(p: &Paths) -> Value {
    let inst = installed_version(p);
    json!({
        "ok": true, "binary_version": VERSION, "embedded_skill_version": skill_md_version(embedded_skill_md()), "installed_version": inst,
        "same_version": inst.as_deref() == Some(VERSION),
        "data_dir": p.version_dir().display().to_string(),
        "claude_link": p.claude.display().to_string(), "claude_link_target": fs::read_link(&p.claude).ok().map(|t| t.display().to_string()),
        "agents_link": p.agents.as_ref().map(|a| a.display().to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pgbx-skill-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }
    fn paths(root: &Path, agents: bool) -> Paths {
        let r = root.display().to_string();
        resolve(&|k| match k {
            "HOME" => Some(format!("{r}/home")),
            "CLAUDE_SKILLS_DIR" => Some(format!("{r}/claude")),
            "AGENTS_SKILLS_DIR" if agents => Some(format!("{r}/agents")),
            _ => None,
        }, false, false).unwrap()
    }

    #[test]
    fn version_matches_skill_md() {
        assert_eq!(skill_md_version(embedded_skill_md()).as_deref(), Some(VERSION), "SKILL.md <!-- version --> must equal the cli crate version");
    }

    #[test]
    fn resolves_paths_from_env() {
        let p = resolve(&|k| (k == "HOME").then(|| "/h".to_string()), false, false).unwrap();
        assert_eq!(p.claude, PathBuf::from("/h/.claude/skills/pgbx-skill"));
        assert_eq!(p.data_root, PathBuf::from("/h/.local/share/pgbx/skill"));
        assert_eq!(p.agents, None);
        let p = resolve(&|k| (k == "HOME").then(|| "/h".to_string()), true, false).unwrap();
        assert_eq!(p.agents, Some(PathBuf::from("/h/.agents/skills/pgbx-skill")));
        assert_eq!(resolve(&|k| (k == "HOME").then(|| "/h".to_string()), true, true).unwrap().agents, None);
    }

    #[test]
    fn install_idempotent_and_uninstall_only_ours() {
        let root = tmp("idem");
        let p = paths(&root, true);
        install(&p).unwrap();
        install(&p).unwrap(); // re-install replaces our links
        assert!(fs::symlink_metadata(&p.claude).unwrap().file_type().is_symlink() || is_our_copy(&p.claude));
        assert!(p.agents.as_ref().unwrap().join("SKILL.md").exists());
        assert_eq!(installed_version(&p).as_deref(), Some(VERSION));
        // a foreign link elsewhere is untouched by uninstall
        let foreign = root.join("claude/other-skill");
        symlink_dir(&root, &foreign).or_else(|_| fs::create_dir_all(&foreign)).unwrap();
        let r = uninstall(&p).unwrap();
        assert!(!p.claude.exists() && !p.agents.as_ref().unwrap().exists() && !p.version_dir().exists());
        assert!(fs::symlink_metadata(&foreign).is_ok());
        assert!(r["removed"].as_array().unwrap().len() >= 3);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn marked_copy_is_replaced_and_uninstalled() {
        let root = tmp("copy");
        let p = paths(&root, false);
        install(&p).unwrap();
        // simulate the no-symlink fallback: replace the link with a marked copy
        remove_link(&p.claude).unwrap();
        copy_dir(&p.version_dir(), &p.claude).unwrap();
        assert!(is_our_copy(&p.claude));
        install(&p).unwrap(); // a marked copy is ours: replaced, not refused
        remove_link(&p.claude).ok();
        let _ = fs::remove_dir_all(&p.claude);
        copy_dir(&p.version_dir(), &p.claude).unwrap();
        uninstall(&p).unwrap();
        assert!(!p.claude.exists(), "uninstall removes a marked copy");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn refuses_real_directory() {
        let root = tmp("real");
        let p = paths(&root, false);
        fs::create_dir_all(&p.claude).unwrap();
        fs::write(p.claude.join("mine.txt"), "x").unwrap();
        let e = install(&p).unwrap_err();
        assert!(e.contains("refusing to clobber") && e.contains("rm -rf"));
        assert!(p.claude.join("mine.txt").exists());
        assert!(!p.version_dir().exists(), "nothing written on refusal");
        uninstall(&p).unwrap();
        assert!(p.claude.join("mine.txt").exists(), "uninstall leaves a foreign dir alone");
        let _ = fs::remove_dir_all(&root);
    }
}
