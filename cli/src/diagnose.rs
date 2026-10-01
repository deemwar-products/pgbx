//! `pgbx diagnose`: why is Postgres down (or struggling)? Reads, never changes anything.
//!
//! Gathers evidence from every source it can reach (each optional): postmaster.pid + process liveness,
//! `pg_ctl status`, the Postgres log (log_directory, journald, or `--log FILE`), the kernel log (OOM kills),
//! `df`/sizes of the data dir, pg_wal, archive_status, pgsql_tmp, log dir, data dir
//! owner/mode. A pattern table turns log lines into a probable cause; each cause maps to printed steps with a
//! safety tier (readonly / safe / guarded / destructive). pgbx never runs the steps itself.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

// ------------------------------------------------------------------------------------------- pattern table

/// Ordered: the first cause whose pattern matches a line wins for that line; across lines the highest priority
/// (lowest index in CAUSE_PRIORITY) wins.
pub const CAUSE_PRIORITY: &[&str] = &[
    "corruption", "disk_full", "oom_kill", "permissions", "stale_pid", "config_error",
    "too_many_connections", "crash", "wal_backlog", "unknown",
];

/// Cause for one log line, if any. `kernel` lines only count for OOM kills of postgres.
pub fn classify_line(line: &str) -> Option<&'static str> {
    let l = line.to_ascii_lowercase();
    let has = |p: &str| l.contains(p);
    if (has("out of memory: kill") || has("oom-kill") || has("killed process")) && has("postgres") {
        return Some("oom_kill");
    }
    if has("no space left on device") || has("could not extend file") && has("space") {
        return Some("disk_full");
    }
    // SQL privilege errors ("permission denied for table x") are not why a server is down
    let sql_priv = has("permission denied for ") || has("permission denied to ");
    if has("data directory") && has("invalid permissions")
        || has("permission denied") && !sql_priv && (has("data directory") || has("\"/") || data_path(&l))
    {
        return Some("permissions");
    }
    if has("invalid page in block") || has("checksum verification failed") || has("could not read block")
        || has("invalid memory alloc request") || has("could not open file") && data_path(&l) || has("missing chunk number")
        || has("invalid resource manager id") && !has("redo done")
    {
        return Some("corruption");
    }
    if has("lock file") && has("postmaster.pid") && has("already exists") || has("stale lock file") {
        return Some("stale_pid");
    }
    if has("could not bind") || has("address already in use") || has("syntax error in file")
        || has("configuration file") && has("contains errors") || has("invalid value for parameter")
        || has("unrecognized configuration parameter") || has("could not create listen socket")
        || has("could not load library")
    {
        return Some("config_error");
    }
    if has("too many connections") || has("sorry, too many clients") || has("remaining connection slots are reserved") {
        return Some("too_many_connections");
    }
    if has("was terminated by signal 9") {
        return Some("oom_kill"); // SIGKILL of a backend is almost always the OOM killer
    }
    if has("panic:") || has("was terminated by signal") || has("terminating any other active server processes") {
        return Some("crash");
    }
    if has("archive command failed") || has("archiving write-ahead log file") && has("failed") {
        return Some("wal_backlog");
    }
    None
}

/// A quoted path inside the data directory: "base/…", "global/…", "pg_…" (or an absolute one ending in them).
fn data_path(l: &str) -> bool {
    ["\"base/", "\"global/", "\"pg_", "/base/", "/global/", "/pg_wal/", "/pg_xact/"].iter().any(|p| l.contains(p))
}

/// Lines from the last postmaster start / ready / shut-down marker on (the current life of the server).
/// Older lines describe a previous incident. No marker: everything counts.
pub fn since_last_start(lines: &[String]) -> &[String] {
    let marker = |l: &str| {
        let l = l.to_ascii_lowercase();
        l.contains("starting postgresql") || l.contains("database system is ready to accept connections")
            || l.contains("database system is shut down") || l.contains("database system was shut down")
            || l.contains("database system was interrupted")
    };
    match lines.iter().rposition(|l| marker(l)) {
        Some(i) => &lines[i..],
        None => lines,
    }
}

/// Timestamp (epoch seconds) of a kernel log line: `journalctl -o short-unix` ("1727773200.123 host kernel: …")
/// or dmesg ("[ 4242.123] …", seconds since boot -> needs `boot_epoch`).
pub fn kernel_ts(line: &str, boot_epoch: Option<f64>) -> Option<f64> {
    let t = line.trim_start();
    if let Some(r) = t.strip_prefix('[') {
        let secs: f64 = r[..r.find(']')?].trim().parse().ok()?;
        return boot_epoch.map(|b| b + secs);
    }
    let first = t.split_whitespace().next()?;
    first.parse::<f64>().ok().filter(|v| *v > 1e9)
}

/// Kernel OOM-kill lines for postgres, minus those older than the newest postmaster start (a previous incident).
pub fn kernel_recent(text: &str, start_epoch: Option<f64>, boot_epoch: Option<f64>) -> Vec<String> {
    text.lines()
        .filter(|l| classify_line(l) == Some("oom_kill"))
        .filter(|l| match (start_epoch, kernel_ts(l, boot_epoch)) {
            (Some(s), Some(t)) => t >= s,
            _ => true,
        })
        .map(String::from)
        .collect()
}

