//! Roles/globals captured with every backup, and applied idempotently on restore.
//! Shared by the extension (worker restore) and the CLI (`db-restore --from-s3 --with-roles`); no pgrx here.
//!
//! Backup: `pg_dumpall --globals-only [--no-role-passwords]` is streamed through zstd (and encryption when
//! on) to <ts>.globals.sql.zst next to <ts>.dump. Its first line records which roles the database
//! references (owners, ACL grantees, and the roles those are members of):
//!   -- pgbx-referenced-roles: ["app","app_ro"]
//!
//! Restore (`with_roles`): before pg_restore, statements are replayed so a missing role is created and an
//! existing one is NEVER changed:
//!   CREATE ROLE x           -> only if x does not exist (else reported as "existing, left as is")
//!   ALTER ROLE / COMMENT ON ROLE / ALTER ROLE x SET -> only for roles created by this restore
//!   GRANT a TO b            -> only when b was created by this restore
//!   tablespaces, \connect, \restrict, SET -> skipped (server-specific or session-only)
//! Scope 'referenced' (default) limits this to the roles the database uses; 'all' replays every role.

// Also compiled into the CLI (edition 2021): no let-chains, so nested `if`s stay as they are.
#![allow(clippy::collapsible_if)]

use std::collections::{BTreeSet, HashSet};

pub const REF_MARK: &str = "-- pgbx-referenced-roles: ";

/// Roles a database references: object owners, ACL grantees, default-ACL roles, and (recursively) the
/// roles those are members of. Run in the database being backed up.
pub const REFERENCED_ROLES_SQL: &str = r#"
WITH RECURSIVE direct(oid) AS (
    SELECT datdba FROM pg_database WHERE datname = current_database()
    UNION SELECT (aclexplode(datacl)).grantee FROM pg_database WHERE datname = current_database() AND datacl IS NOT NULL
    UNION SELECT nspowner FROM pg_namespace WHERE nspname NOT LIKE 'pg\_%' AND nspname <> 'information_schema'
    UNION SELECT (aclexplode(nspacl)).grantee FROM pg_namespace WHERE nspacl IS NOT NULL AND nspname NOT LIKE 'pg\_%' AND nspname <> 'information_schema'
    UNION SELECT c.relowner FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname NOT LIKE 'pg\_%' AND n.nspname <> 'information_schema'
    UNION SELECT (aclexplode(c.relacl)).grantee FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE c.relacl IS NOT NULL AND n.nspname NOT LIKE 'pg\_%' AND n.nspname <> 'information_schema'
    UNION SELECT p.proowner FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace WHERE n.nspname NOT LIKE 'pg\_%' AND n.nspname <> 'information_schema'
    UNION SELECT (aclexplode(p.proacl)).grantee FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace WHERE p.proacl IS NOT NULL AND n.nspname NOT LIKE 'pg\_%' AND n.nspname <> 'information_schema'
    UNION SELECT t.typowner FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace WHERE n.nspname NOT LIKE 'pg\_%' AND n.nspname <> 'information_schema'
    UNION SELECT defaclrole FROM pg_default_acl
    UNION SELECT (aclexplode(defaclacl)).grantee FROM pg_default_acl
    UNION SELECT s.setrole FROM pg_db_role_setting s JOIN pg_database d ON d.oid = s.setdatabase WHERE d.datname = current_database()
), closure(oid) AS (
    SELECT oid FROM direct WHERE oid <> 0
    UNION SELECT m.roleid FROM pg_auth_members m JOIN closure c ON m.member = c.oid
)
SELECT coalesce(json_agg(DISTINCT r.rolname ORDER BY r.rolname), '[]')::text
FROM closure c JOIN pg_roles r ON r.oid = c.oid WHERE r.rolname NOT LIKE 'pg\_%'
"#;

