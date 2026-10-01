//! 30-day audit retention: the per-database worker prunes `pgbx.history` rows older than
//! `pgbx.audit_days`, except rows that still matter (see `keep`).

use postgres::Client;

/// One history row, as far as pruning cares.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub id: i64,
    pub kind: String,
    pub state: String,
    pub has_key: bool,
    /// age in days of coalesce(finished, started, requested_at)
    pub age_days: f64,
    /// the newest row of its kind
    pub latest_of_kind: bool,
}

/// Rows kept regardless of age:
/// - a backup that still exists in S3 (done, has an s3_key; expired backups have state 'expired')
/// - any job still queued or running
/// - the most recent row of each kind (so "last restore test" etc. never vanish)
pub fn keep(r: &Row, audit_days: i32) -> bool {
    let days = audit_days.max(1) as f64;
    r.age_days < days
        || (r.kind == "backup" && r.state == "done" && r.has_key)
        || matches!(r.state.as_str(), "queued" | "running")
        || r.latest_of_kind
}

/// Ids to delete.
pub fn prune_ids(rows: &[Row], audit_days: i32) -> Vec<i64> {
    rows.iter().filter(|r| !keep(r, audit_days)).map(|r| r.id).collect()
}

/// Prune this database's history. Returns how many rows were deleted.
pub fn prune(cl: &mut Client, audit_days: i32) -> Result<u64, String> {
    let rows: Vec<Row> = cl
        .query(
            "SELECT id, kind, state, s3_key IS NOT NULL, age_days, latest FROM (
                 SELECT id, kind, state, s3_key,
                        extract(epoch FROM now() - coalesce(finished, started, requested_at))::float8 / 86400 AS age_days,
                        id = max(id) OVER (PARTITION BY kind) AS latest
                 FROM pgbx.history) h
             WHERE age_days >= $1::float8",
            &[&(audit_days.max(1) as f64)],
        )
        .map_err(|e| e.to_string())?
        .iter()
        .map(|r| Row {
            id: r.get(0),
            kind: r.get(1),
            state: r.get(2),
            has_key: r.get(3),
            age_days: r.get(4),
            latest_of_kind: r.get(5),
        })
        .collect();
    let ids = prune_ids(&rows, audit_days);
    if ids.is_empty() {
        return Ok(0);
    }
    cl.execute("DELETE FROM pgbx.history WHERE id = ANY($1)", &[&ids]).map_err(|e| e.to_string())
}

#[cfg(test)]
mod t {
    use super::*;

    fn r(id: i64, kind: &str, state: &str, key: bool, age: f64, latest: bool) -> Row {
        Row { id, kind: kind.into(), state: state.into(), has_key: key, age_days: age, latest_of_kind: latest }
    }

    #[test]
    fn prune_selection() {
        let rows = vec![
            r(1, "backup", "done", true, 40.0, false),     // still in S3: kept
            r(2, "backup", "expired", true, 40.0, false),  // gone from S3: pruned
            r(3, "backup", "failed", false, 40.0, false),  // old failure: pruned
            r(4, "verify", "running", false, 90.0, false), // running job: kept
            r(5, "verify", "done", false, 90.0, false),    // old restore test, not the latest: pruned
            r(6, "prune", "done", false, 60.0, true),      // newest prune: kept
            r(7, "verify", "done", false, 31.0, true),     // last restore test: kept
            r(8, "config", "done", false, 10.0, false),    // young: kept
            r(9, "restore", "queued", false, 45.0, false), // queued restore: kept
            r(10, "backup", "queued", false, 45.0, false), // pending job: kept
            r(11, "config", "done", false, 30.0, false),   // exactly audit_days old: pruned
        ];
        assert_eq!(prune_ids(&rows, 30), vec![2, 3, 5, 11]);
        // min 1 day
        assert_eq!(prune_ids(&[r(1, "config", "done", false, 0.5, false)], 0), Vec::<i64>::new());
        assert_eq!(prune_ids(&[r(1, "config", "done", false, 1.5, false)], 0), vec![1]);
    }
}