/// May old files in this log directory be offered for cleaning? Never when it IS the data directory or looks
/// like one (contains pg_wal / base / global).
pub fn log_dir_cleanable(log_dir: &Path, pgdata: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let (l, d) = (canon(log_dir), canon(pgdata));
    l != d && !d.starts_with(&l) && !["pg_wal", "base", "global", "PG_VERSION"].iter().any(|x| l.join(x).exists())
}

/// Lifecycle hint from a log line (newest wins): starting / recovering / shutting_down.
pub fn state_hint(line: &str) -> Option<&'static str> {
    let l = line.to_ascii_lowercase();
    if l.contains("database system is shutting down") || l.contains("received fast shutdown") || l.contains("received smart shutdown") {
        Some("shutting_down")
    } else if l.contains("redo starts") || l.contains("in recovery") || l.contains("automatic recovery in progress") {
        Some("recovering")
    } else if l.contains("database system is starting up") || l.contains("starting postgresql") {
        Some("starting")
    } else if l.contains("database system is ready to accept connections") || l.contains("database system is shut down") {
        Some("settled")
    } else {
        None
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Evidence {
    pub source: String,
    pub line: String,
}

/// Probable cause from evidence lines: (cause, the lines that support it — at most 8, newest last).
pub fn classify(lines: &[Evidence]) -> (String, Vec<Evidence>) {
    let mut best: Option<usize> = None;
    for e in lines {
        if let Some(c) = classify_line(&e.line) {
            let i = CAUSE_PRIORITY.iter().position(|x| *x == c).unwrap();
            if best.is_none_or(|b| i < b) {
                best = Some(i);
            }
        }
    }
    let Some(b) = best else { return ("unknown".into(), vec![]) };
    let cause = CAUSE_PRIORITY[b];
    let mut ev: Vec<Evidence> = lines.iter().filter(|e| classify_line(&e.line) == Some(cause)).cloned().collect();
    let skip = ev.len().saturating_sub(8);
    ev.drain(..skip);
    (cause.into(), ev)
}

// ------------------------------------------------------------------------------------------- facts + steps

/// A directory/file that takes space, and whether removing (some of) it is safe.
#[derive(Debug, Clone)]
pub struct Culprit {
    pub path: String,
    pub bytes: u64,
    pub clean: &'static str, // "safe" | "safe_when_stopped" | "never"
    pub note: &'static str,
}

#[derive(Debug, Clone, Default)]
pub struct Facts {
    pub pgdata: String,
    pub disk_pct: Option<u64>,     // use% of the data dir's filesystem
    pub wal_disk_pct: Option<u64>, // use% of pg_wal's filesystem (may be a separate mount)
    pub pg_wal_bytes: u64,
    pub ready_count: u64,
    pub oldest_ready_secs: Option<u64>,
    pub culprits: Vec<Culprit>,
    pub pid_alive: Option<bool>, // None = no postmaster.pid
    pub log_dir: Option<String>,
}

/// pg_wal piling up (an inactive replication slot, a failing archive_command of some other tool, a huge
/// max_wal_size): many .ready files, an old one, or pg_wal is most of the space.
pub fn wal_backlog(f: &Facts) -> bool {
    let biggest = f.culprits.iter().map(|c| c.bytes).max().unwrap_or(0);
    f.ready_count >= 64 || f.oldest_ready_secs.is_some_and(|s| s > 3600) || (f.pg_wal_bytes > 1 << 30 && f.pg_wal_bytes >= biggest)
}

/// Refine the log-based cause with facts: a full disk whose space is mostly pg_wal is a WAL backlog.
pub fn refine(cause: &str, f: &Facts) -> String {
    let full = f.disk_pct.is_some_and(|p| p >= 98) || f.wal_disk_pct.is_some_and(|p| p >= 98);
    match cause {
        "disk_full" | "unknown" | "crash" if full && wal_backlog(f) => "wal_backlog".into(),
        "unknown" | "crash" if full => "disk_full".into(),
        "unknown" if f.pid_alive == Some(false) => "stale_pid".into(),
        c => c.into(),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub tier: &'static str, // readonly | safe | guarded | destructive
    pub why: String,
    pub command: String,
}

fn st(tier: &'static str, why: impl Into<String>, command: impl Into<String>) -> Step {
    Step { tier, why: why.into(), command: command.into() }
}

pub const NEVER_DELETE: &str = "NEVER delete anything in pg_wal/, base/, global/, pg_xact/ (or any other file inside the \
    data directory) by hand: Postgres cannot start without them and the data is lost for good";

/// Printed remedies for a cause. Destructive ones say "needs human approval".
pub fn steps_for(cause: &str, f: &Facts) -> Vec<Step> {
    let d = if f.pgdata.is_empty() { "<pgdata>" } else { f.pgdata.as_str() };
    let start = format!("pg_ctl -D {d} start   (or: systemctl start postgresql / docker start <container>)");
    let mut v = vec![];
    match cause {
        "oom_kill" => {
            v.push(st("readonly", "confirm the kernel killed postgres for memory", "journalctl -k -n 500 | grep -i -E 'out of memory|killed process'"));
            v.push(st("readonly", "see what memory Postgres is allowed", "grep -E '^(shared_buffers|work_mem|maintenance_work_mem|max_connections)' $PGDATA/postgresql*.conf"));
            v.push(st("safe", "Postgres recovers by itself on start (crash recovery replays WAL)", start.clone()));
            v.push(st("guarded", "lower memory use so it does not happen again (needs a restart to apply shared_buffers)",
                "ALTER SYSTEM SET work_mem = '16MB'; ALTER SYSTEM SET max_connections = 100;  -- size to the machine, then restart"));
            v.push(st("guarded", "or give the machine/container more RAM, and avoid swapless overcommit", "vm.overcommit_memory=2 (sysctl) / raise the container memory limit"));
        }
        "disk_full" | "wal_backlog" => {
            v.push(st("readonly", "see which filesystem is full", format!("df -h {d} {d}/pg_wal")));
            v.push(st("readonly", "see the biggest directories", format!("du -sh {d}/pg_wal {d}/base {d}/base/pgsql_tmp {d}/log")));
            for c in &f.culprits {
                let tier = match c.clean { "safe" => "safe", "safe_when_stopped" => "guarded", _ => continue };
                v.push(st(tier, format!("{} is {} — {}", c.path, human(c.bytes), c.note), clean_cmd(c)));
            }
            if cause == "wal_backlog" {
                v.push(st("readonly", "find what holds WAL: an inactive replication slot, WAL waiting for an archive_command \
                    (pgbx needs none; another tool may have set it), or a large max_wal_size / wal_keep_size",
                    format!("SELECT slot_name, active, wal_status FROM pg_replication_slots; SHOW archive_command; SHOW max_wal_size; \
                    SHOW wal_keep_size;  ls {d}/pg_wal/archive_status | grep -c ready")));
                v.push(st("guarded", "cap how much WAL a slot may hold so a dead consumer cannot fill the disk",
                    "ALTER SYSTEM SET max_slot_wal_keep_size = '10GB'; SELECT pg_reload_conf();"));
                v.push(st("destructive", "an inactive replication slot can pin WAL: drop it only if its consumer is gone \
                    for good (needs human approval: the slot cannot be recreated at the same position)",
                    "SELECT slot_name, active, wal_status FROM pg_replication_slots;  SELECT pg_drop_replication_slot('<name>');"));
                v.push(st("destructive", "if an archive_command you no longer need keeps failing, turn it off (needs human \
                    approval: whatever relied on that WAL archive gets a gap)",
                    "ALTER SYSTEM RESET archive_command; SELECT pg_reload_conf();"));
            }
            v.push(st("safe", "or grow the volume (no data touched)", "resize the disk / volume, then start Postgres"));
            v.push(st("readonly", NEVER_DELETE, "-"));
            v.push(st("safe", "start Postgres once space is free", start.clone()));
        }
        "corruption" => {
            v.push(st("readonly", "do NOT repair in place: no pg_resetwal, no zero_damaged_pages, no deleting files — \
                any of these can destroy what is still recoverable", "-"));
            v.push(st("safe", "restore each database's newest backup into a NEW database (or onto a new server) and compare",
                "pgbx db-restore --db <name> --into <name>_check --wait   # new server: pgbx db-restore --from-s3 --db <name> --into <name> ..."));
            v.push(st("safe", "take a file-level copy of the damaged data dir before anyone touches it",
                format!("cp -a {d} {d}.damaged-$(date +%F)   # with Postgres stopped")));
        }
        "permissions" => {
            v.push(st("readonly", "check owner and mode", format!("ls -ld {d}; stat -c '%U %a' {d}")));
            v.push(st("guarded", "the data dir must be owned by the postgres OS user, mode 0700 (or 0750)",
                format!("chown -R postgres:postgres {d} && chmod 700 {d}")));
            v.push(st("safe", "then start Postgres", start.clone()));
        }
        "stale_pid" => {
            v.push(st("readonly", "make sure no postmaster is really running", format!("head -1 {d}/postmaster.pid; ps -p $(head -1 {d}/postmaster.pid)")));
            v.push(st("guarded", "only if that PID is NOT a running postgres: remove the stale lock file (the only file \
                inside the data dir that may be removed by hand)", format!("rm {d}/postmaster.pid")));
            v.push(st("safe", "then start Postgres", start.clone()));
        }
        "config_error" => {
            v.push(st("readonly", "the log names the file, line and parameter", "pgbx diagnose (evidence above)"));
            v.push(st("readonly", "check postgresql.auto.conf too (ALTER SYSTEM writes there)", format!("cat {d}/postgresql.auto.conf")));
            v.push(st("guarded", "fix or comment out the bad line; if the port is taken, find who holds it",
                "ss -ltnp | grep 5432"));
            v.push(st("safe", "then start Postgres", start.clone()));
        }
        "too_many_connections" => {
            v.push(st("readonly", "see who holds connections", "SELECT usename, application_name, state, count(*) FROM pg_stat_activity GROUP BY 1,2,3 ORDER BY 4 DESC;"));
            v.push(st("guarded", "end idle sessions of a misbehaving app",
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE state = 'idle' AND state_change < now() - interval '10 min';"));
            v.push(st("safe", "put a pooler (pgbouncer) in front, or raise max_connections with care (memory)", "-"));
        }
        "crash" => {
            v.push(st("readonly", "read the lines before the PANIC / signal", "pgbx diagnose --json (evidence)"));
            v.push(st("safe", "Postgres runs crash recovery on start", start.clone()));
            v.push(st("readonly", "if it crashes again on start, treat it as possible corruption: restore the databases onto a new server",
                "pgbx db-restore --from-s3 --db <name> --into <name> --s3-endpoint ... (see pgbx help)"));
        }
        _ => {
            v.push(st("readonly", "no known pattern in the logs we could read; pass the log explicitly",
                "pgbx diagnose --log /path/to/postgres.log --pgdata <dir>"));
            v.push(st("readonly", "or look yourself", "journalctl -u 'postgresql*' -n 200; journalctl -k -n 200; docker logs <container> --tail 200"));
            v.push(st("safe", "try to start it and read the error", start.clone()));
        }
    }
    v
}

fn clean_cmd(c: &Culprit) -> String {
    if c.path.ends_with("pgsql_tmp") {
        format!("# only while Postgres is STOPPED: rm -rf {}/*", c.path)
    } else {
        format!("find {} -name '*.log*' -mtime +7 -delete   # old logs only", c.path)
    }
}

pub fn human(b: u64) -> String {
    let u = ["B", "kB", "MB", "GB", "TB"];
    let (mut v, mut i) = (b as f64, 0);
    while v >= 1024.0 && i < 4 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{b} B") } else { format!("{v:.1} {}", u[i]) }
}

// ------------------------------------------------------------------------------------------- gathering

fn out(cmd: &str, args: &[&str]) -> Option<String> {
    let o = Command::new(cmd).args(args).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).to_string())
}

fn tail_lines(text: &str, n: usize) -> Vec<String> {
    let v: Vec<&str> = text.lines().collect();
    v[v.len().saturating_sub(n)..].iter().map(|s| s.to_string()).collect()
}

fn read_tail(p: &Path, n: usize) -> Option<Vec<String>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(p).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(512 * 1024))).ok()?;
    let mut b = vec![];
    f.read_to_end(&mut b).ok()?;
    Some(tail_lines(&String::from_utf8_lossy(&b), n))
}

