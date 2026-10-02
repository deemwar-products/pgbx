//! `pgbx setup client`: set up the pgbx CLI on the user's own machine (no sudo). Asks for (or takes flags
//! for) a profile name, how to reach the server (direct host/port, or an SSH target), the database user and
//! optional S3 locations for `--from-s3` restores — never a password or S3 key. Writes the profile (default if
//! it is the first), then tests it: connect (through the tunnel when ssh), the pgbx extension's version and a
//! `pgbx.status()` summary, with the next step for each failure. A server without the extension is fine:
//! pgbx then is a plain client (profiles, tunnels, `pgbx query`, agent memory) and backups are reported as off,
//! with turning them on as an optional step. Last, offers `pgbx skill install`.
//! Non-interactive (no TTY, or --json): flags only, and --yes to write.

use crate::{one, Args, Ctx, Out};
use serde_json::{json, Value};
use std::collections::HashMap;

/// Profile fields this command asks about, in order: (flag, question, default).
const QUESTIONS: &[(&str, &str, &str)] = &[
    ("ssh", "SSH target to reach the server (user@host; empty = connect directly)", ""),
    ("host", "Postgres host", "localhost"),
    ("port", "Postgres port", "5432"),
    ("user", "Database user (password: ~/.pgpass or PGPASSWORD, never stored)", "postgres"),
    ("s3-endpoint", "S3 endpoint for restores from S3 (optional)", ""),
];
const S3_MORE: &[(&str, &str, &str)] = &[
    ("s3-bucket", "S3 bucket", ""),
    ("s3-region", "S3 region", "us-east-1"),
    ("server-name", "Server name (the server's folder in the bucket)", ""),
    ("credentials-file", "S3 credentials file path (keys stay in that file)", ""),
];
const PASS_THROUGH: &[&str] = &["ssh-port", "ssh-jump", "tunnel-idle", "admin-db"];

pub type Ask<'a> = &'a mut dyn FnMut(&str, &str) -> Result<String, String>;

/// Collect the profile name and fields from flags, asking for the rest when `ask` is given.
pub fn gather(a: &Args, mut ask: Option<Ask>) -> Result<(String, HashMap<String, String>), String> {
    let name = match (a.pos.get(1), ask.as_mut()) {
        (Some(n), _) => n.clone(),
        (None, Some(q)) => q("Profile name", "default")?,
        (None, None) => return Err("pgbx setup client NAME --host H | --ssh user@host [--user U] --yes".into()),
    };
    crate::profile::check_name(&name)?;
    let mut f = HashMap::new();
    let put = |f: &mut HashMap<String, String>, k: &str, v: String| {
        if !v.is_empty() {
            f.insert(k.to_string(), v);
        }
    };
    let flags_given = QUESTIONS.iter().chain(S3_MORE).any(|(k, _, _)| a.flags.contains_key(*k));
    for (k, q, d) in QUESTIONS {
        let v = match (a.flags.get(*k), ask.as_mut()) {
            (Some(v), _) => v.clone(),
            (None, Some(ask)) if !flags_given => {
                // over ssh, Postgres is usually on the server's own localhost
                ask(q, d)?
            }
            _ => String::new(),
        };
        put(&mut f, k, v);
    }
    let want_s3 = f.contains_key("s3-endpoint") || S3_MORE.iter().any(|(k, _, _)| a.flags.contains_key(*k));
    for (k, q, d) in S3_MORE {
        let v = match (a.flags.get(*k), ask.as_mut()) {
            (Some(v), _) => v.clone(),
            (None, Some(ask)) if want_s3 && !flags_given => ask(q, d)?,
            _ => String::new(),
        };
        put(&mut f, k, v);
    }
    for k in PASS_THROUGH {
        if let Some(v) = a.flags.get(*k) {
            f.insert(k.to_string(), v.clone());
        }
    }
    if !f.contains_key("host") && !f.contains_key("ssh") {
        return Err("say how to reach the server: --host H (direct) or --ssh user@host".into());
    }
    Ok((name, f))
}

