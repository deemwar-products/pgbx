//! `pgbx jobs` — the server-wide job queue (pgbx.server_queue in the admin database, rewritten by the worker every
//! poll): what runs, what waits and why. `pgbx jobs cancel ID [--db X] --yes` cancels a queued or running job.
use crate::{one, pe, rows, scalar, Ctx, Out};
use serde_json::{json, Value};

const QUEUE_SQL: &str = "SELECT database, job_id, kind, trigger, state, position, slot, requested_at, started_at, detail,
                                progress, eta_start, eta_finish, est_bytes, done_bytes, seen_at
                         FROM pgbx.server_queue
                         ORDER BY state IN ('running', 'cancelling') DESC, position NULLS LAST, requested_at";

pub fn run(cx: &mut Ctx) -> Out {
    match cx.a.pos.first().map(String::as_str) {
        None | Some("list") => list(cx),
        Some("cancel") => cancel(cx),
        Some(x) => Err(format!("unknown jobs action '{x}' (pgbx jobs | pgbx jobs cancel ID [--db X] --yes)")),
    }
}

fn list(cx: &mut Ctx) -> Out {
    let admin = cx.admin_db();
    let mut c = cx.connect(&admin)?;
    let jobs = rows(&mut c, QUEUE_SQL, &[])?;
    let slots = one(
        &mut c,
        "SELECT current_setting('pgbx.max_concurrent_jobs', true) AS max_concurrent_jobs,
                current_setting('pgbx.restore_lane', true) AS restore_lane,
                (SELECT max(seen_at) FROM pgbx.server_queue) AS seen_at",
        &[],
    )?;
    Ok(json!({"ok": true, "database": admin, "jobs": jobs, "slots": slots,
              "note": "job ids are per database: cancel with pgbx jobs cancel ID --db DATABASE --yes"}))
}

/// The database a job id belongs to: --db, or the one queue row with that id.
pub fn job_db(given: Option<&str>, id: i64, queue: &[Value]) -> Result<String, String> {
    if let Some(d) = given {
        return Ok(d.to_string());
    }
    let dbs: Vec<String> =
        queue.iter().filter(|r| r["job_id"].as_i64() == Some(id)).filter_map(|r| r["database"].as_str().map(String::from)).collect();
    match dbs.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(format!("job {id} is not in the server queue (finished already?); add --db DATABASE")),
        _ => Err(format!("job id {id} exists in several databases ({}); add --db DATABASE", dbs.join(", "))),
    }
}

fn cancel(cx: &mut Ctx) -> Out {
    let id: i64 = cx.a.pos.get(1).and_then(|s| s.parse().ok()).ok_or("usage: pgbx jobs cancel ID [--db X] --yes")?;
    let queue = {
        let admin = cx.admin_db();
        let mut c = cx.connect(&admin)?;
        rows(&mut c, QUEUE_SQL, &[]).unwrap_or_default()
    };
    let db = job_db(cx.a.get("db"), id, &queue)?;
    if !cx.a.has("yes") {
        let what = queue.iter().find(|r| r["job_id"].as_i64() == Some(id) && r["database"] == db.as_str())
            .map(|r| format!("{} {} job {id} in {db}", scalar(&r["state"]), scalar(&r["kind"])))
            .unwrap_or(format!("job {id} in {db}"));
        return Err(format!("refusing without --yes: cancelling {what} (a cancelled backup is simply not taken)"));
    }
    let mut c = cx.connect(&db)?;
    let msg: String = c.query_one("SELECT pgbx.cancel($1)", &[&id]).map_err(pe)?.get(0);
    Ok(json!({"ok": true, "database": db, "job_id": id, "message": msg}))
}

/// Human view: one line per job.
pub fn text(v: &Value) -> String {
    let mut s = String::new();
    let jobs = v["jobs"].as_array().cloned().unwrap_or_default();
    if jobs.is_empty() {
        s.push_str("no jobs running or queued\n");
    }
    for r in jobs {
        let pos = match (&r["position"], &r["slot"]) {
            (Value::Number(p), _) => format!("#{p} in line"),
            (_, Value::Number(n)) if n.as_i64() == Some(0) => "restore lane".into(),
            (_, Value::Number(n)) => format!("slot {n}"),
            _ => "-".into(),
        };
        let progress = r["progress"].as_str().map(|p| format!(" [{p}]")).unwrap_or_default();
        s.push_str(&format!(
            "{:<20} {:>6} {:<8} {:<10} {:<12} {}{progress}\n",
            scalar(&r["database"]), scalar(&r["job_id"]), scalar(&r["kind"]), scalar(&r["state"]), pos, scalar(&r["detail"])
        ));
    }
    s.push_str(&format!(
        "slots: max_concurrent_jobs={} restore_lane={}\n",
        scalar(&v["slots"]["max_concurrent_jobs"]), scalar(&v["slots"]["restore_lane"])
    ));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_db_lookup() {
        let q = vec![json!({"database": "shop", "job_id": 7}), json!({"database": "crm", "job_id": 7}), json!({"database": "crm", "job_id": 9})];
        assert_eq!(job_db(Some("x"), 7, &q).unwrap(), "x");
        assert_eq!(job_db(None, 9, &q).unwrap(), "crm");
        assert!(job_db(None, 7, &q).unwrap_err().contains("several databases"));
        assert!(job_db(None, 5, &q).unwrap_err().contains("--db"));
    }

    #[test]
    fn text_lines() {
        let v = json!({"jobs": [{"database": "shop", "job_id": 3, "kind": "backup", "state": "running", "slot": 1, "position": null,
                                 "detail": "running in job slot 1", "progress": "41 % · ~9 min left"},
                                {"database": "crm", "job_id": 4, "kind": "restore", "state": "queued", "slot": null, "position": 1,
                                 "detail": "waits"}],
                       "slots": {"max_concurrent_jobs": "1", "restore_lane": "on"}});
        let t = text(&v);
        assert!(t.contains("slot 1") && t.contains("#1 in line") && t.contains("restore_lane=on") && t.contains("[41 % · ~9 min left]"), "{t}");
    }
}