/// Bytes used under a path (du -sk), None when unreadable.
fn du(p: &Path) -> Option<u64> {
    if !p.exists() {
        return None;
    }
    let s = out("du", &["-sk", &p.display().to_string()])?;
    s.split_whitespace().next()?.parse::<u64>().ok().map(|k| k * 1024)
}

/// (use%, free bytes) of the filesystem holding `p`.
pub fn df(p: &Path) -> Option<(u64, u64)> {
    parse_df(&out("df", &["-Pk", &p.display().to_string()])?)
}

pub fn parse_df(s: &str) -> Option<(u64, u64)> {
    let f: Vec<&str> = s.lines().nth(1)?.split_whitespace().collect();
    let free = f.get(3)?.parse::<u64>().ok()? * 1024;
    let pct = f.get(4)?.trim_end_matches('%').parse().ok()?;
    Some((pct, free))
}

/// log_directory from postgresql.auto.conf / postgresql.conf (last one wins; default 'log').
pub fn log_directory(conf_texts: &[String]) -> String {
    let mut dir = "log".to_string();
    for t in conf_texts {
        for l in t.lines() {
            let l = l.split('#').next().unwrap_or("").trim();
            if let Some(v) = l.strip_prefix("log_directory") {
                let v = v.trim().trim_start_matches('=').trim().trim_matches('\'');
                if !v.is_empty() {
                    dir = v.to_string();
                }
            }
        }
    }
    dir
}

