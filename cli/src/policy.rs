//! Policy and access subcommands: 1:1 with the extension's SQL. Showing is read-only; changes that
//! reduce protection (pause, lowering retention, narrowing data scope, disabling restore tests) need --yes.
use crate::{one, pe, rows, Args, Ctx, Out};
use serde_json::{json, Value};

/// Refuse a higher-risk change unless --yes was given.
pub fn require_yes(a: &Args, risk: Option<String>) -> Result<(), String> {
    match risk {
        Some(r) if !a.has("yes") => Err(format!("refusing without --yes: {r}")),
        _ => Ok(()),
    }
}

pub fn retention_risk(cur: (i64, i64), new_backups: Option<i64>, new_days: Option<i64>) -> Option<String> {
    let mut v = vec![];
    if let Some(b) = new_backups.filter(|b| *b < cur.0) {
        v.push(format!("max backups {} -> {b}", cur.0));
    }
    if let Some(d) = new_days.filter(|d| *d < cur.1) {
        v.push(format!("max days {} -> {d}", cur.1));
    }
    (!v.is_empty()).then(|| format!("lowering retention ({}) deletes older backups now", v.join(", ")))
}

/// Narrowing = any new exclude pattern, or an include list that is new or drops patterns.
pub fn scope_risk(cur_inc: &[String], cur_exc: &[String], new_inc: &[String], new_exc: &[String]) -> Option<String> {
    let mut v = vec![];
    let added_exc: Vec<_> = new_exc.iter().filter(|p| !cur_exc.contains(p)).cloned().collect();
    if !added_exc.is_empty() {
        v.push(format!("rows of {} will no longer be backed up", added_exc.join(", ")));
    }
    if !new_inc.is_empty() {
        if cur_inc.is_empty() {
            v.push(format!("only rows of {} will be backed up", new_inc.join(", ")));
        } else {
            let dropped: Vec<_> = cur_inc.iter().filter(|p| !new_inc.contains(p)).cloned().collect();
            if !dropped.is_empty() {
                v.push(format!("rows of {} drop out of the include list", dropped.join(", ")));
            }
        }
    }
    (!v.is_empty()).then(|| format!("narrowing the data scope: {}", v.join("; ")))
}

pub fn verify_schedule_risk(s: &str) -> Option<String> {
    matches!(s.trim().to_lowercase().as_str(), "never" | "off")
        .then(|| "turning restore tests off means nobody notices when backups stop being restorable".to_string())
}

pub fn pause_risk() -> Option<String> {
    Some("pausing stops automatic backups until `pgbx resume`".into())
}

fn list(s: Option<&str>) -> Vec<String> {
    s.map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()).unwrap_or_default()
}

fn text_call(cx: &mut Ctx, sql: &str, p: &[&(dyn postgres::types::ToSql + Sync)]) -> Out {
    let db = cx.db();
    let mut c = cx.connect(&db)?;
    let msg: String = c.query_one(sql, p).map_err(pe)?.get(0);
    Ok(json!({"ok": true, "database": db, "message": msg}))
}

pub fn schedule(cx: &mut Ctx) -> Out {
    let db = cx.db();
    if cx.a.pos.first().map(String::as_str) == Some("suggest") {
        return suggest(cx);
    }
    match cx.a.pos.first().cloned() {
        None => {
            let mut c = cx.connect(&db)?;
            let r = one(&mut c, "SELECT schedule, cron, next_backup_at, state, verify_schedule FROM pgbx.status()", &[])?;
            Ok(json!({"ok": true, "database": db, "schedule": r}))
        }
        Some(s) => text_call(cx, "SELECT pgbx.set_schedule($1)", &[&s]),
    }
}

