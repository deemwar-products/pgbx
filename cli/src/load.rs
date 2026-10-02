//! `pgbx load` — the load gate (ADR 0001 §1): the server's last load sample, the thresholds, and per database the gate
//! mode and what it would have deferred (shadow), deferred (on) or forced. `pgbx load --gate off|shadow|on|default --db X`
//! sets one database's gate (`on` needs --yes: backups may then start up to pgbx.max_defer later).
use crate::{one, rows, scalar, Args, Ctx, Out};
use serde_json::{json, Value};

pub const LOAD_SQL: &str = "SELECT load_at, load_busy, load_reasons, load_active, load_tps, load_factor FROM pgbx.server_capacity";
pub const SETTINGS_SQL: &str = "SELECT current_setting('pgbx.load_gate', true) AS load_gate,
        current_setting('pgbx.gate_manual_jobs', true) AS gate_manual_jobs,
        current_setting('pgbx.busy_active_backends', true) AS busy_active_backends, current_setting('pgbx.busy_tps', true) AS busy_tps,
        current_setting('pgbx.busy_long_xact', true) AS busy_long_xact, current_setting('pgbx.busy_replica_lag', true) AS busy_replica_lag,
        current_setting('pgbx.busy_loadavg', true) AS busy_loadavg, current_setting('pgbx.max_defer', true) AS max_defer,
        current_setting('pgbx.defer_backoff', true) AS defer_backoff";
pub const DBS_SQL: &str = "SELECT database, load_gate, would_defer_7d, deferred_7d, forced_7d FROM pgbx.server_overview ORDER BY database";

/// --gate value -> the risk that needs --yes (None = safe).
pub fn gate_risk(g: &str) -> Result<Option<String>, String> {
    match g {
        "on" => Ok(Some("load_gate = on defers scheduled backups while the server is busy, up to pgbx.max_defer".into())),
        "off" | "shadow" | "default" => Ok(None),
        x => Err(format!("--gate '{x}': use off, shadow, on or default (follow the server's pgbx.load_gate)")),
    }
}

pub fn level_of(a: &Args) -> crate::Level {
    match a.get("gate") {
        None => crate::Level::ReadOnly,
        Some("on") => crate::Level::Guarded,
        Some(_) => crate::Level::Safe,
    }
}

pub fn run(cx: &mut Ctx) -> Out {
    if let Some(g) = cx.a.get("gate").map(String::from) {
        let db = cx.a.get("db").ok_or("--gate needs --db DATABASE (the gate is set per database)")?.to_string();
        crate::policy::require_yes(&cx.a, gate_risk(&g)?)?;
        let mut c = cx.connect(&db)?;
        let r = one(&mut c, "SELECT coalesce(load_gate, 'default (' || current_setting('pgbx.load_gate', true) || ')') AS load_gate
                              FROM pgbx.configure(load_gate => $1)", &[&g])?;
        return Ok(json!({"ok": true, "database": db, "load_gate": r["load_gate"]}));
    }
    let admin = cx.admin_db();
    let mut c = cx.connect(&admin)?;
    let sample = one(&mut c, LOAD_SQL, &[])?;
    let settings = one(&mut c, SETTINGS_SQL, &[])?;
    let dbs = rows(&mut c, DBS_SQL, &[])?;
    let deferred = rows(&mut c, "SELECT database, job_id, kind, detail FROM pgbx.server_queue WHERE state = 'deferred' ORDER BY database, job_id", &[])
        .map_err(|e| e.to_string())?;
    let mut out = json!({"ok": true, "database": admin, "sample": sample, "settings": settings, "databases": dbs, "deferred_jobs": deferred});
    if let Some(db) = cx.a.get("db").map(String::from) {
        let mut d = cx.connect(&db)?;
        let recent = rows(&mut d, "SELECT id, kind, trigger, state, requested_at, started,
                                          params->>'would_defer_reason' AS would_defer, params->>'defer_reason' AS deferred,
                                          params->>'busy_deferrals' AS busy_deferrals, params->>'forced' AS forced,
                                          params->>'load_at_start' AS load_at_start
                                     FROM pgbx.history WHERE kind IN ('backup', 'verify') AND requested_at > now() - interval '7 days'
                                      AND (params ? 'would_defer' OR params ? 'busy_deferrals' OR params ? 'load_at_start')
                                    ORDER BY id DESC LIMIT 20", &[]).map_err(|e| e.to_string())?;
        out["recent_jobs"] = json!({"database": db, "jobs": recent});
    }
    Ok(out)
}

/// Human view.
pub fn text(v: &Value) -> String {
    let s = &v["sample"];
    let mut o = format!(
        "load: {} at {} ({} active session(s), {} tps, load x{})\n",
        if s["load_busy"] == true { format!("BUSY: {}", scalar(&s["load_reasons"])) } else if s["load_busy"] == false { "quiet".into() } else { "no sample yet".into() },
        scalar(&s["load_at"]), scalar(&s["load_active"]), scalar(&s["load_tps"]), scalar(&s["load_factor"])
    );
    let st = &v["settings"];
    o.push_str(&format!(
        "gate: pgbx.load_gate = {} (manual jobs: {}); busy when active > {}, tps > {}, writer open > {}, replica lag > {}, load/core > {}; max_defer {}\n",
        scalar(&st["load_gate"]), scalar(&st["gate_manual_jobs"]), scalar(&st["busy_active_backends"]), scalar(&st["busy_tps"]),
        scalar(&st["busy_long_xact"]), scalar(&st["busy_replica_lag"]), scalar(&st["busy_loadavg"]), scalar(&st["max_defer"])
    ));
    for d in v["databases"].as_array().cloned().unwrap_or_default() {
        o.push_str(&format!(
            "  {:<20} gate {:<7} last 7 days: {} would have waited, {} deferred, {} forced\n",
            scalar(&d["database"]), scalar(&d["load_gate"]), scalar(&d["would_defer_7d"]), scalar(&d["deferred_7d"]), scalar(&d["forced_7d"])
        ));
    }
    for j in v["deferred_jobs"].as_array().cloned().unwrap_or_default() {
        o.push_str(&format!("  deferred: {} {} #{}: {}\n", scalar(&j["database"]), scalar(&j["kind"]), scalar(&j["job_id"]), scalar(&j["detail"])));
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_values() {
        assert!(gate_risk("on").unwrap().is_some());
        assert_eq!(gate_risk("shadow").unwrap(), None);
        assert_eq!(gate_risk("default").unwrap(), None);
        assert!(gate_risk("maybe").is_err());
    }

    #[test]
    fn text_view() {
        let v = json!({"sample": {"load_busy": true, "load_reasons": "12 active sessions > 4", "load_at": "t", "load_active": 12},
                       "settings": {"load_gate": "shadow"}, "databases": [{"database": "shop", "load_gate": "on", "would_defer_7d": 2}],
                       "deferred_jobs": []});
        let t = text(&v);
        assert!(t.contains("BUSY: 12 active") && t.contains("pgbx.load_gate = shadow") && t.contains("shop"), "{t}");
    }
}