/// Where the data directory is when Postgres is down.
pub fn find_pgdata(given: Option<String>) -> Option<String> {
    given
        .or_else(|| std::env::var("PGDATA").ok().filter(|s| !s.is_empty()))
        .or_else(|| {
            let mut c: Vec<String> = vec!["/var/lib/postgresql/data".into()];
            if let Ok(rd) = std::fs::read_dir("/var/lib/postgresql") {
                for e in rd.flatten() {
                    c.push(e.path().join("main").display().to_string());
                }
            }
            c.into_iter().find(|d| Path::new(d).join("PG_VERSION").exists())
        })
}

fn pid_alive(pid: &str) -> bool {
    if Path::new("/proc/self").exists() {
        return Path::new("/proc").join(pid).exists();
    }
    Command::new("kill").args(["-0", pid]).status().map(|s| s.success()).unwrap_or(true)
}

pub struct Input {
    pub pgdata: Option<String>,
    pub log_file: Option<String>,
    pub postgres_up: bool,
}

pub fn diagnose(inp: &Input) -> Value {
    let mut lines: Vec<Evidence> = vec![];
    let mut sources: Vec<Value> = vec![];
    let mut facts = Facts::default();
    let pgdata = find_pgdata(inp.pgdata.clone());
    let mut extra: Vec<Evidence> = vec![]; // non-log evidence (pid file, disk, permissions)

    // --- process / pid file
    if let Some(d) = &pgdata {
        facts.pgdata = d.clone();
        let pidf = Path::new(d).join("postmaster.pid");
        if let Ok(s) = std::fs::read_to_string(&pidf) {
            let pid = s.lines().next().unwrap_or("").trim().to_string();
            let alive = !pid.is_empty() && pid.chars().all(|c| c.is_ascii_digit()) && pid_alive(&pid);
            facts.pid_alive = Some(alive);
            extra.push(Evidence { source: "postmaster.pid".into(),
                line: format!("postmaster.pid names PID {pid}, which is {}", if alive { "running" } else { "NOT running (stale lock file)" }) });
        }
        let pg_ctl = out("sh", &["-c", "command -v pg_ctl || ls /usr/lib/postgresql/*/bin/pg_ctl 2>/dev/null | tail -1"])
            .map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        if let Some(pc) = pg_ctl {
            if let Ok(o) = Command::new(&pc).args(["status", "-D", d]).output() {
                let t = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
                if let Some(l) = t.lines().next() {
                    extra.push(Evidence { source: "pg_ctl status".into(), line: l.to_string() });
                }
            }
        }
        // owner / mode
        #[cfg(unix)]
        if let Ok(m) = std::fs::metadata(d) {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let mode = m.permissions().mode() & 0o777;
            if m.uid() == 0 || (mode != 0o700 && mode != 0o750) {
                extra.push(Evidence { source: "data dir".into(),
                    line: format!("{d} is owned by uid {} with mode {:o} (want the postgres user, 700 or 750) — data directory has invalid permissions", m.uid(), mode) });
            }
        }
    }

    // --- Postgres log
    if let Some(f) = &inp.log_file {
        match read_tail(Path::new(f), 300).map(|v| since_last_start(&v).to_vec()) {
            Some(v) => {
                sources.push(json!({"source": f, "lines": v.len()}));
                lines.extend(v.into_iter().map(|l| Evidence { source: f.clone(), line: l }));
            }
            None => sources.push(json!({"source": f, "error": "unreadable"})),
        }
    }
    if let Some(d) = &pgdata {
        let confs: Vec<String> = ["postgresql.conf", "postgresql.auto.conf"]
            .iter().filter_map(|n| std::fs::read_to_string(Path::new(d).join(n)).ok()).collect();
        let ld = log_directory(&confs);
        let ldp = if ld.starts_with('/') { PathBuf::from(&ld) } else { Path::new(d).join(&ld) };
        facts.log_dir = Some(ldp.display().to_string());
        if let Ok(rd) = std::fs::read_dir(&ldp) {
            let newest = rd.flatten().filter(|e| e.path().is_file())
                .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok());
            if let Some(e) = newest {
                if let Some(v) = read_tail(&e.path(), 300).map(|v| since_last_start(&v).to_vec()) {
                    let src = e.path().display().to_string();
                    sources.push(json!({"source": src, "lines": v.len()}));
                    lines.extend(v.into_iter().map(|l| Evidence { source: src.clone(), line: l }));
                }
            }
        }
    }
    if let Some(t) = out("journalctl", &["-u", "postgresql*", "-n", "200", "--no-pager", "-q"]) {
        let v = since_last_start(&tail_lines(&t, 200)).to_vec();
        sources.push(json!({"source": "journalctl -u postgresql*", "lines": v.len()}));
        lines.extend(v.into_iter().map(|l| Evidence { source: "journald".into(), line: l }));
    }
    // --- kernel log (OOM killer)
    let kern = out("journalctl", &["-k", "-n", "500", "--no-pager", "-q", "-o", "short-unix"]).or_else(|| out("dmesg", &[]));
    // newest postmaster start: postmaster.pid line 3, else postmaster.opts (rewritten at every start)
    let start_epoch: Option<f64> = pgdata.as_ref().and_then(|d| {
        std::fs::read_to_string(Path::new(d).join("postmaster.pid")).ok()
            .and_then(|s| s.lines().nth(2).and_then(|x| x.trim().parse::<f64>().ok()))
            .or_else(|| std::fs::metadata(Path::new(d).join("postmaster.opts")).ok()?.modified().ok()?
                .duration_since(std::time::UNIX_EPOCH).ok().map(|x| x.as_secs_f64()))
    });
    let boot_epoch: Option<f64> = std::fs::read_to_string("/proc/uptime").ok()
        .and_then(|u| u.split_whitespace().next()?.parse::<f64>().ok())
        .and_then(|up| Some(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs_f64() - up));
    match kern {
        Some(t) => {
            let v: Vec<String> = kernel_recent(&tail_lines(&t, 500).join("\n"), start_epoch, boot_epoch);
            sources.push(json!({"source": "kernel log", "oom_lines": v.len()}));
            lines.extend(v.into_iter().map(|l| Evidence { source: "kernel".into(), line: l }));
        }
        None => sources.push(json!({"source": "kernel log", "error": "not readable (needs root or journald)"})),
    }

    // --- disk + sizes
    if let Some(d) = &pgdata {
        let dp = Path::new(d);
        if let Some((p, free)) = df(dp) {
            facts.disk_pct = Some(p);
            extra.push(Evidence { source: "df".into(), line: format!("data dir filesystem {p}% used, {} free", human(free)) });
        }
        if let Some((p, free)) = df(&dp.join("pg_wal")) {
            facts.wal_disk_pct = Some(p);
            if Some(p) != facts.disk_pct {
                extra.push(Evidence { source: "df".into(), line: format!("pg_wal filesystem {p}% used, {} free", human(free)) });
            }
        }
        facts.pg_wal_bytes = du(&dp.join("pg_wal")).unwrap_or(0);
        if let Ok(rd) = std::fs::read_dir(dp.join("pg_wal/archive_status")) {
            let now = std::time::SystemTime::now();
            for e in rd.flatten().filter(|e| e.file_name().to_string_lossy().ends_with(".ready")) {
                facts.ready_count += 1;
                if let Some(age) = e.metadata().ok().and_then(|m| m.modified().ok()).and_then(|t| now.duration_since(t).ok()) {
                    facts.oldest_ready_secs = Some(facts.oldest_ready_secs.unwrap_or(0).max(age.as_secs()));
                }
            }
        }
        if facts.ready_count > 0 {
            extra.push(Evidence { source: "pg_wal/archive_status".into(), line: format!("{} WAL segment(s) waiting to be archived, oldest {}s old; pg_wal is {}",
                facts.ready_count, facts.oldest_ready_secs.unwrap_or(0), human(facts.pg_wal_bytes)) });
        }
        let mut add = |p: PathBuf, clean: &'static str, note: &'static str| {
            if let Some(b) = du(&p) {
                facts.culprits.push(Culprit { path: p.display().to_string(), bytes: b, clean, note });
            }
        };
        add(dp.join("pg_wal"), "never", "WAL: never delete by hand; it shrinks once nothing holds it (slots, archive_command, max_wal_size)");
        add(dp.join("base"), "never", "table data");
        add(dp.join("base/pgsql_tmp"), "safe_when_stopped", "temporary sort/hash files; safe to empty only while Postgres is stopped");
        if let Some(l) = &facts.log_dir {
            if log_dir_cleanable(Path::new(l), dp) {
                add(PathBuf::from(l), "safe", "old server logs; safe to delete or compress (keep the newest)");
            } else {
                add(PathBuf::from(l), "never", "log_directory is the data directory (or looks like one): clean nothing here by hand");
            }
        }
        facts.culprits.sort_by(|a, b| b.bytes.cmp(&a.bytes));
    }

    // --- verdict
    let mut state = if inp.postgres_up { "up" } else { "down" }.to_string();
    if !inp.postgres_up && facts.pid_alive == Some(true) {
        state = lines.iter().rev().find_map(|e| state_hint(&e.line)).filter(|s| *s != "settled").unwrap_or("starting").to_string();
    }
    let mut all = extra.clone();
    all.extend(lines.iter().cloned());
    let (cause0, ev) = classify(&all);
    let cause = refine(&cause0, &facts);
    let mut evidence: Vec<Value> = extra.iter().map(|e| json!({"source": e.source, "line": e.line})).collect();
    evidence.extend(ev.iter().filter(|e| !extra.contains(e)).map(|e| json!({"source": e.source, "line": e.line})));
    let steps: Vec<Value> = steps_for(&cause, &facts).into_iter()
        .map(|s| json!({"tier": s.tier, "why": s.why, "command": s.command,
                        "needs_human_approval": s.tier == "destructive"})).collect();
    json!({
        "ok": true,
        "postgres": state,
        "probable_cause": cause,
        "evidence": evidence,
        "steps": steps,
        "facts": {
            "pgdata": pgdata, "log_dir": facts.log_dir,
            "disk_used_pct": facts.disk_pct, "pg_wal_disk_used_pct": facts.wal_disk_pct,
            "pg_wal_bytes": facts.pg_wal_bytes, "ready_wal": facts.ready_count, "oldest_ready_secs": facts.oldest_ready_secs,
            "space": facts.culprits.iter().map(|c| json!({"path": c.path, "bytes": c.bytes, "size": human(c.bytes), "clean": c.clean, "note": c.note})).collect::<Vec<_>>(),
        },
        "sources": sources,
        "policy": "pgbx only diagnoses: it never deletes, restarts or changes anything. Run the steps yourself; destructive ones need human approval.",
    })
}