/// What to do next, given the test results. Steps for an extension that is simply absent start with "optional".
pub fn next_steps(t: &Value, via_ssh: bool) -> Vec<String> {
    let mut s = vec![];
    if let Some(e) = t["connect_error"].as_str() {
        if via_ssh && (e.contains("ssh") || e.contains("tunnel")) {
            s.push("ssh failed: check `ssh <target> true` works without a prompt (key in your ssh agent or ~/.ssh/config)".into());
        } else if e.contains("password") || e.contains("authentication") {
            s.push("Postgres wants a password: put it in ~/.pgpass (host:port:*:user:password, chmod 600) or set PGPASSWORD".into());
        } else {
            s.push("cannot reach Postgres: check host/port, that it listens there (listen_addresses) and pg_hba.conf allows you".into());
        }
        return s;
    }
    if t["extension_version"].is_null() {
        s.push(if t["extension"] == "not_in_db" {
            "pgbx is on this server but not in this database yet: wait for the worker's next poll, then run `pgbx doctor` if it stays missing".into()
        } else {
            crate::client_only::turn_on()
        });
        return s;
    }
    if t["status"]["state"].as_str() == Some("failing") {
        s.push("backups are failing: run `pgbx doctor` and `pgbx logs` for the cause".into());
    }
    s
}

fn test(name: &str, fields: &HashMap<String, String>, db: &str) -> Value {
    let mut a = Args { cmd: "status".into(), pos: vec![], flags: fields.clone() };
    a.flags.insert("profile".into(), name.into());
    a.flags.insert("db".into(), db.into());
    let cx = Ctx { a, admin_db: None, tunnel: Default::default() };
    let mut t = json!({"database": db, "connected": false, "extension_version": null, "status": null});
    let mut c = match cx.connect(db) {
        Ok(c) => c,
        Err(e) => {
            t["connect_error"] = json!(e);
            return t;
        }
    };
    t["connected"] = json!(true);
    if let Ok(v) = one(&mut c, "SELECT current_setting('server_version') AS postgres, current_user AS user", &[]) {
        t["postgres_version"] = v["postgres"].clone();
        t["user"] = v["user"].clone();
    }
    if let Ok(v) = one(&mut c, "SELECT extversion FROM pg_extension WHERE extname = 'pgbx'", &[]) {
        t["extension_version"] = v["extversion"].clone();
    }
    match crate::client_only::ext(&mut c) {
        Ok(crate::client_only::Ext::Absent) => {
            t["extension"] = json!("absent");
            t["backups"] = json!("off");
            t["info"] = json!(crate::client_only::OFF);
        }
        Ok(crate::client_only::Ext::NotInDb) => t["extension"] = json!("not_in_db"),
        _ => {}
    }
    if !t["extension_version"].is_null() {
        match one(&mut c, "SELECT state, schedule, last_backup_at, last_backup_age::text AS last_backup_age, backups_kept, last_error FROM pgbx.status()", &[]) {
            Ok(v) => t["status"] = v,
            Err(e) => t["status_error"] = json!(e),
        }
    }
    t
}

