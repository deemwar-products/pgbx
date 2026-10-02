//! CLI entry points for point-in-time restore: wal-push, wal-get, `pgbx pitr ...` and `pgbx setup pitr`.

use crate::pitr::{self, RestoreReq};
use crate::wal::{self, Conf};
use crate::{one, s3restore, s3x, Args, Ctx, Out};
use serde_json::json;
use std::path::PathBuf;

/// The background push / prefetch processes are detached: once the archive_command or restore_command that started
/// them exits, they are reparented to PID 1, which in a container is often the postmaster. The postmaster takes an
/// unknown child that dies by a signal or with an exit code above 1 for a crashed backend and restarts the server, so
/// these processes must only ever end with exit code 0 or 1: SIGTERM / SIGINT / SIGHUP exit 1, a panic exits 1.
fn orphan_safe() {
    #[cfg(unix)]
    {
        extern "C" fn quit(_: libc::c_int) {
            unsafe { libc::_exit(1) };
        }
        let h = quit as extern "C" fn(libc::c_int) as libc::sighandler_t;
        unsafe {
            libc::signal(libc::SIGTERM, h);
            libc::signal(libc::SIGINT, h);
            libc::signal(libc::SIGHUP, h);
        }
    }
    std::panic::set_hook(Box::new(|_| std::process::exit(1)));
}

fn daemon<T>(f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|_| Err("panicked".into()))
}

/// archive_command: exit 0 = archived (or deliberately dropped under wal_queue_max), non-zero = Postgres retries.
pub fn wal_push_main(a: &Args) -> i32 {
    let conf = wal::conf_path(a.get("conf"));
    let r = wal::load_conf(&conf).and_then(|c| {
        let p = a.pos.first().ok_or("usage: pgbx wal-push <%p> [--conf FILE]")?;
        if a.has("async-daemon") {
            orphan_safe();
            daemon(|| wal::push_daemon(&c, p)).map(|_| String::new())
        } else {
            wal::wal_push(&c, p)
        }
    });
    match r {
        Ok(msg) => {
            if msg.starts_with("DROPPED") {
                eprintln!("pgbx wal-push: WARNING {msg}");
            }
            0
        }
        Err(e) => {
            eprintln!("pgbx wal-push: {e}");
            1
        }
    }
}

/// restore_command: exit 0 = delivered, 1 = not in the archive (normal end of WAL), 127 = hard error (Postgres stops
/// recovery with FATAL instead of treating it as the end of the archive and promoting too early).
pub fn wal_get_main(a: &Args) -> i32 {
    let conf = wal::conf_path(a.get("conf"));
    let (Some(name), Some(dest)) = (a.pos.first(), a.pos.get(1)) else {
        eprintln!("usage: pgbx wal-get <%f> <%p> --conf FILE");
        return 127;
    };
    let c = match wal::load_conf(&conf) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("pgbx wal-get: {e}");
            return 127;
        }
    };
    if a.has("prefetch-daemon") {
        orphan_safe();
        let _ = daemon(|| wal::prefetch_daemon(&c, name));
        return 0;
    }
    match wal::wal_get(&c, name, dest) {
        Ok(true) => 0,
        Ok(false) => 1,
        Err(e) => {
            eprintln!("pgbx wal-get: {e}");
            127
        }
    }
}

fn conf_from(a: &Args) -> Result<Conf, String> {
    if let Some(p) = a.get("conf") {
        let mut c = wal::load_conf(&PathBuf::from(p))?;
        if let Some(s) = a.get("system-id") {
            c.system_id = s.to_string();
        }
        return Ok(c);
    }
    let f = s3restore::s3_flags(a).map_err(|e| format!("{e} (or pass --conf <work_dir>/pgbx-wal.conf)"))?;
    Ok(Conf {
        s3: s3x::S3Conf { endpoint: f.endpoint, bucket: f.bucket, region: f.region, credentials_file: f.credentials_file },
        server: f.server,
        system_id: a.get("system-id").unwrap_or("").to_string(),
        process_max: 8,
        prefetch: 16,
        segment_size: 16 << 20,
        compress_level: 1,
        archive_timeout: 60,
        ..Default::default()
    })
}