pub fn text(v: &Value) -> String {
    let mut s = format!("postgres: {}\nprobable cause: {}\n", v["postgres"].as_str().unwrap_or("?"), v["probable_cause"].as_str().unwrap_or("?"));
    s.push_str("evidence:\n");
    for e in v["evidence"].as_array().cloned().unwrap_or_default() {
        s.push_str(&format!("  [{}] {}\n", e["source"].as_str().unwrap_or(""), e["line"].as_str().unwrap_or("")));
    }
    if let Some(sp) = v["facts"]["space"].as_array().filter(|a| !a.is_empty()) {
        s.push_str("space:\n");
        for c in sp {
            s.push_str(&format!("  {:>9}  {}  ({})\n", c["size"].as_str().unwrap_or(""), c["path"].as_str().unwrap_or(""), c["clean"].as_str().unwrap_or("")));
        }
    }
    s.push_str("steps (pgbx runs none of these):\n");
    for (i, st) in v["steps"].as_array().cloned().unwrap_or_default().iter().enumerate() {
        let tier = st["tier"].as_str().unwrap_or("");
        let tag = if tier == "destructive" { "DESTRUCTIVE — needs human approval".to_string() } else { tier.to_string() };
        s.push_str(&format!("  {}. [{tag}] {}\n", i + 1, st["why"].as_str().unwrap_or("")));
        if st["command"] != "-" {
            s.push_str(&format!("       {}\n", st["command"].as_str().unwrap_or("")));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(l: &str) -> Evidence {
        Evidence { source: "t".into(), line: l.into() }
    }

    #[test]
    fn line_patterns() {
        let cases = [
            ("[12345.6] Out of memory: Killed process 4242 (postgres) total-vm:9000kB", "oom_kill"),
            ("LOG:  server process (PID 77) was terminated by signal 9: Killed", "oom_kill"),
            ("PANIC:  could not write to file \"pg_wal/xlogtemp.31\": No space left on device", "disk_full"),
            ("ERROR:  invalid page in block 4 of relation base/16384/16385", "corruption"),
            ("ERROR:  could not open file \"base/16384/16399\": No such file or directory", "corruption"),
            ("FATAL:  data directory \"/var/lib/postgresql/data\" has invalid permissions", "permissions"),
            ("FATAL:  lock file \"postmaster.pid\" already exists", "stale_pid"),
            ("LOG:  could not bind IPv4 address \"0.0.0.0\": Address already in use", "config_error"),
            ("LOG:  syntax error in file \"/var/lib/postgresql/data/postgresql.auto.conf\" line 3, near token \"=\"", "config_error"),
            ("FATAL:  configuration file \"/etc/postgresql/16/main/postgresql.conf\" contains errors", "config_error"),
            ("FATAL:  sorry, too many clients already", "too_many_connections"),
            ("FATAL:  remaining connection slots are reserved for roles with the SUPERUSER attribute", "too_many_connections"),
            ("PANIC:  something went very wrong", "crash"),
            ("LOG:  archive command failed with exit code 1", "wal_backlog"),
        ];
        for (l, want) in cases {
            assert_eq!(classify_line(l), Some(want), "{l}");
        }
        assert_eq!(classify_line("LOG:  checkpoint complete"), None);
        assert_eq!(classify_line("ERROR:  permission denied for table invoices"), None, "SQL privilege, not the server");
        assert_eq!(classify_line("ERROR:  permission denied for schema app"), None);
        assert_eq!(classify_line("FATAL:  could not open file \"/etc/ssl/private/server.key\": Permission denied"), Some("permissions"));
        assert_eq!(classify_line("ERROR:  could not open file \"/tmp/export.csv\" for reading: No such file or directory"), None,
                   "COPY from a missing file is not corruption");
        assert_eq!(classify_line("ERROR:  could not open file \"global/1262\": No such file or directory"), Some("corruption"));
        assert_eq!(classify_line("Out of memory: Killed process 9 (java)"), None, "only postgres OOM counts");
    }

    #[test]
    fn recency() {
        let v: Vec<String> = [
            "2026-09-30 LOG:  server process (PID 9) was terminated by signal 9: Killed",
            "2026-09-30 LOG:  database system is ready to accept connections",
            "2026-10-01 LOG:  starting PostgreSQL 16.4",
            "2026-10-01 LOG:  invalid value for parameter \"shared_buffers\": \"lots\"",
        ].map(String::from).to_vec();
        let cur = since_last_start(&v);
        assert_eq!(cur.len(), 2);
        let ev: Vec<Evidence> = cur.iter().map(|l| super::Evidence { source: "pg".into(), line: l.clone() }).collect();
        assert_eq!(classify(&ev).0, "config_error");
        // an OOM kill from BEFORE the newest postmaster start is an old incident; the config error wins
        let kern = "1727700000.000000 host kernel: Out of memory: Killed process 4242 (postgres)\n\
                    1727790000.000000 host kernel: usb 1-1: new device";
        assert!(kernel_recent(kern, Some(1727780000.0), None).is_empty());
        assert_eq!(kernel_recent(kern, Some(1727600000.0), None).len(), 1);
        let dm = "[  100.5] Out of memory: Killed process 77 (postgres)";
        assert!(kernel_recent(dm, Some(2000.0), Some(1000.0)).is_empty(), "dmesg: boot 1000 + 100.5 < start 2000");
        assert_eq!(kernel_recent(dm, None, None).len(), 1, "no start time known: keep");
        let mut all = ev.clone();
        all.extend(kernel_recent(kern, Some(1727780000.0), None).into_iter().map(|l| super::Evidence { source: "kernel".into(), line: l }));
        assert_eq!(classify(&all).0, "config_error");
    }

    #[test]
    fn log_dir_guard() {
        let base = std::env::temp_dir().join(format!("pgbx-diag-t-{}", std::process::id()));
        let d = base.join("data");
        std::fs::create_dir_all(d.join("base")).unwrap();
        std::fs::create_dir_all(d.join("log")).unwrap();
        assert!(log_dir_cleanable(&d.join("log"), &d));
        assert!(!log_dir_cleanable(&d, &d), "log_directory = '.' is the data dir");
        assert!(!log_dir_cleanable(&base, &d), "a parent of the data dir");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn state_hints() {
        assert_eq!(state_hint("FATAL:  the database system is shutting down"), Some("shutting_down"));
        assert_eq!(state_hint("FATAL:  the database system is starting up"), Some("starting"));
        assert_eq!(state_hint("LOG:  redo starts at 0/1000028"), Some("recovering"));
        assert_eq!(state_hint("FATAL:  the database system is in recovery mode"), Some("recovering"));
    }

    #[test]
    fn priority_and_evidence() {
        let lines = vec![
            ev("FATAL:  sorry, too many clients already"),
            ev("PANIC:  could not write to file: No space left on device"),
            ev("LOG:  checkpoint starting"),
        ];
        let (c, e) = classify(&lines);
        assert_eq!(c, "disk_full");
        assert_eq!(e.len(), 1);
        let (c, e) = classify(&[ev("LOG: all good")]);
        assert_eq!((c.as_str(), e.len()), ("unknown", 0));
        let (c, _) = classify(&[ev("ERROR:  invalid page in block 1"), ev("PANIC: no space left on device")]);
        assert_eq!(c, "corruption", "corruption outranks everything");
        let (c, _) = classify(&[ev("LOG:  archive command failed with exit code 1"), ev("Out of memory: Killed process 7 (postgres)")]);
        assert_eq!(c, "oom_kill", "a failing archive_command does not stop Postgres; the OOM kill does");
        let (c, _) = classify(&[ev("LOG:  archive command failed"), ev("postmaster.pid names PID 9, which is NOT running (stale lock file)")]);
        assert_eq!(c, "stale_pid");
    }

    #[test]
    fn refine_wal_backlog() {
        let mut f = Facts { disk_pct: Some(100), pg_wal_bytes: 50 << 30, ready_count: 3000, ..Default::default() };
        f.culprits.push(Culprit { path: "/d/pg_wal".into(), bytes: 50 << 30, clean: "never", note: "" });
        assert_eq!(refine("disk_full", &f), "wal_backlog");
        assert_eq!(refine("unknown", &f), "wal_backlog");
        let g = Facts { disk_pct: Some(100), ..Default::default() };
        assert_eq!(refine("unknown", &g), "disk_full");
        assert_eq!(refine("oom_kill", &f), "oom_kill", "a clear log cause is kept");
        let h = Facts { pid_alive: Some(false), ..Default::default() };
        assert_eq!(refine("unknown", &h), "stale_pid");
    }

    #[test]
    fn step_tiers() {
        let f = Facts {
            pgdata: "/d".into(),
            culprits: vec![
                Culprit { path: "/d/pg_wal".into(), bytes: 9 << 30, clean: "never", note: "" },
                Culprit { path: "/d/log".into(), bytes: 1 << 30, clean: "safe", note: "" },
                Culprit { path: "/d/base/pgsql_tmp".into(), bytes: 1 << 30, clean: "safe_when_stopped", note: "" },
            ],
            ..Default::default()
        };
        let tiers = ["readonly", "safe", "guarded", "destructive"];
        for c in CAUSE_PRIORITY {
            let s = steps_for(c, &f);
            assert!(!s.is_empty(), "{c}");
            assert!(s.iter().all(|x| tiers.contains(&x.tier)), "{c}");
        }
        let w = steps_for("wal_backlog", &f);
        assert!(w.iter().any(|s| s.tier == "destructive" && s.command.contains("pg_drop_replication_slot") && s.why.contains("needs human approval")));
        assert!(!w.iter().any(|s| s.tier != "destructive" && s.command.contains("pg_drop_replication_slot")));
        assert!(w.iter().any(|s| s.tier == "guarded" && s.command.contains("max_slot_wal_keep_size")));
        assert!(!w.iter().any(|s| s.command.contains("wal_queue_max") || s.command.contains("--cluster") || s.command.contains("pgbackrest ")));
        // pg_wal is never offered for cleaning; logs are safe, pgsql_tmp guarded
        assert!(!w.iter().any(|s| s.tier != "readonly" && s.command.contains("rm") && s.command.contains("pg_wal")));
        assert!(w.iter().any(|s| s.tier == "safe" && s.command.contains("/d/log")));
        assert!(w.iter().any(|s| s.tier == "guarded" && s.command.contains("pgsql_tmp") && s.command.contains("STOPPED")));
        assert!(w.iter().any(|s| s.why.contains("NEVER delete anything in pg_wal")));
        let c = steps_for("corruption", &f);
        assert!(c.iter().any(|s| s.tier == "safe" && s.command.contains("db-restore")));
        assert!(!c.iter().any(|s| s.command.contains("pgbx restore ")));
        assert!(c.iter().filter(|s| s.tier == "destructive").all(|s| s.why.contains("needs human approval")));
        assert!(!c.iter().any(|s| s.tier != "readonly" && s.command.contains("pg_resetwal")));
    }

    #[test]
    fn helpers() {
        assert_eq!(log_directory(&["log_directory = 'pg_log'  # x".into(), "#log_directory='no'".into()]), "pg_log");
        assert_eq!(log_directory(&[]), "log");
        assert_eq!(parse_df("Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/sda1 100 99 1 99% /\n"), Some((99, 1024)));
    }
}