/// `pgbx schedule suggest [--hours N] [--apply [--yes]]`: the quietest window learned from activity, with the
/// configure() call to copy. Never applied by itself: --apply asks first on a terminal; otherwise it needs --yes.
pub fn suggest(cx: &mut Ctx) -> Out {
    let db = cx.db();
    let hours: i32 = match cx.a.get("hours") {
        Some(h) => h.parse().ok().filter(|n| (1..=12).contains(n)).ok_or("--hours must be 1-12")?,
        None => 1,
    };
    let mut c = cx.connect(&db)?;
    let w = one(&mut c, "SELECT * FROM pgbx.suggest_window($1)", &[&hours])?;
    let mut out = json!({"ok": true, "database": db, "suggestion": w.clone(),
        "apply_sql": w["apply_sql"], "applied": false,
        "note": "never applied by itself: run apply_sql in a migration, or pgbx schedule suggest --apply"});
    if !cx.a.has("apply") {
        return Ok(out);
    }
    let cron = w["cron"].as_str().ok_or("nothing to apply: no activity samples yet (the worker learns them hour by hour)")?.to_string();
    let what = format!("set the backup schedule of {db} to '{cron}' ({}; now {})", crate::scalar(&w["start_at"]), crate::scalar(&w["current_schedule"]));
    if !cx.a.has("yes") {
        use std::io::IsTerminal;
        if cx.a.has("json") || !std::io::stdin().is_terminal() {
            return Err(format!("refusing without --yes: {what}"));
        }
        eprint!("{what}? [y/N] ");
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        if !matches!(line.trim(), "y" | "Y" | "yes") {
            out["note"] = json!("not applied (answered no)");
            return Ok(out);
        }
    }
    let r = one(&mut c, "SELECT schedule, schedule_label FROM pgbx.configure(schedule => $1)", &[&cron])?;
    out["applied"] = json!(true);
    out["schedule"] = r;
    out["note"] = json!(what);
    Ok(out)
}