pub fn pitr(cx: &mut Ctx) -> Out {
    let sub = cx.a.pos.first().cloned().unwrap_or_default();
    match sub.as_str() {
        "status" => {
            let admin = cx.admin_db();
            let mut c = cx.connect(&admin)?;
            let st = one(&mut c, "SELECT * FROM pgbx.pitr_status()", &[])?;
            Ok(json!({"ok": true, "pitr": st}))
        }
        "backup-now" => {
            let admin = cx.admin_db();
            let mut c = cx.connect(&admin)?;
            let id: i64 = c.query_one("SELECT pgbx.pitr_backup_now()", &[]).map_err(crate::pe)?.get(0);
            if cx.a.has("wait") {
                let t = cx.timeout();
                let r = crate::wait_job(&mut c, id, t)?;
                let ok = r["state"] == "done";
                return Ok(json!({"ok": ok, "job_id": id, "job": r}));
            }
            Ok(json!({"ok": true, "job_id": id, "queued": "base backup", "watch": "pgbx pitr status"}))
        }
        "list" => {
            let mut c = conf_from(&cx.a)?;
            let b = s3x::bucket(&c.s3)?;
            if c.system_id.is_empty() {
                c.system_id = pitr::find_system_id(&b, &c.server)?;
            }
            let root = wal::root(&c.server, &c.system_id);
            let bs = pitr::backups(&b, &root)?;
            let gs = pitr::gaps(&b, &root)?;
            Ok(json!({"ok": true, "system_id": c.system_id,
                "restorable_from": bs.first().map(|x| x.to_json()["stop_time"].clone()),
                "base_backups": bs.iter().rev().map(|x| x.to_json()).collect::<Vec<_>>(),
                "gaps": gs.iter().map(|g| json!({"id": g.id, "safe_until": g.safe_until.to_rfc3339(), "margin_s": g.margin_s,
                                                  "healed_at": g.end.map(|e| e.to_rfc3339())})).collect::<Vec<_>>()}))
        }
        "backup" => {
            let c = conf_from(&cx.a)?;
            pitr::base_backup(&c, cx.a.has("expire"))
        }
        "expire" => {
            let mut c = conf_from(&cx.a)?;
            let b = s3x::bucket(&c.s3)?;
            if c.system_id.is_empty() {
                c.system_id = pitr::find_system_id(&b, &c.server)?;
            }
            let (gone, n) = pitr::expire(&b, &wal::root(&c.server, &c.system_id), c.retention_days)?;
            Ok(json!({"ok": true, "expired_backups": gone, "expired_wal": n}))
        }
        "publish-gaps" => {
            let c = conf_from(&cx.a)?;
            let b = s3x::bucket(&c.s3)?;
            let sid = if c.system_id.is_empty() { pitr::find_system_id(&b, &c.server)? } else { c.system_id.clone() };
            let done = pitr::publish_gaps(&c, &b, &wal::root(&c.server, &sid))?;
            Ok(json!({"ok": true, "published": done}))
        }
        "restore" => {
            let t = cx.a.get("time").ok_or("--time TS (with UTC offset) or --time latest is required")?;
            let target = pitr::parse_target(t)?;
            let dir = cx.a.get("target").ok_or("--target DIR is required (an empty directory, or a stopped server's data directory with --yes-replace-whole-server)")?;
            let mut conf = conf_from(&cx.a)?;
            conf.process_max = conf.process_max.max(8);
            conf.prefetch = conf.prefetch.max(16);
            pitr::restore(RestoreReq { conf, target, dir: PathBuf::from(dir), replace: cx.a.has("yes-replace-whole-server") })
        }
        "" => Err("pgbx pitr status | list | backup-now | restore --time TS|latest --target DIR (see pgbx help)".into()),
        x => Err(format!("unknown pitr action '{x}' (status | list | backup-now | restore | backup | expire)")),
    }
}

/// Is `cmd` an archive_command written by `pgbx setup pitr` (any absolute pgbx path)?
pub fn is_pgbx_archive_command(cmd: &str) -> bool {
    let c = cmd.trim();
    let Some(bin) = c.strip_suffix(" wal-push %p").or_else(|| c.split_once(" wal-push %p --conf ").map(|(b, _)| b)) else { return false };
    let bin = bin.trim_matches('"');
    bin.starts_with('/') && (bin.ends_with("/pgbx") || bin.ends_with("/pgbx.exe")) && !bin.contains(['\'', ';', '&', '|'])
}

/// The archive_command `pgbx setup pitr` writes: this binary's absolute path.
/// A non-default pgbx.work_dir is passed as --conf (wal-push otherwise looks in <data_directory>/../pgbx).
pub fn our_archive_command(exe: &str, work_dir: Option<&str>) -> String {
    let bin = if exe.contains(' ') { format!("\"{exe}\"") } else { exe.to_string() };
    match work_dir.filter(|w| !w.is_empty()) {
        Some(w) => format!("{bin} wal-push %p --conf {}/pgbx-wal.conf", w.trim_end_matches('/')),
        None => format!("{bin} wal-push %p"),
    }
}

/// Decide what setup --pitr may do with the current archive_command.
pub fn archive_command_plan(current: &str) -> Result<bool, String> {
    let c = current.trim();
    if c.is_empty() || c == "(disabled)" {
        return Ok(true);
    }
    if is_pgbx_archive_command(c) {
        return Ok(true);
    }
    Err(format!(
        "refusing: archive_command is already set to something pgbx did not write ({c}). Another tool archives WAL \
         on this server; pgbx will not replace it. Remove it yourself (ALTER SYSTEM RESET archive_command) if it is \
         no longer used, then run pgbx setup pitr again"
    ))
}

