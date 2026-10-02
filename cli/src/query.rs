//! `pgbx query "SQL"`: one SELECT-style statement, typed JSON rows — so an agent can inspect a server
//! without a shell or psql. Layers (a best-effort guard for agents, NOT a security boundary; roles and
//! permissions are the user's business):
//!   a. exactly one statement;
//!   b. it starts with SELECT, WITH, TABLE, VALUES, SHOW or EXPLAIN (without ANALYZE); no INSERT/UPDATE/
//!      DELETE/MERGE anywhere (data-modifying WITH), no SELECT ... INTO, no FOR UPDATE/SHARE;
//!   c. no known side-effect functions (DENY_FNS) and no pgbx.* function except the read ones (PGBX_READ_FNS);
//!   d. run inside BEGIN READ ONLY with SET LOCAL statement_timeout / lock_timeout, then ROLLBACK.

use crate::{pe, Ctx, Out};
use serde_json::{json, Map, Value};
use std::time::Duration;

/// Split on top-level ';' (outside quotes, dollar quotes and comments); return the non-empty statements.
pub fn statements(sql: &str) -> Vec<String> {
    let b: Vec<char> = sql.chars().collect();
    let (mut out, mut cur, mut i) = (vec![], String::new(), 0);
    let push = |cur: &mut String, out: &mut Vec<String>| {
        if !strip_comments(cur).trim().is_empty() {
            out.push(cur.trim().to_string());
        }
        cur.clear();
    };
    while i < b.len() {
        let c = b[i];
        let next = b.get(i + 1).copied();
        if c == '-' && next == Some('-') {
            while i < b.len() && b[i] != '\n' {
                cur.push(b[i]);
                i += 1;
            }
            continue;
        }
        if c == '/' && next == Some('*') {
            let mut depth = 0;
            while i < b.len() {
                if b[i] == '/' && b.get(i + 1) == Some(&'*') {
                    depth += 1;
                    cur.push_str("/*");
                    i += 2;
                } else if b[i] == '*' && b.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    cur.push_str("*/");
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    cur.push(b[i]);
                    i += 1;
                }
            }
            continue;
        }
        if c == '\'' || c == '"' {
            cur.push(c);
            i += 1;
            while i < b.len() {
                cur.push(b[i]);
                if b[i] == c {
                    if b.get(i + 1) == Some(&c) {
                        cur.push(c);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        if c == '$' {
            // $tag$ ... $tag$ (tag: empty or identifier not starting with a digit)
            let mut j = i + 1;
            while j < b.len() && (b[j].is_alphanumeric() || b[j] == '_') {
                j += 1;
            }
            let tag_ok = j < b.len() && b[j] == '$' && !b.get(i + 1).is_some_and(|d| d.is_ascii_digit());
            if tag_ok {
                let tag: String = b[i..=j].iter().collect();
                let rest: String = b[j + 1..].iter().collect();
                let end = rest.find(&tag).map(|k| j + 1 + rest[..k].chars().count() + tag.chars().count()).unwrap_or(b.len());
                cur.extend(&b[i..end]);
                i = end;
                continue;
            }
        }
        if c == ';' {
            push(&mut cur, &mut out);
        } else {
            cur.push(c);
        }
        i += 1;
    }
    push(&mut cur, &mut out);
    out
}

fn strip_comments(s: &str) -> String {
    let mut o = String::new();
    let mut it = s.lines();
    for l in it.by_ref() {
        o.push_str(l.split("--").next().unwrap_or(""));
        o.push('\n');
    }
    // a statement that is only /* ... */ is empty too
    let t = o.trim();
    if t.starts_with("/*") && t.ends_with("*/") {
        String::new()
    } else {
        o
    }
}

pub fn single_statement(sql: &str) -> Result<String, String> {
    let s = statements(sql);
    match s.len() {
        0 => Err("empty query".into()),
        1 => Ok(s.into_iter().next().unwrap()),
        n => Err(format!("pgbx query runs exactly one statement; got {n} (separate calls, or one SELECT)")),
    }
}

const STARTS: &[&str] = &["select", "with", "table", "values", "show", "explain"];
const WRITES: &[&str] = &["insert", "update", "delete", "merge", "into", "truncate", "copy", "call", "do"];
/// Exact names, or `*` prefix patterns.
const DENY_FNS: &[&str] = &[
    "pg_terminate_backend", "pg_cancel_backend", "pg_reload_conf", "pg_rotate_logfile", "pg_switch_wal", "pg_switch_xlog",
    "pg_promote", "pg_create_restore_point", "set_config", "setval", "nextval", "pg_advisory*", "pg_try_advisory*", "lo_*",
    "dblink*", "pg_file_write", "pg_file_rename", "pg_file_unlink", "pg_logical_emit_message", "txid_current",
    "pg_current_xact_id", "pg_notify", "pg_sleep*", "pg_read_file", "pg_read_binary_file", "pg_stat_reset*",
    "pg_create_*replication_slot", "pg_drop_replication_slot", "pg_replication_slot_advance", "pg_log_backend_memory_contexts",
    "pg_backup_start", "pg_backup_stop", "pg_start_backup", "pg_stop_backup", "pg_wal_replay_pause", "pg_wal_replay_resume",
];
/// pgbx functions that only read (src/lib.rs); every other pgbx.*() queues jobs, changes policy or mints links.
const PGBX_READ_FNS: &[&str] = &["status", "overview", "doctor", "rowless_tables", "to_cron", "next_run_epoch"];

/// Lowercased SQL with comments removed, string literals blanked and identifier quotes dropped.
fn normalize(sql: &str) -> String {
    let b: Vec<char> = sql.chars().collect();
    let (mut o, mut i) = (String::new(), 0);
    while i < b.len() {
        let c = b[i];
        let next = b.get(i + 1).copied();
        if c == '-' && next == Some('-') {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
            o.push(' ');
        } else if c == '/' && next == Some('*') {
            let mut depth = 0;
            while i < b.len() {
                if b[i] == '/' && b.get(i + 1) == Some(&'*') {
                    depth += 1;
                    i += 2;
                } else if b[i] == '*' && b.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            o.push(' ');
        } else if c == '\'' || c == '"' {
            i += 1;
            while i < b.len() {
                if b[i] == c {
                    if b.get(i + 1) == Some(&c) {
                        i += 2;
                        continue;
                    }
                    break;
                }
                if c == '"' {
                    o.extend(b[i].to_lowercase());
                }
                i += 1;
            }
            i += 1;
            o.push_str(if c == '\'' { " '' " } else { "" });
        } else if c == '$' && !next.is_some_and(|d| d.is_ascii_digit()) {
            let mut j = i + 1;
            while j < b.len() && (b[j].is_alphanumeric() || b[j] == '_') {
                j += 1;
            }
            if j < b.len() && b[j] == '$' {
                let tag: String = b[i..=j].iter().collect();
                let rest: String = b[j + 1..].iter().collect();
                i = rest.find(&tag).map(|k| j + 1 + rest[..k].chars().count() + tag.chars().count()).unwrap_or(b.len());
                o.push_str(" '' ");
            } else {
                o.push(c);
                i += 1;
            }
        } else {
            o.extend(c.to_lowercase());
            i += 1;
        }
    }
    o
}

fn denied_fn(name: &str) -> bool {
    let last = name.rsplit('.').next().unwrap_or(name);
    if let Some(f) = name.strip_prefix("pgbx.") {
        return !PGBX_READ_FNS.contains(&f);
    }
    DENY_FNS.iter().any(|d| match d.split_once('*') {
        Some((pre, post)) => last.starts_with(pre) && last[pre.len()..].contains(post),
        None => last == *d,
    })
}

/// Layers b and c. `sql` is one statement.
pub fn check_read_only(sql: &str) -> Result<(), String> {
    let n = normalize(sql);
    let words: Vec<&str> = n.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|w| !w.is_empty()).collect();
    let first = words.first().copied().unwrap_or("");
    if !STARTS.contains(&first) {
        return Err(format!("pgbx query runs SELECT-style statements only (SELECT, WITH, TABLE, VALUES, SHOW, EXPLAIN); got '{}'",
            first.to_uppercase()));
    }
    if first == "explain" && words.iter().any(|w| *w == "analyze" || *w == "analyse") {
        return Err("EXPLAIN ANALYZE executes the statement; use plain EXPLAIN".into());
    }
    if let Some(w) = words.iter().find(|w| WRITES.contains(w)) {
        return Err(format!("pgbx query refuses '{}' (no writes, SELECT ... INTO, data-modifying WITH)", w.to_uppercase()));
    }
    for (i, w) in words.iter().enumerate() {
        if *w == "for" && matches!(words.get(i + 1).copied(), Some("share" | "no" | "key")) {
            return Err("pgbx query refuses FOR UPDATE / FOR SHARE (row locks)".into());
        }
    }
    // function calls: identifier (maybe schema-qualified) followed by '('
    let ch: Vec<char> = n.chars().collect();
    let mut i = 0;
    while i < ch.len() {
        if ch[i].is_alphabetic() || ch[i] == '_' {
            let s = i;
            while i < ch.len() && (ch[i].is_alphanumeric() || ch[i] == '_' || ch[i] == '$' || ch[i] == '.') {
                i += 1;
            }
            let name: String = ch[s..i].iter().collect();
            let mut j = i;
            while j < ch.len() && ch[j].is_whitespace() {
                j += 1;
            }
            if ch.get(j) == Some(&'(') && denied_fn(&name) {
                return Err(format!("pgbx query refuses {name}(): it has side effects (use the matching pgbx command instead)"));
            }
        } else {
            i += 1;
        }
    }
    Ok(())
}

/// "30", "30s", "500ms", "2m" -> Duration.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let bad = || format!("bad --timeout '{s}' (e.g. 30s, 500ms, 2m)");
    let (num, mul) = if let Some(n) = s.strip_suffix("ms") {
        (n, 1)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1000)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60_000)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3_600_000)
    } else {
        (s, 1000)
    };
    let n: u64 = num.trim().parse().map_err(|_| bad())?;
    if n == 0 {
        return Err(bad());
    }
    Ok(Duration::from_millis(n * mul))
}