/// Split SQL text into statements on `;` outside quotes; comment lines (`--`) and psql meta lines (`\...`)
/// are returned as their own items so the caller can skip them.
pub fn split(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let (mut sq, mut dq) = (false, false);
    for line in sql.lines() {
        if cur.is_empty() && !sq && !dq {
            let t = line.trim_start();
            if t.is_empty() {
                continue;
            }
            if t.starts_with("--") || t.starts_with('\\') {
                out.push(t.to_string());
                continue;
            }
        }
        for ch in line.chars() {
            cur.push(ch);
            match ch {
                '\'' if !dq => sq = !sq,
                '"' if !sq => dq = !dq,
                ';' if !sq && !dq => {
                    out.push(cur.trim().to_string());
                    cur.clear();
                }
                _ => {}
            }
        }
        if !cur.is_empty() {
            cur.push('\n');
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// Read one identifier (bare or "quoted") at the start of `s`; returns (name, rest).
fn ident(s: &str) -> Option<(String, &str)> {
    let s = s.trim_start();
    if let Some(r) = s.strip_prefix('"') {
        let mut name = String::new();
        let mut it = r.char_indices().peekable();
        while let Some((i, c)) = it.next() {
            if c == '"' {
                if let Some((_, '"')) = it.peek() {
                    name.push('"');
                    it.next();
                    continue;
                }
                return Some((name, &r[i + 1..]));
            }
            name.push(c);
        }
        None
    } else {
        let end = s.find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$')).unwrap_or(s.len());
        (end > 0).then(|| (s[..end].to_lowercase(), &s[end..]))
    }
}

fn after_kw<'a>(s: &'a str, kws: &[&str]) -> Option<&'a str> {
    let mut rest = s.trim_start();
    for k in kws {
        let n = k.len();
        if rest.len() < n || !rest[..n].eq_ignore_ascii_case(k) {
            return None;
        }
        rest = rest[n..].trim_start();
    }
    Some(rest)
}

#[derive(Debug, PartialEq)]
pub enum Kind {
    CreateRole(String),
    RoleAttr(String), // ALTER ROLE x ..., COMMENT ON ROLE x, SECURITY LABEL ... ON ROLE x
    Grant { member: String },
    Skip(&'static str),
    Other,
}

pub fn classify(st: &str) -> Kind {
    let t = st.trim();
    if t.starts_with("--") {
        return Kind::Skip("comment");
    }
    if t.starts_with('\\') {
        return Kind::Skip("psql meta-command");
    }
    if after_kw(t, &["SET"]).is_some() || after_kw(t, &["SELECT"]).is_some() {
        return Kind::Skip("session setting");
    }
    if let Some(r) = after_kw(t, &["CREATE", "ROLE"]) {
        if let Some((n, _)) = ident(r) {
            return Kind::CreateRole(n);
        }
    }
    if let Some(r) = after_kw(t, &["ALTER", "ROLE"]).or_else(|| after_kw(t, &["COMMENT", "ON", "ROLE"])) {
        if let Some((n, _)) = ident(r) {
            return Kind::RoleAttr(n);
        }
    }
    if t.to_ascii_uppercase().contains("TABLESPACE") {
        return Kind::Skip("tablespace (server-specific path)");
    }
    if let Some(r) = after_kw(t, &["GRANT"]) {
        // GRANT a[, b] TO member [WITH ...] [GRANTED BY x]
        let up = r.to_ascii_uppercase();
        if let Some(i) = up.find(" TO ") {
            if !up[..i].contains(" ON ") {
                if let Some((m, _)) = ident(&r[i + 4..]) {
                    return Kind::Grant { member: m };
                }
            }
        }
    }
    if after_kw(t, &["SECURITY", "LABEL"]).is_some() {
        if let Some(i) = t.to_ascii_uppercase().find(" ON ROLE ") {
            if let Some((n, _)) = ident(&t[i + 9..]) {
                return Kind::RoleAttr(n);
            }
        }
    }
    Kind::Other
}

/// The referenced-roles header line, if present.
pub fn referenced(sql: &str) -> Option<BTreeSet<String>> {
    let line = sql.lines().find(|l| l.starts_with(REF_MARK))?;
    // a JSON array of strings: read each "..." with \" and \\ escapes
    let mut out = BTreeSet::new();
    let mut it = line[REF_MARK.len()..].chars();
    while let Some(c) = it.next() {
        if c != '"' {
            continue;
        }
        let mut s = String::new();
        while let Some(c) = it.next() {
            match c {
                '\\' => s.push(it.next()?),
                '"' => break,
                c => s.push(c),
            }
        }
        out.insert(s);
    }
    Some(out)
}

pub fn header_line(roles_json: &str) -> String {
    format!("{REF_MARK}{roles_json}\n")
}

#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    pub run: Vec<String>,
    pub created: Vec<String>,
    pub existing: Vec<String>,
    pub out_of_scope: Vec<String>,
    pub skipped: Vec<String>, // short notes ("tablespace ...")
}

/// Decide which statements to run. `existing` = roles already on the target; `only` = scope filter.
pub fn plan(sql: &str, existing: &HashSet<String>, only: Option<&BTreeSet<String>>) -> Plan {
    let mut p = Plan::default();
    let mut created: HashSet<String> = HashSet::new();
    let in_scope = |n: &str| only.is_none_or(|s| s.contains(n));
    for st in split(sql) {
        match classify(&st) {
            Kind::CreateRole(n) => {
                if existing.contains(&n) {
                    p.existing.push(n);
                } else if !in_scope(&n) {
                    p.out_of_scope.push(n);
                } else {
                    created.insert(n.clone());
                    p.created.push(n);
                    p.run.push(st);
                }
            }
            Kind::RoleAttr(n) => {
                if created.contains(&n) {
                    p.run.push(st);
                }
            }
            Kind::Grant { member } => {
                if created.contains(&member) {
                    p.run.push(st);
                }
            }
            Kind::Skip(why) => {
                if why.starts_with("tablespace") && !p.skipped.iter().any(|s| s.starts_with("tablespace")) {
                    p.skipped.push("tablespaces (create them by hand on the new server if needed)".into());
                }
            }
            Kind::Other => p.skipped.push(st.chars().take(80).collect()),
        }
    }
    p
}

/// Apply a plan on `admin` (any database, as a role that may CREATE ROLE). Each statement runs on its own;
/// failures are reported, not fatal (e.g. GRANT of a role that was out of scope). Returns a JSON report.
pub fn apply(admin: &mut postgres::Client, sql: &str, scope: &str) -> Result<String, String> {
    let existing: HashSet<String> = admin
        .query("SELECT rolname FROM pg_roles", &[])
        .map_err(|e| format!("list roles: {e}"))?
        .iter()
        .map(|r| r.get::<_, String>(0))
        .collect();
    let only = match scope {
        "all" => None,
        _ => Some(referenced(sql).ok_or("this backup's roles file has no referenced-roles line; use roles scope 'all'")?),
    };
    let p = plan(sql, &existing, only.as_ref());
    let mut failed = Vec::new();
    for st in &p.run {
        if let Err(e) = admin.batch_execute(st) {
            let msg = e.as_db_error().map(|d| d.message().to_string()).unwrap_or_else(|| e.to_string());
            failed.push(format!("{}: {msg}", st.chars().take(60).collect::<String>()));
        }
    }
    let j = |v: &[String]| format!("[{}]", v.iter().map(|s| jstr(s)).collect::<Vec<_>>().join(","));
    Ok(format!(
        "{{\"scope\":{},\"created\":{},\"existing\":{},\"out_of_scope\":{},\"skipped\":{},\"failed\":{}}}",
        jstr(scope), j(&p.created), j(&p.existing), j(&p.out_of_scope), j(&p.skipped), j(&failed)
    ))
}

pub fn jstr(s: &str) -> String {
    let mut o = String::from("\"");
    for ch in s.chars() {
        match ch {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// S3 key of the globals file that belongs to a dump key: <ts>.dump -> <ts>.globals.sql.zst
pub fn globals_key(dump_key: &str) -> String {
    format!("{}.globals.sql.zst", dump_key.strip_suffix(".dump").unwrap_or(dump_key))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUMPALL: &str = r#"--
-- PostgreSQL database cluster dump
--

\restrict abc123

SET default_transaction_read_only = off;
SET client_encoding = 'UTF8';

--
-- Roles
--

CREATE ROLE app;
ALTER ROLE app WITH NOSUPERUSER INHERIT NOCREATEROLE NOCREATEDB LOGIN NOREPLICATION NOBYPASSRLS;
CREATE ROLE "Odd ""Name""";
ALTER ROLE "Odd ""Name""" WITH NOLOGIN;
CREATE ROLE postgres;
ALTER ROLE postgres WITH SUPERUSER INHERIT CREATEROLE CREATEDB LOGIN REPLICATION BYPASSRLS;
CREATE ROLE other_tenant;
ALTER ROLE other_tenant WITH LOGIN;
COMMENT ON ROLE app IS 'the app; it''s ours';
ALTER ROLE app SET search_path TO 'app', 'public';

--
-- Role memberships
--

GRANT app TO "Odd ""Name""" WITH INHERIT TRUE GRANTED BY postgres;
GRANT pg_read_all_data TO app;
GRANT app TO postgres;

CREATE TABLESPACE fast OWNER postgres LOCATION '/mnt/fast';

\unrestrict abc123
"#;

    #[test]
    fn splits_and_classifies() {
        let s = split("CREATE ROLE a;\nCOMMENT ON ROLE a IS 'x;\ny';\n-- c\n\\connect x\n");
        assert_eq!(s, vec!["CREATE ROLE a;", "COMMENT ON ROLE a IS 'x;\ny';", "-- c", "\\connect x"]);
        assert_eq!(classify("CREATE ROLE \"Odd \"\"Name\"\"\";"), Kind::CreateRole("Odd \"Name\"".into()));
        assert_eq!(classify("create role App;"), Kind::CreateRole("app".into()));
        assert_eq!(classify("ALTER ROLE app SET x = 1;"), Kind::RoleAttr("app".into()));
        assert_eq!(classify("GRANT app TO bob WITH INHERIT TRUE GRANTED BY postgres;"), Kind::Grant { member: "bob".into() });
        assert_eq!(classify("GRANT ALL ON TABLESPACE fast TO bob;"), Kind::Skip("tablespace (server-specific path)"));
        assert!(matches!(classify("SET client_encoding = 'UTF8';"), Kind::Skip(_)));
    }

    #[test]
    fn plan_creates_missing_and_never_touches_existing() {
        let existing: HashSet<String> = ["postgres", "pg_read_all_data"].iter().map(|s| s.to_string()).collect();
        let p = plan(DUMPALL, &existing, None);
        assert_eq!(p.created, vec!["app", "Odd \"Name\"", "other_tenant"]);
        assert_eq!(p.existing, vec!["postgres"]);
        assert!(p.run.iter().all(|s| !s.contains("ALTER ROLE postgres")), "existing role untouched");
        assert!(p.run.iter().any(|s| s.starts_with("GRANT pg_read_all_data TO app")));
        assert!(p.run.iter().all(|s| s != "GRANT app TO postgres;"), "membership of an existing role untouched");
        assert!(p.run.iter().any(|s| s.starts_with("COMMENT ON ROLE app IS 'the app; it''s ours'")));
        assert!(p.run.iter().all(|s| !s.contains("TABLESPACE") && !s.starts_with('\\') && !s.starts_with("SET")));
        assert!(p.skipped.iter().any(|s| s.starts_with("tablespaces")));
        // idempotent: a second run with everything existing does nothing
        let all: HashSet<String> = existing.iter().cloned().chain(p.created.iter().cloned()).collect();
        let again = plan(DUMPALL, &all, None);
        assert!(again.run.is_empty() && again.created.is_empty());
    }

    #[test]
    fn referenced_scope() {
        let sql = format!("{}{DUMPALL}", header_line(r#"["Odd \"Name\"","app"]"#));
        let only = referenced(&sql).unwrap();
        assert!(only.contains("app") && only.contains("Odd \"Name\""));
        let p = plan(&sql, &HashSet::new(), Some(&only));
        assert_eq!(p.created, vec!["app", "Odd \"Name\""]);
        assert!(p.out_of_scope.contains(&"other_tenant".to_string()));
        assert!(p.run.iter().all(|s| !s.contains("other_tenant")));
        assert_eq!(referenced(DUMPALL), None);
        assert_eq!(referenced(&header_line("[]")).unwrap().len(), 0);
    }

    #[test]
    fn globals_key_next_to_dump() {
        assert_eq!(globals_key("srv/shop/2026-01-01T02-00-00Z.dump"), "srv/shop/2026-01-01T02-00-00Z.globals.sql.zst");
    }
}