fn current_retention(cx: &mut Ctx, db: &str) -> Result<(i64, i64), String> {
    let mut c = cx.connect(db)?;
    let r = c.query_one("SELECT coalesce((SELECT max_backups FROM pgbx.config), 14)::bigint,
                               coalesce((SELECT max_days FROM pgbx.config), 90)::bigint", &[]).map_err(pe)?;
    Ok((r.get(0), r.get(1)))
}

fn int_flag(a: &Args, k: &str) -> Result<Option<i64>, String> {
    a.get(k).map(|v| v.parse::<i64>().ok().filter(|n| *n >= 1).ok_or(format!("--{k} must be a positive number"))).transpose()
}

pub fn retention(cx: &mut Ctx) -> Out {
    let db = cx.db();
    let (nb, nd) = (int_flag(&cx.a, "max-backups")?, int_flag(&cx.a, "max-days")?);
    let cur = current_retention(cx, &db)?;
    if nb.is_none() && nd.is_none() {
        return Ok(json!({"ok": true, "database": db, "max_backups": cur.0, "max_days": cur.1}));
    }
    require_yes(&cx.a, retention_risk(cur, nb, nd))?;
    let (b, d) = (nb.map(|x| x as i32), nd.map(|x| x as i32));
    text_call(cx, "SELECT pgbx.set_retention($1, $2)", &[&b, &d])
}

pub fn pause(cx: &mut Ctx) -> Out {
    require_yes(&cx.a, pause_risk())?;
    let reason = cx.a.get("reason").map(String::from);
    text_call(cx, "SELECT pgbx.pause($1)", &[&reason])
}

pub fn resume(cx: &mut Ctx) -> Out {
    text_call(cx, "SELECT pgbx.resume()", &[])
}

pub fn scope(cx: &mut Ctx) -> Out {
    let db = cx.db();
    let (inc, exc, reset) = (list(cx.a.get("include")), list(cx.a.get("exclude")), cx.a.has("reset"));
    let mut c = cx.connect(&db)?;
    if inc.is_empty() && exc.is_empty() && !reset {
        let st = one(&mut c, "SELECT data_scope FROM pgbx.status()", &[])?;
        let rl = rows(&mut c, "SELECT table_name FROM pgbx.rowless_tables()", &[])?;
        return Ok(json!({"ok": true, "database": db, "data_scope": st["data_scope"],
            "rowless_tables": rl.iter().map(|r| r["table_name"].clone()).collect::<Vec<Value>>()}));
    }
    if reset && !(inc.is_empty() && exc.is_empty()) {
        return Err("--reset cannot be combined with --include/--exclude".into());
    }
    let cur = one(&mut c, "SELECT coalesce((SELECT include_data FROM pgbx.config), '{}') AS i,
                                  coalesce((SELECT exclude_data FROM pgbx.config), '{}') AS e", &[])?;
    let arr = |v: &Value| -> Vec<String> { v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default() };
    require_yes(&cx.a, scope_risk(&arr(&cur["i"]), &arr(&cur["e"]), &inc, &exc))?;
    drop(c);
    let (i, e) = ((!inc.is_empty()).then_some(inc), (!exc.is_empty()).then_some(exc));
    text_call(cx, "SELECT pgbx.set_data_scope($1, $2)", &[&i, &e])
}

pub fn verify_schedule(cx: &mut Ctx) -> Out {
    let s = cx.a.pos.first().cloned().ok_or("give a schedule, e.g. pgbx verify-schedule 'weekly on sunday at 04:00' (or 'never')")?;
    require_yes(&cx.a, verify_schedule_risk(&s))?;
    text_call(cx, "SELECT pgbx.set_verify_schedule($1)", &[&s])
}

pub fn link(cx: &mut Ctx) -> Out {
    let db = cx.db();
    let id: Option<i64> = cx.a.get("backup-id").map(|v| v.parse().map_err(|_| "--backup-id must be a number".to_string())).transpose()?;
    let exp = cx.a.get("expires").unwrap_or("1 hour").to_string();
    let mut c = cx.connect(&db)?;
    let url: String = c.query_one("SELECT pgbx.download_url($1, ($2::text)::interval)", &[&id, &exp]).map_err(pe)?.get(0);
    Ok(json!({"ok": true, "database": db, "url": url, "expires": exp}))
}

pub fn overview(cx: &mut Ctx) -> Out {
    let admin = cx.admin_db();
    let mut c = cx.connect(&admin)?;
    Ok(json!({"ok": true, "databases": rows(&mut c, "SELECT * FROM pgbx.overview()", &[])?}))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn a(s: &[&str]) -> Args {
        crate::parse_args(s.iter().map(|x| x.to_string())).unwrap()
    }
    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn retention_lowering_needs_yes() {
        let r = retention_risk((14, 90), Some(7), None);
        assert!(require_yes(&a(&["retention"]), r.clone()).unwrap_err().contains("--yes"));
        assert!(require_yes(&a(&["retention", "--yes"]), r).is_ok());
        assert_eq!(retention_risk((14, 90), Some(20), Some(120)), None);
        assert!(retention_risk((14, 90), None, Some(30)).unwrap().contains("90 -> 30"));
    }

    #[test]
    fn scope_narrowing_needs_yes() {
        assert!(scope_risk(&[], &[], &[], &v(&["public.sessions"])).is_some());
        assert!(scope_risk(&[], &[], &v(&["billing.*"]), &[]).is_some());
        assert!(scope_risk(&v(&["a", "b"]), &[], &v(&["a"]), &[]).is_some());
        assert_eq!(scope_risk(&v(&["a"]), &v(&["x"]), &v(&["a", "b"]), &v(&["x"])), None, "widening is safe");
        assert!(require_yes(&a(&["scope"]), scope_risk(&[], &[], &[], &v(&["t"]))).is_err());
    }

    #[test]
    fn pause_and_verify_never_need_yes() {
        assert!(require_yes(&a(&["pause"]), pause_risk()).is_err());
        assert!(require_yes(&a(&["pause", "--yes"]), pause_risk()).is_ok());
        assert!(require_yes(&a(&["verify-schedule", "never"]), verify_schedule_risk("never")).is_err());
        assert!(verify_schedule_risk(" OFF ").is_some());
        assert_eq!(verify_schedule_risk("weekly on sunday at 04:00"), None);
    }

    #[test]
    fn splits_pattern_lists() {
        assert_eq!(list(Some("a, b.*,,c")), v(&["a", "b.*", "c"]));
    }
}