/// Shape rows (objects from row_to_json) into the reply: cap at max, report truncation.
pub fn shape(columns: Vec<Value>, mut rows: Vec<Value>, max: usize) -> Value {
    let truncated = rows.len() > max;
    rows.truncate(max);
    json!({"ok": true, "columns": columns, "row_count": rows.len(), "truncated": truncated, "rows": rows})
}

pub fn run(cx: &mut Ctx) -> Out {
    let raw = cx.a.pos.first().ok_or("pgbx query \"SELECT ...\" [--db D] [--max-rows N] [--timeout 30s]")?.clone();
    if cx.a.pos.len() > 1 {
        return Err("put the SQL in ONE quoted argument".into());
    }
    let sql = single_statement(&raw)?;
    check_read_only(&sql)?;
    let max: usize = match cx.a.get("max-rows") {
        Some(m) => m.parse().ok().filter(|n| *n > 0).ok_or(format!("bad --max-rows '{m}'"))?,
        None => 1000,
    };
    let timeout = parse_duration(cx.a.get("timeout").unwrap_or("30s"))?;
    let db = cx.db();
    let mut c = cx.connect(&db)?;
    let user: String = c.query_one("SELECT current_user::text", &[]).map_err(pe)?.get(0);
    let mut t = c.transaction().map_err(pe)?;
    let ms = timeout.as_millis();
    t.batch_execute(&format!(
        "SET TRANSACTION READ ONLY; SET LOCAL statement_timeout = {ms}; SET LOCAL lock_timeout = {}",
        ms.min(5000)
    )).map_err(pe)?;
    let stmt = t.prepare(&sql).map_err(pe)?;
    let columns: Vec<Value> = stmt.columns().iter().map(|c| json!({"name": c.name(), "type": c.type_().name()})).collect();
    if columns.is_empty() {
        return Err("pgbx query is for statements that return rows (SELECT, WITH, SHOW, EXPLAIN, VALUES, TABLE)".into());
    }
    // row_to_json types values for us (numbers, bool, null, nested json); works for anything usable as a subquery
    let wrapped = format!("SELECT row_to_json(q)::text FROM ({sql}\n) q LIMIT {}", max + 1);
    let rows: Vec<Value> = t.execute("SAVEPOINT w", &[]).map_err(pe).and_then(|_| match t.query(&wrapped, &[]) {
        Ok(r) => r.iter().map(|r| serde_json::from_str(r.get::<_, &str>(0)).map_err(|e| e.to_string())).collect(),
        Err(e) if e.code() == Some(&postgres::error::SqlState::SYNTAX_ERROR) => {
            // SHOW / EXPLAIN cannot be a subquery: run it as is, values as text
            t.execute("ROLLBACK TO SAVEPOINT w", &[]).map_err(pe)?;
            let mut v = vec![];
            for m in t.simple_query(&sql).map_err(pe)? {
                if let postgres::SimpleQueryMessage::Row(r) = m {
                    let mut o = Map::new();
                    for (i, col) in r.columns().iter().enumerate() {
                        o.insert(col.name().to_string(), r.get(i).map_or(Value::Null, |s| json!(s)));
                    }
                    v.push(Value::Object(o));
                    if v.len() > max {
                        break;
                    }
                }
            }
            Ok(v)
        }
        Err(e) => Err(pe(e)),
    })?;
    t.rollback().map_err(pe)?;
    let mut v = shape(columns, rows, max);
    v["database"] = json!(db);
    v["user"] = json!(user);
    v["read_only_transaction"] = json!(true);
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_statement_only() {
        assert_eq!(single_statement("SELECT 1;").unwrap(), "SELECT 1");
        assert_eq!(single_statement("  SELECT 1 ; -- trailing\n").unwrap(), "SELECT 1");
        assert!(single_statement("SELECT 1; SELECT 2").unwrap_err().contains("got 2"));
        assert!(single_statement("SELECT 1; DELETE FROM t").is_err());
        assert!(single_statement(" ; ").is_err());
        for one in ["SELECT ';'", "SELECT 'a'';b'", "SELECT \"x;y\" FROM t", "SELECT $$a;b$$", "SELECT $f$ ; $f$",
            "SELECT 1 -- ; DROP TABLE t", "SELECT /* ; /* nested ; */ ; */ 1", "SELECT $1", "SELECT 1 /* x */;"] {
            assert!(single_statement(one).is_ok(), "{one}");
        }
        assert!(single_statement("SELECT $$a$$; SELECT 2").is_err());
    }

    #[test]
    fn select_style_only() {
        for ok in ["SELECT * FROM pgbx.status()", "with x as (select 1) select * from x", "TABLE pgbx.backups", "VALUES (1)",
            "SHOW data_directory", "EXPLAIN SELECT 1", "/* hi */ -- x\n select 1", "SELECT 'insert into t' AS s",
            "SELECT $$delete$$", "SELECT * FROM pgbx.overview()", "select count(*) from pgbx.history",
            "SELECT pg_size_pretty(pg_database_size('x'))", "SELECT * FROM pgbx.rowless_tables()"] {
            assert!(check_read_only(ok).is_ok(), "{ok}: {:?}", check_read_only(ok));
        }
        for bad in ["INSERT INTO t VALUES (1)", "UPDATE t SET a=1", "DELETE FROM t", "with d as (delete from t returning *) select * from d",
            "WITH x AS (INSERT INTO t VALUES (1) RETURNING 1) SELECT 1", "SELECT * INTO newt FROM t", "SELECT * FROM t FOR UPDATE",
            "SELECT * FROM t FOR SHARE", "SELECT * FROM t FOR NO KEY UPDATE", "EXPLAIN ANALYZE SELECT 1", "EXPLAIN (ANALYZE true) SELECT 1",
            "SELECT pg_terminate_backend(123)", "SELECT pg_catalog.set_config('a','b',false)", "SELECT nextval ('s')",
            "SELECT pg_advisory_xact_lock(1)", "SELECT lo_import('/etc/passwd')", "SELECT dblink_exec('x','y')",
            "SELECT pgbx.backup_now()", "SELECT \"pgbx\".\"restore\"('x')", "SELECT pgbx.download_url()", "SELECT txid_current()",
            "CREATE TABLE t (a int)", "DROP TABLE t", "VACUUM", "MERGE INTO t USING s ON true WHEN MATCHED THEN DELETE",
            "SET search_path = x", "COPY t TO '/tmp/x'", "CALL p()", "DO $$ begin end $$", "SELECT pg_switch_wal()"] {
            assert!(check_read_only(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("30").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert!(parse_duration("0").is_err() && parse_duration("soon").is_err());
    }

    #[test]
    fn shaping_caps_rows() {
        let cols = vec![json!({"name": "n", "type": "int4"})];
        let rows: Vec<Value> = (0..3).map(|n| json!({"n": n})).collect();
        let v = shape(cols.clone(), rows.clone(), 2);
        assert_eq!((v["row_count"].as_u64(), v["truncated"].as_bool()), (Some(2), Some(true)));
        let v = shape(cols, rows, 3);
        assert_eq!((v["row_count"].as_u64(), v["truncated"].as_bool(), &v["rows"][2]["n"]), (Some(3), Some(false), &json!(2)));
    }
}
