//! `pgbx setup client`: set up the pgbx CLI on the user's own machine (no sudo). Asks for (or takes flags for) a
//! profile name and how to reach the server: a connection string, or an adapter (ssh, aws, gcp, azure or your own)
//! plus that adapter's settings, and optional S3 locations for `--from-s3` restores. Secrets are `$VAR` references,
//! never values. Writes the profile (default if it is the first) through `pgbx profile add`, then tests it:
//! connect (starting the adapter if there is one), the pgbx extension's version and a `pgbx.status()` summary,
//! with the next step for each failure. A server without the extension is fine: pgbx then is a plain client
//! (profiles, adapters, `pgbx query`, agent memory) and backups are reported as off, with turning them on as an
//! optional step. Last, offers `pgbx skill install`.
//! Non-interactive (no TTY, or --json): flags only, and --yes to write.

use crate::{config, one, profile, Args, Ctx, Out};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

/// The settings the shipped example adapters read, asked in this order: (key, question, default).
/// (key, question, default)
type Question = (&'static str, &'static str, &'static str);
const EXAMPLE_KEYS: &[(&str, &[Question])] = &[
    ("ssh", &[
        ("target", "SSH target (user@host or a ~/.ssh/config Host)", ""),
        ("pg_host", "Postgres host as seen from the SSH host", "localhost"),
        ("pg_port", "Postgres port there", "5432"),
    ]),
    ("aws", &[
        ("target", "SSM-managed instance id that can reach the database (i-...)", ""),
        ("db_host", "Database host as seen from that instance (the RDS endpoint)", "localhost"),
        ("region", "AWS region (empty = your default)", ""),
        ("aws_profile", "AWS CLI profile (empty = your default)", ""),
        ("iam_auth", "RDS IAM auth token as the password? (true/empty)", ""),
    ]),
    ("gcp", &[
        ("instance", "Cloud SQL instance (project:region:instance)", ""),
        ("iam_auth", "IAM database auth? (true/empty)", ""),
    ]),
    ("azure", &[
        ("host", "Flexible server host (empty when going through Bastion)", ""),
        ("entra_auth", "Entra ID token as the password? (true/empty)", ""),
    ]),
];
/// Asked for every adapter after its own keys.
const COMMON_KEYS: &[(&str, &str, &str)] = &[
    ("user", "Database user", "postgres"),
    ("password", "Password: a reference like $PGPASSWORD, never the value (empty = none / ~/.pgpass)", ""),
    ("dbname", "Database", "postgres"),
];
const S3_FIRST: (&str, &str, &str) = ("s3-endpoint", "S3 endpoint for restores from S3 (optional)", "");
const S3_MORE: &[(&str, &str, &str)] = &[
    ("s3-bucket", "S3 bucket", ""),
    ("s3-region", "S3 region", "us-east-1"),
    ("server-name", "Server name (the server's folder in the bucket)", ""),
    ("credentials-file", "S3 credentials file path (keys stay in that file)", ""),
];
/// Flags handed to `pgbx profile add` as they are.
const PASS_THROUGH: &[&str] = &["url", "adapter", "adapter-command", "host", "port", "user", "admin-db",
    "s3-endpoint", "s3-bucket", "s3-region", "server-name", "credentials-file"];

pub type Ask<'a> = &'a mut dyn FnMut(&str, &str) -> Result<String, String>;

/// The `pgbx profile add` arguments for this setup: (name, flags, key=value settings).
#[derive(Debug, Default)]
pub struct Plan {
    pub name: String,
    pub flags: HashMap<String, String>,
    pub settings: Vec<String>,
}

/// Collect the profile from flags, asking for the rest when `ask` is given.
pub fn gather(a: &Args, mut ask: Option<Ask>, configured: &[String]) -> Result<Plan, String> {
    let name = match (a.pos.get(1), ask.as_mut()) {
        (Some(n), _) => n.clone(),
        (None, Some(q)) => q("Profile name", "default")?,
        (None, None) => return Err("pgbx setup client NAME --url URL | --adapter A [key=value ...] | --host H [--port P --user U] --yes".into()),
    };
    profile::check_name(&name)?;
    let mut p = Plan { name, ..Default::default() };
    for k in PASS_THROUGH {
        if let Some(v) = a.flags.get(*k) {
            p.flags.insert(k.to_string(), v.clone());
        }
    }
    p.settings = a.pos.iter().skip(2).cloned().collect();
    let given = ["url", "adapter", "host", "port"].iter().any(|k| p.flags.contains_key(*k));
    let Some(ask) = ask.filter(|_| !given) else {
        if !given {
            return Err("say how to reach the server: --url 'postgres://user:$PGPASSWORD@host:5432/db', --adapter NAME key=value ..., or --host H".into());
        }
        return Ok(p);
    };
    let known = if configured.is_empty() { "none yet".to_string() } else { configured.join(", ") };
    let how = ask(&format!("Connection string (postgres://user:$PGPASSWORD@host:5432/db) or adapter name (configured: {known}; examples: ssh, aws, gcp, azure)"), "")?;
    let how = how.trim().to_string();
    if how.is_empty() {
        return Err("no connection given: a connection string or an adapter name".into());
    }
    if how.contains("://") || how.contains('=') {
        p.flags.insert("url".into(), how);
    } else {
        let mut put = |k: &str, v: String| {
            if !v.trim().is_empty() {
                p.settings.push(format!("{k}={}", v.trim()));
            }
        };
        if !configured.contains(&how) {
            let js = config::adapters_dir(&config::sys_env).map(|d| d.join(&how).join(format!("{how}-adapter.js")));
            if !js.as_ref().is_ok_and(|j| j.is_file()) {
                let c = ask(&format!("Command that runs adapter '{how}' (e.g. node /path/to/pgbx/adapters/{how}/{how}-adapter.js)"), "")?;
                if c.trim().is_empty() {
                    return Err(format!("adapter '{how}' has no command"));
                }
                p.flags.insert("adapter-command".into(), c.trim().to_string());
            }
        }
        let own = EXAMPLE_KEYS.iter().find(|(n, _)| *n == how).map(|(_, k)| *k).unwrap_or(&[]);
        for (k, q, d) in own.iter().chain(COMMON_KEYS) {
            put(k, ask(q, d)?);
        }
        loop {
            let kv = ask("More adapter settings as key=value (empty to finish)", "")?;
            if kv.trim().is_empty() {
                break;
            }
            if !kv.contains('=') {
                return Err(format!("'{kv}': settings are key=value"));
            }
            p.settings.push(kv.trim().to_string());
        }
        p.flags.insert("adapter".into(), how);
    }
    let s3 = ask(S3_FIRST.1, S3_FIRST.2)?;
    if !s3.trim().is_empty() {
        p.flags.insert(S3_FIRST.0.into(), s3.trim().into());
        for (k, q, d) in S3_MORE {
            let v = ask(q, d)?;
            if !v.trim().is_empty() {
                p.flags.insert(k.to_string(), v.trim().into());
            }
        }
    }
    Ok(p)
}

/// What to do next, given the test results. Steps for an extension that is simply absent start with "optional".
pub fn next_steps(t: &Value, via_adapter: bool) -> Vec<String> {
    let mut s = vec![];
    if let Some(e) = t["connect_error"].as_str() {
        if e.contains("password") || e.contains("authentication") {
            s.push("Postgres wants a password: reference it as $PGPASSWORD in the url (or the adapter's password setting) and export it, \
                    put it in your secrets: source, or use ~/.pgpass (host:port:*:user:password, chmod 600); never store the value".into());
        } else if e.contains(" is not set") {
            s.push("a $VAR in the profile is not set: export it, or add it to the secrets: source in config.yaml (pgbx profile show <name>)".into());
        } else if via_adapter && e.contains("adapter") {
            s.push("the adapter failed (its message is above): check its settings with `pgbx profile show <name>` and that its own tool \
                    works by hand (for ssh: `ssh <target> true` without a prompt)".into());
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

fn test(cx: &Ctx, db: &str) -> Value {
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
    let configured: Vec<String> = profile::load_sys().map(|c| c.adapters.keys().cloned().collect()).unwrap_or_default();
    let plan = gather(&cx.a, if interactive { Some(&mut prompt) } else { None }, &configured)?;
    if !interactive && !cx.a.has("yes") {
        return Ok(json!({"ok": false, "error": "pgbx setup client writes a profile: re-run with --yes (or in a terminal)",
            "plan": {"profile": plan.name, "settings": plan.flags, "adapter_settings": plan.settings}}));
    }
    let mut pos = vec!["add".to_string(), plan.name.clone()];
    pos.extend(plan.settings.iter().cloned());
    let add = Args { cmd: "profile".into(), pos, flags: plan.flags.clone() };
    let saved = profile::run_sys(&add)?;
    let cfg = profile::load_sys()?;
    let (pa, conn) = profile::args_for(&plan.name, &cfg)?;
    let conn = Arc::new(conn);
    let tcx = Ctx::new(pa, Arc::clone(&conn));
    let db = cx.a.get("db").map(String::from).or_else(|| conn.default_db()).unwrap_or("postgres".into());
    let t = test(&tcx, &db);
    conn.close(); // stops the adapter, if any
    let steps = next_steps(&t, conn.is_adapter());
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
    Ok(json!({"ok": ok, "profile": saved["profile"], "file": saved["file"], "notices": saved.get("notices"), "test": t,
        "next_steps": steps, "skill": skill,
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
        let pl = gather(&p(&["setup", "client", "prod", "--host", "db1", "--user", "ops", "--yes"]), None, &[]).unwrap();
        assert_eq!(pl.name, "prod");
        assert_eq!((pl.flags["host"].as_str(), pl.flags["user"].as_str()), ("db1", "ops"));
        let pl = gather(&p(&["setup", "client", "p", "--adapter", "ssh", "target=ops@db1", "jump=b", "--s3-bucket", "bk"]), None, &[]).unwrap();
        assert_eq!((pl.flags["adapter"].as_str(), pl.flags["s3-bucket"].as_str()), ("ssh", "bk"));
        assert_eq!(pl.settings, ["target=ops@db1", "jump=b"]);
        let pl = gather(&p(&["setup", "client", "u", "--url", "postgres://a:$PGPASSWORD@h/d"]), None, &[]).unwrap();
        assert_eq!(pl.flags["url"], "postgres://a:$PGPASSWORD@h/d");
        assert!(gather(&p(&["setup", "client", "p", "--user", "u"]), None, &[]).unwrap_err().contains("--url"));
        assert!(gather(&p(&["setup", "client"]), None, &[]).is_err());
        assert!(gather(&p(&["setup", "client", "bad/name", "--host", "h"]), None, &[]).is_err());
    }

    #[test]
    fn interactive_adapter_asks_its_keys_and_skips_s3_when_empty() {
        let mut asked = vec![];
        let mut ask = |q: &str, d: &str| -> Result<String, String> {
            asked.push(q.to_string());
            Ok(match q {
                "Profile name" => "home".into(),
                q if q.starts_with("Connection string") => "ssh".into(),
                q if q.starts_with("SSH target") => "me@box".into(),
                q if q.starts_with("Password") => "$PGPASSWORD".into(),
                _ => d.to_string(),
            })
        };
        let pl = gather(&p(&["setup", "client"]), Some(&mut ask), &["ssh".into()]).unwrap();
        assert_eq!((pl.name.as_str(), pl.flags["adapter"].as_str()), ("home", "ssh"));
        assert_eq!(pl.settings, ["target=me@box", "pg_host=localhost", "pg_port=5432", "user=postgres", "password=$PGPASSWORD", "dbname=postgres"]);
        assert!(!pl.flags.contains_key("s3-endpoint") && !pl.flags.contains_key("adapter-command"));
        assert!(!asked.iter().any(|q| q.starts_with("S3 bucket")), "{asked:?}");
    }

    #[test]
    fn interactive_url() {
        let mut ask = |q: &str, d: &str| -> Result<String, String> {
            Ok(if q.starts_with("Connection string") { "postgres://app:$PGPASSWORD@db:5432/shop".into() } else { d.to_string() })
        };
        let pl = gather(&p(&["setup", "client", "dev"]), Some(&mut ask), &[]).unwrap();
        assert_eq!(pl.flags["url"], "postgres://app:$PGPASSWORD@db:5432/shop");
        assert!(pl.settings.is_empty());
    }

    #[test]
    fn steps_for_each_failure() {
        assert!(next_steps(&json!({"connect_error": "profile 'p' (adapter ssh): adapter state: error: x"}), true)[0].contains("adapter failed"));
        assert!(next_steps(&json!({"connect_error": "password authentication failed"}), false)[0].contains("$PGPASSWORD"));
        assert!(next_steps(&json!({"connect_error": "profile 'p': $PGPASSWORD is not set (looked in the environment)"}), false)[0].contains("not set"));
        assert!(next_steps(&json!({"connect_error": "connection refused"}), false)[0].contains("listen"));
        let s = next_steps(&json!({"connected": true, "extension_version": null, "extension": "absent"}), false);
        assert!(s[0].starts_with("optional") && s[0].contains("install.sh") && s[0].contains("pgbx setup server"));
        let s = next_steps(&json!({"connected": true, "extension_version": null, "extension": "not_in_db"}), false);
        assert!(s[0].contains("not in this database") && s[0].contains("doctor"));
        assert!(next_steps(&json!({"connected": true, "extension_version": "0.5.0", "status": {"state": "active"}}), false).is_empty());
        assert!(next_steps(&json!({"connected": true, "extension_version": "0.5.0", "status": {"state": "failing"}}), false)[0].contains("doctor"));
    }
}