pub fn run(cx: &mut Ctx) -> Out {
    let interactive = !cx.a.has("json") && std::io::IsTerminal::is_terminal(&std::io::stdin());
    let mut prompt = |q: &str, d: &str| crate::setup::prompt(q, Some(d));
    let (name, fields) = gather(&cx.a, if interactive { Some(&mut prompt) } else { None })?;
    if !interactive && !cx.a.has("yes") {
        return Ok(json!({"ok": false, "error": "pgbx setup client writes a profile: re-run with --yes (or in a terminal)",
            "plan": {"profile": name, "settings": fields}}));
    }
    let mut add = Args { cmd: "profile".into(), pos: vec!["add".into(), name.clone()], flags: fields.clone() };
    add.flags.remove("db");
    let saved = crate::profile::run_sys(&add)?;
    let db = cx.a.get("db").unwrap_or("postgres").to_string();
    let t = test(&name, &fields, &db);
    let steps = next_steps(&t, fields.contains_key("ssh"));
    // connected is enough: without the extension pgbx is a plain client (backups off), which is a fine way to use it
    let ok = t["connected"] == true && t["extension"] != "not_in_db";

    // agent skill: default yes on a terminal, skipped without one
    let skill = if cx.a.has("no-skill") || !interactive {
        json!({"installed": false, "reason": if interactive { "--no-skill" } else { "no terminal: run `pgbx skill install` yourself" }})
    } else if crate::setup::prompt("Install the pgbx agent skill for Claude Code / Codex? [Y/n]", Some("y"))?.to_lowercase().starts_with('n') {
        json!({"installed": false, "reason": "declined"})
    } else {
        match crate::skill::system_paths(false).and_then(|p| crate::skill::install(&p)) {
            Ok(v) => json!({"installed": true, "result": v}),
            Err(e) => json!({"installed": false, "reason": e}),
        }
    };
    Ok(json!({"ok": ok, "profile": saved["profile"], "file": saved["file"], "test": t, "next_steps": steps, "skill": skill,
        "error": if ok { Value::Null } else { json!(steps.first().cloned().unwrap_or_default()) }}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_args;

    fn p(s: &[&str]) -> Args {
        parse_args(s.iter().map(|x| x.to_string())).unwrap()
    }

    #[test]
    fn flags_only() {
        let (n, f) = gather(&p(&["setup", "client", "prod", "--host", "db1", "--user", "ops", "--yes"]), None).unwrap();
        assert_eq!(n, "prod");
        assert_eq!((f["host"].as_str(), f["user"].as_str()), ("db1", "ops"));
        assert!(!f.contains_key("port") && !f.contains_key("s3-bucket"));
        let (_, f) = gather(&p(&["setup", "client", "p", "--ssh", "ops@db1", "--ssh-jump", "b", "--s3-bucket", "bk"]), None).unwrap();
        assert_eq!((f["ssh"].as_str(), f["ssh-jump"].as_str(), f["s3-bucket"].as_str()), ("ops@db1", "b", "bk"));
        assert!(gather(&p(&["setup", "client", "p", "--user", "u"]), None).unwrap_err().contains("--host"));
        assert!(gather(&p(&["setup", "client"]), None).is_err());
        assert!(gather(&p(&["setup", "client", "bad/name", "--host", "h"]), None).is_err());
    }

    #[test]
    fn interactive_asks_and_skips_s3_when_empty() {
        let mut asked = vec![];
        let mut ask = |q: &str, d: &str| -> Result<String, String> {
            asked.push(q.to_string());
            Ok(match q {
                "Profile name" => "home".into(),
                q if q.starts_with("SSH target") => "me@box".into(),
                _ => d.to_string(),
            })
        };
        let (n, f) = gather(&p(&["setup", "client"]), Some(&mut ask)).unwrap();
        assert_eq!(n, "home");
        assert_eq!((f["ssh"].as_str(), f["host"].as_str(), f["user"].as_str()), ("me@box", "localhost", "postgres"));
        assert!(!f.contains_key("s3-endpoint"));
        assert!(!asked.iter().any(|q| q.starts_with("S3 bucket")), "{asked:?}");
    }

    #[test]
    fn steps_for_each_failure() {
        assert!(next_steps(&json!({"connect_error": "ssh tunnel to x failed"}), true)[0].contains("ssh"));
        assert!(next_steps(&json!({"connect_error": "password authentication failed"}), false)[0].contains(".pgpass"));
        assert!(next_steps(&json!({"connect_error": "connection refused"}), false)[0].contains("listen"));
        let s = next_steps(&json!({"connected": true, "extension_version": null, "extension": "absent"}), false);
        assert!(s[0].starts_with("optional") && s[0].contains("install.sh") && s[0].contains("pgbx setup server"));
        let s = next_steps(&json!({"connected": true, "extension_version": null, "extension": "not_in_db"}), false);
        assert!(s[0].contains("not in this database") && s[0].contains("doctor"));
        assert!(next_steps(&json!({"connected": true, "extension_version": "0.5.0", "status": {"state": "active"}}), false).is_empty());
        assert!(next_steps(&json!({"connected": true, "extension_version": "0.5.0", "status": {"state": "failing"}}), false)[0].contains("doctor"));
    }
}
