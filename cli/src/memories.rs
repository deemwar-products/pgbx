//! `pgbx memories export|import`: move the agent's per-database memory between machines.
//! Memory lives in plain Markdown files, one folder per database:
//!   ${PGBX_MEMORY_DIR:-~/pgbx}/<connection>/<db>/memories.md and tables.md
//! `<connection>` is the profile name (or the host without a profile). An export is one JSON bundle per
//! connection: {"pgbx_memories": 1, "connection", "exported_at", "files": {"<db>/memories.md": "...", ...}}.
//! Import never overwrites a file that differs unless --overwrite; the files belong to the user.

use crate::Args;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

const FILES: [&str; 2] = ["memories.md", "tables.md"];

type Env<'a> = &'a dyn Fn(&str) -> Option<String>;
/// (database, file name, text)
type Entry = (String, String, String);
/// (written, unchanged, conflicts), as "<db>/<file>"
type Applied = (Vec<String>, Vec<String>, Vec<String>);

pub fn root(env: Env) -> Result<PathBuf, String> {
    if let Some(d) = env("PGBX_MEMORY_DIR") {
        return Ok(PathBuf::from(d));
    }
    let home = env("HOME").or_else(|| env("USERPROFILE")).ok_or("HOME (or USERPROFILE) is not set")?;
    Ok(PathBuf::from(home).join("pgbx"))
}

/// A connection or database name used as a folder: no path separators, no "..".
fn check_part(kind: &str, s: &str) -> Result<(), String> {
    if s.is_empty() || s == "." || s == ".." || s.contains(['/', '\\', '\0']) {
        return Err(format!("{kind} '{s}' cannot be used as a folder name"));
    }
    Ok(())
}

/// Every "<db>/<file>" under root/connection (only memories.md and tables.md), sorted.
pub fn collect(dir: &Path, only_db: Option<&str>) -> Result<Map<String, Value>, String> {
    let mut out = Map::new();
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return Ok(out),
    };
    let mut dbs: Vec<String> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|d| only_db.is_none_or(|o| o == d))
        .collect();
    dbs.sort();
    for db in dbs {
        for f in FILES {
            let p = dir.join(&db).join(f);
            if p.is_file() {
                let text = std::fs::read_to_string(&p).map_err(|e| format!("read {}: {e}", p.display()))?;
                out.insert(format!("{db}/{f}"), Value::String(text));
            }
        }
    }
    Ok(out)
}

pub fn bundle(connection: &str, files: Map<String, Value>, now: &str) -> Value {
    json!({"pgbx_memories": 1, "connection": connection, "exported_at": now, "files": files})
}

/// Validate a bundle and return (connection, [(db, file, text)]).
pub fn parse_bundle(v: &Value) -> Result<(String, Vec<Entry>), String> {
    if v["pgbx_memories"].as_i64() != Some(1) {
        return Err("not a pgbx memories export (missing \"pgbx_memories\": 1)".into());
    }
    let conn = v["connection"].as_str().ok_or("export has no connection")?.to_string();
    let files = v["files"].as_object().ok_or("export has no files")?;
    let mut out = vec![];
    for (k, t) in files {
        let (db, f) = k.split_once('/').ok_or_else(|| format!("bad entry '{k}' (want <db>/<file>)"))?;
        check_part("database", db)?;
        if !FILES.contains(&f) {
            return Err(format!("bad entry '{k}': only memories.md and tables.md are imported"));
        }
        let text = t.as_str().ok_or_else(|| format!("entry '{k}' is not text"))?;
        out.push((db.to_string(), f.to_string(), text.to_string()));
    }
    Ok((conn, out))
}

/// Write the files under dir; returns (written, unchanged, conflicts). Without overwrite a differing file is
/// left alone and reported as a conflict.
pub fn apply(dir: &Path, entries: &[Entry], overwrite: bool) -> Result<Applied, String> {
    let (mut written, mut same, mut conflicts) = (vec![], vec![], vec![]);
    for (db, f, text) in entries {
        let p = dir.join(db).join(f);
        let key = format!("{db}/{f}");
        match std::fs::read_to_string(&p) {
            Ok(old) if old == *text => same.push(key),
            Ok(_) if !overwrite => conflicts.push(key),
            _ => {
                std::fs::create_dir_all(dir.join(db)).map_err(|e| format!("create {}: {e}", dir.join(db).display()))?;
                std::fs::write(&p, text).map_err(|e| format!("write {}: {e}", p.display()))?;
                written.push(key);
            }
        }
    }
    Ok((written, same, conflicts))
}