/// `pgbx setup pitr --yes`: ALTER SYSTEM archive_mode=on, archive_command='<abs pgbx> wal-push %p', pgbx.pitr=on.
pub fn setup_pitr(cx: &mut Ctx) -> Out {
    let mut c = cx.connect("postgres")?;
    let r = c
        .query_one(
            "SELECT current_setting('archive_command'), current_setting('archive_mode'),
                    coalesce(current_setting('pgbx.pitr', true), 'off'),
                    (SELECT rolsuper FROM pg_roles WHERE rolname = current_user),
                    coalesce(current_setting('pgbx.work_dir', true), '')",
            &[],
        )
        .map_err(crate::pe)?;
    let (cur, mode, pitr_on, su, wd): (String, String, String, bool, String) = (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4));
    if wd.contains(char::is_whitespace) || wd.contains('\'') {
        return Err(format!("pgbx.work_dir '{wd}' contains spaces or quotes; pick a plain path"));
    }
    if !su {
        return Err("pgbx setup pitr needs a superuser connection (it runs ALTER SYSTEM)".into());
    }
    // with archive_mode=off SHOW says "(disabled)": judge the CONFIGURED value, so a foreign command is never clobbered
    let configured: Option<String> = c
        .query_one(
            "SELECT (SELECT setting FROM pg_file_settings WHERE name = 'archive_command' AND error IS NULL ORDER BY seqno DESC LIMIT 1)",
            &[],
        )
        .map_err(crate::pe)?
        .get(0);
    let cur = configured.unwrap_or(cur);
    archive_command_plan(&cur)?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?.display().to_string();
    let exe = std::env::var("PGBX_SERVER_BINARY").ok().filter(|s| !s.is_empty()).unwrap_or(exe);
    if !exe.starts_with('/') {
        return Err(format!("cannot work out an absolute path for pgbx ({exe}); set PGBX_SERVER_BINARY=/usr/local/bin/pgbx"));
    }
    let want = our_archive_command(&exe, Some(&wd));
    let plan = json!({"archive_mode": "on", "archive_command": want, "pgbx.pitr": "on"});
    if !cx.a.has("yes") {
        return Ok(json!({"ok": false, "error": "guarded: review the plan, then add --yes", "plan": plan,
            "current": {"archive_mode": mode, "archive_command": cur, "pgbx.pitr": pitr_on}}));
    }
    let q = |s: &str| s.replace('\'', "''");
    // ALTER SYSTEM cannot run in a (multi-statement implicit) transaction: one statement each
    for stmt in [
        "ALTER SYSTEM SET archive_mode = 'on'".to_string(),
        format!("ALTER SYSTEM SET archive_command = '{}'", q(&want)),
        "ALTER SYSTEM SET pgbx.pitr = 'on'".to_string(),
        "SELECT pg_reload_conf()".to_string(),
    ] {
        c.batch_execute(&stmt).map_err(crate::pe)?;
    }
    let restart = mode == "off";
    Ok(json!({"ok": true, "written": plan, "restart_needed": restart,
        "next": if restart { "restart Postgres once (archive_mode needs it), e.g. pg_ctl -D <data dir> restart or systemctl restart postgresql; the first base backup starts right after" }
                else { "archive_mode was already on: nothing to restart; the first base backup starts within a minute" }}))
}

#[cfg(test)]
mod t {
    use super::*;

    #[test]
    fn archive_command_ownership() {
        assert!(is_pgbx_archive_command("/usr/local/bin/pgbx wal-push %p"));
        assert!(is_pgbx_archive_command("\"/opt/my tools/pgbx\" wal-push %p"));
        assert!(is_pgbx_archive_command("/usr/local/bin/pgbx wal-push %p --conf /var/lib/postgresql/pgbx/pgbx-wal.conf"));
        assert!(!is_pgbx_archive_command("pgbx wal-push %p"));
        assert!(!is_pgbx_archive_command("cp %p /mnt/archive/%f"));
        assert!(!is_pgbx_archive_command("/usr/local/bin/pgbx wal-push %p; rm -rf /"));
        assert!(archive_command_plan("").unwrap());
        assert!(archive_command_plan("(disabled)").unwrap());
        assert!(archive_command_plan("/usr/bin/pgbx wal-push %p").unwrap());
        assert!(archive_command_plan("pgbackrest --stanza=main archive-push %p").unwrap_err().contains("will not replace"));
        assert_eq!(our_archive_command("/usr/local/bin/pgbx", None), "/usr/local/bin/pgbx wal-push %p");
        let c = our_archive_command("/usr/local/bin/pgbx", Some("/srv/pgbx/"));
        assert_eq!(c, "/usr/local/bin/pgbx wal-push %p --conf /srv/pgbx/pgbx-wal.conf");
        assert!(is_pgbx_archive_command(&c));
    }
}
