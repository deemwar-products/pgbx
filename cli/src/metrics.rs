//! Prometheus metrics (text exposition format 0.0.4), read-only, from the admin database's server_overview
//! (the worker refreshes it every poll). Served at `pgbx ui` GET /metrics and printed by `pgbx metrics`.

use crate::{rows, Ctx, Out};
use serde_json::{json, Value};

pub const SQL: &str = "SELECT database::text, state,
        extract(epoch FROM last_backup_at)::float8 AS last_ts,
        extract(epoch FROM now() - last_backup_at)::float8 AS age,
        last_backup_bytes, backups_kept, failures_total, queued_jobs, last_verify_ok, last_backup_encrypted,
        extract(epoch FROM now() - seen_at)::float8 AS seen_age
    FROM pgbx.server_overview ORDER BY database";

fn esc(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// Render rows of `SQL` as Prometheus text. Pure; unit-tested.
pub fn render(dbs: &[Value]) -> String {
    let mut o = String::new();
    let fams: &[(&str, &str, &str)] = &[
        ("pgbx_last_backup_timestamp_seconds", "last_ts", "Unix time of the newest completed backup."),
        ("pgbx_last_backup_age_seconds", "age", "Seconds since the newest completed backup."),
        ("pgbx_last_backup_size_bytes", "last_backup_bytes", "Size in S3 of the newest backup."),
        ("pgbx_backups_kept", "backups_kept", "Completed backups recorded for the database."),
        ("pgbx_failed_jobs", "failures_total", "Failed jobs in the retained history (pgbx.audit_days)."),
        ("pgbx_queue_depth", "queued_jobs", "Jobs queued and not yet started."),
        ("pgbx_restore_test_ok", "last_verify_ok", "1 if the last restore test passed, 0 if it failed (absent: never ran)."),
        ("pgbx_last_backup_encrypted", "last_backup_encrypted", "1 if the newest backup is client-side encrypted."),
        ("pgbx_overview_age_seconds", "seen_age", "Seconds since the worker last refreshed this row (worker heartbeat)."),
    ];
    o.push_str("# HELP pgbx_up 1 when pgbx could read the server overview.\n# TYPE pgbx_up gauge\npgbx_up 1\n");
    o.push_str(&format!("# HELP pgbx_databases Databases pgbx is looking after.\n# TYPE pgbx_databases gauge\npgbx_databases {}\n", dbs.len()));
    for (name, col, help) in fams {
        o.push_str(&format!("# HELP {name} {help}\n# TYPE {name} gauge\n"));
        for d in dbs {
            if let Some(v) = num(&d[*col]) {
                o.push_str(&format!("{name}{{database=\"{}\"}} {}\n", esc(d["database"].as_str().unwrap_or("")), fmt(v)));
            }
        }
    }
    o.push_str("# HELP pgbx_database_state 1 for the database's current state.\n# TYPE pgbx_database_state gauge\n");
    for d in dbs {
        o.push_str(&format!(
            "pgbx_database_state{{database=\"{}\",state=\"{}\"}} 1\n",
            esc(d["database"].as_str().unwrap_or("")),
            esc(d["state"].as_str().unwrap_or("unknown"))
        ));
    }
    o
}

fn fmt(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 { format!("{}", v as i64) } else { format!("{v:.3}") }
}

/// The text for one scrape; on error a body with pgbx_up 0 (so alerts fire on "pgbx unreadable").
pub fn scrape(c: &mut postgres::Client) -> (bool, String) {
    match rows(c, SQL, &[]) {
        Ok(v) => (true, render(&v)),
        Err(e) => (false, format!("# pgbx: {}\n# TYPE pgbx_up gauge\npgbx_up 0\n", e.replace('\n', " "))),
    }
}

/// `pgbx metrics`: one scrape printed as text (or inside JSON with --json).
pub fn cmd(cx: &mut Ctx) -> Out {
    let admin = cx.admin_db();
    let mut c = cx.connect(&admin)?;
    let _ = c.batch_execute(crate::ui::READ_ONLY_SQL);
    let (ok, text) = scrape(&mut c);
    Ok(json!({"ok": ok, "database": admin, "text": text}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_prometheus_text() {
        let dbs = vec![
            json!({"database": "shop", "state": "active", "last_ts": 1790000000.0, "age": 3600.5, "last_backup_bytes": 12345,
                   "backups_kept": 14, "failures_total": 0, "queued_jobs": 1, "last_verify_ok": true,
                   "last_backup_encrypted": false, "seen_age": 4.0}),
            json!({"database": "we\"ird", "state": "waiting for first backup", "last_ts": null, "age": null,
                   "last_backup_bytes": null, "backups_kept": 0, "failures_total": 2, "queued_jobs": 0,
                   "last_verify_ok": null, "last_backup_encrypted": null, "seen_age": 1.0}),
        ];
        let t = render(&dbs);
        assert!(t.contains("pgbx_up 1\n") && t.contains("pgbx_databases 2\n"));
        assert!(t.contains("pgbx_last_backup_timestamp_seconds{database=\"shop\"} 1790000000\n"));
        assert!(t.contains("pgbx_last_backup_age_seconds{database=\"shop\"} 3600.500\n"));
        assert!(t.contains("pgbx_restore_test_ok{database=\"shop\"} 1\n"));
        assert!(!t.contains("pgbx_restore_test_ok{database=\"we"), "never ran = absent");
        assert!(t.contains("pgbx_failed_jobs{database=\"we\\\"ird\"} 2\n"));
        assert!(t.contains("pgbx_database_state{database=\"we\\\"ird\",state=\"waiting for first backup\"} 1\n"));
        assert!(t.contains("pgbx_queue_depth{database=\"shop\"} 1\n"));
        // every sample line belongs to a declared family
        for l in t.lines().filter(|l| !l.starts_with('#')) {
            let fam = l.split(['{', ' ']).next().unwrap();
            assert!(t.contains(&format!("# TYPE {fam} gauge")), "{l}");
        }
    }
}