fn now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// `connection` = the profile in use, else the host, else "localhost".
pub fn run(a: &Args, profile: Option<String>, env: Env) -> Result<Value, String> {
    let sub = a.pos.first().map(String::as_str).unwrap_or("");
    let root = root(env)?;
    match sub {
        "export" => {
            let conn = profile.or_else(|| a.get("host").map(String::from)).unwrap_or_else(|| "localhost".into());
            check_part("connection", &conn)?;
            let dir = root.join(&conn);
            let files = collect(&dir, a.get("db"))?;
            if files.is_empty() {
                return Err(format!("no memories under {} (nothing to export)", dir.display()));
            }
            let n = files.len();
            let dbs: std::collections::BTreeSet<String> = files.keys().filter_map(|k| k.split_once('/').map(|x| x.0.to_string())).collect();
            let b = bundle(&conn, files, &now());
            let out = a.pos.get(1).cloned().unwrap_or_else(|| format!("pgbx-memories-{conn}.json"));
            let text = serde_json::to_string_pretty(&b).map_err(|e| e.to_string())? + "\n";
            if out == "-" {
                print!("{text}");
            } else {
                std::fs::write(&out, text).map_err(|e| format!("write {out}: {e}"))?;
            }
            Ok(json!({"ok": true, "connection": conn, "file": out, "databases": dbs, "files": n, "from": dir.display().to_string()}))
        }
        "import" => {
            let file = a.pos.get(1).ok_or("usage: pgbx memories import FILE [--as CONNECTION] [--overwrite]")?;
            let text = if file == "-" {
                std::io::read_to_string(std::io::stdin()).map_err(|e| e.to_string())?
            } else {
                std::fs::read_to_string(file).map_err(|e| format!("read {file}: {e}"))?
            };
            let v: Value = serde_json::from_str(&text).map_err(|e| format!("{file}: not JSON: {e}"))?;
            let (from, entries) = parse_bundle(&v)?;
            let conn = a.get("as").map(String::from).unwrap_or(from.clone());
            check_part("connection", &conn)?;
            let dir = root.join(&conn);
            let (written, unchanged, conflicts) = apply(&dir, &entries, a.has("overwrite"))?;
            Ok(json!({
                "ok": conflicts.is_empty(), "connection": conn, "exported_from": from, "into": dir.display().to_string(),
                "written": written, "unchanged": unchanged, "conflicts": conflicts,
                "hint": if conflicts.is_empty() { Value::Null } else { json!("these files differ locally and were kept; re-run with --overwrite to replace them") },
            }))
        }
        "" | "path" => {
            let conn = profile.or_else(|| a.get("host").map(String::from)).unwrap_or_else(|| "localhost".into());
            Ok(json!({"root": root.display().to_string(), "connection": conn, "dir": root.join(&conn).display().to_string()}))
        }
        x => Err(format!("unknown 'memories {x}' (pgbx memories export [FILE|-] | import FILE [--as C] [--overwrite] | path)")),
    }
}

pub fn run_sys(a: &Args, profile: Option<String>) -> Result<Value, String> {
    run(a, profile, &|k| std::env::var(k).ok().filter(|s| !s.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pgbx-mem-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn root_resolution() {
        let e = |k: &str| match k { "HOME" => Some("/home/u".to_string()), _ => None };
        assert_eq!(root(&e).unwrap(), PathBuf::from("/home/u/pgbx"));
        let e2 = |k: &str| match k { "PGBX_MEMORY_DIR" => Some("/m".to_string()), "HOME" => Some("/home/u".to_string()), _ => None };
        assert_eq!(root(&e2).unwrap(), PathBuf::from("/m"));
    }

    #[test]
    fn export_import_round_trip_and_conflicts() {
        let src = tmp("src");
        std::fs::create_dir_all(src.join("shop")).unwrap();
        std::fs::create_dir_all(src.join("crm")).unwrap();
        std::fs::write(src.join("shop/memories.md"), "## orders today\nSELECT 1\n").unwrap();
        std::fs::write(src.join("shop/tables.md"), "## public.orders\n").unwrap();
        std::fs::write(src.join("crm/memories.md"), "note\n").unwrap();
        std::fs::write(src.join("shop/other.txt"), "ignored").unwrap();
        let files = collect(&src, None).unwrap();
        assert_eq!(files.keys().cloned().collect::<Vec<_>>(), ["crm/memories.md", "shop/memories.md", "shop/tables.md"]);
        assert_eq!(collect(&src, Some("shop")).unwrap().len(), 2);

        let b = bundle("prod", files, "2026-10-02T00:00:00Z");
        let (conn, entries) = parse_bundle(&b).unwrap();
        assert_eq!(conn, "prod");
        let dst = tmp("dst");
        let (w, s, c) = apply(&dst, &entries, false).unwrap();
        assert_eq!((w.len(), s.len(), c.len()), (3, 0, 0));
        // again: unchanged
        let (w, s, c) = apply(&dst, &entries, false).unwrap();
        assert_eq!((w.len(), s.len(), c.len()), (0, 3, 0));
        // local edit: kept and reported, unless overwrite
        std::fs::write(dst.join("crm/memories.md"), "my own edit\n").unwrap();
        let (_, _, c) = apply(&dst, &entries, false).unwrap();
        assert_eq!(c, ["crm/memories.md"]);
        assert_eq!(std::fs::read_to_string(dst.join("crm/memories.md")).unwrap(), "my own edit\n");
        let (w, _, _) = apply(&dst, &entries, true).unwrap();
        assert_eq!(w, ["crm/memories.md"]);
        assert_eq!(std::fs::read_to_string(dst.join("crm/memories.md")).unwrap(), "note\n");
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
    }

    #[test]
    fn bundle_rejects_bad_input() {
        assert!(parse_bundle(&json!({"files": {}})).is_err());
        assert!(parse_bundle(&json!({"pgbx_memories": 1, "connection": "p", "files": {"../x/memories.md": "a"}})).is_err());
        assert!(parse_bundle(&json!({"pgbx_memories": 1, "connection": "p", "files": {"shop/secrets.txt": "a"}})).is_err());
        assert!(parse_bundle(&json!({"pgbx_memories": 1, "connection": "p", "files": {"shop/memories.md": 5}})).is_err());
        assert!(check_part("connection", "a/b").is_err());
        assert!(check_part("connection", "..").is_err());
    }
}
