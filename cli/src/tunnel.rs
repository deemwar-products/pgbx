//! Reaching a server over SSH with the SYSTEM `ssh` binary (keys, agent, ~/.ssh/config and ProxyJump are
//! all ssh's business; pgbx never touches a key). Works the same on Windows OpenSSH (no ControlMaster needed).
//!
//! Postgres traffic goes through a REUSED forward: the first command for a profile spawns a small detached
//! helper (`pgbx tunnel --serve KEY ...`) that owns
//!   ssh -N -o ExitOnForwardFailure=yes -o BatchMode=yes -o ServerAliveInterval=30 -L 127.0.0.1:<free>:<pg host>:<pg port> target
//! and records {pid, ssh_pid, port, started, last_used, idle_secs, spec} in `<state dir>/tunnels/KEY.json` (0600).
//! Later commands reuse it while the helper is alive and the port accepts, refreshing `last_used`.
//! The helper kills ssh and exits once idle longer than `tunnel-idle` (default 10m), when ssh dies, or when
//! its state file is removed (`pgbx tunnel close`). A lock file stops two commands starting two tunnels.
//!
//! Host-side commands (setup, doctor, logs, diagnose) run as `ssh target pgbx <cmd> ... --json`.

use crate::Args;
use serde_json::{json, Value};
use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const INSTALL: &str = "curl -fsSL https://deemwar-products.github.io/pgbx/install.sh | sh";
pub const DEFAULT_IDLE: u64 = 600;

type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

fn sys_env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|s| !s.is_empty())
}

/// The ssh binary; PGBX_SSH overrides (e.g. a wrapper adding `-F config` for tests).
fn ssh_bin() -> String {
    sys_env("PGBX_SSH").unwrap_or("ssh".into())
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// ---------------------------------------------------------------- command lines

/// Commands that need the machine itself, not just a Postgres connection.
pub fn runs_remotely(cmd: &str, a: &Args) -> bool {
    a.flags.contains_key("ssh") && matches!(cmd, "doctor" | "logs" | "diagnose" | "setup")
        && !(cmd == "setup" && a.pos.first().map(String::as_str) == Some("client"))
}

/// `ssh` options shared by the tunnel and remote commands, ending with the target.
pub fn ssh_base(a: &Args) -> Result<Vec<String>, String> {
    let target = a.flags.get("ssh").ok_or("no --ssh target")?;
    if target.starts_with('-') || target.is_empty() {
        return Err(format!("bad ssh target '{target}' (want user@host or a ~/.ssh/config Host)"));
    }
    let mut v: Vec<String> = ["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"].iter().map(|s| s.to_string()).collect();
    if let Some(p) = a.flags.get("ssh-port") {
        p.parse::<u16>().map_err(|_| format!("bad --ssh-port '{p}'"))?;
        v.extend(["-p".into(), p.clone()]);
    }
    if let Some(j) = a.flags.get("ssh-jump") {
        v.extend(["-J".into(), j.clone()]);
    }
    v.push(target.clone());
    Ok(v)
}

/// Where Postgres listens as seen from the ssh host: --host unless it is a socket dir, else localhost.
pub fn remote_pg(a: &Args) -> (String, String) {
    let h = a.flags.get("host").filter(|h| !h.starts_with('/')).cloned().unwrap_or("localhost".into());
    (h, a.flags.get("port").cloned().unwrap_or("5432".into()))
}

pub fn tunnel_args(a: &Args, local: u16) -> Result<Vec<String>, String> {
    let (h, p) = remote_pg(a);
    let mut v: Vec<String> =
        ["-N", "-o", "ExitOnForwardFailure=yes", "-o", "ServerAliveInterval=30", "-L"].iter().map(|s| s.to_string()).collect();
    v.push(format!("127.0.0.1:{local}:{h}:{p}"));
    v.extend(ssh_base(a)?);
    Ok(v)
}

/// What the forward reaches; a state file with another spec is not reused.
pub fn spec(a: &Args) -> Result<String, String> {
    let (h, p) = remote_pg(a);
    Ok(format!("{} -> {h}:{p}", ssh_base(a)?.join(" ")))
}

/// State-file key: the profile name, else the ssh target.
pub fn key(a: &Args, profile: Option<&str>) -> String {
    let raw = match profile {
        Some(p) => p.to_string(),
        None => format!("ssh-{}-{}", a.flags.get("ssh").map(String::as_str).unwrap_or(""), remote_pg(a).1),
    };
    raw.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' }).collect()
}

pub fn idle_secs(a: &Args) -> Result<u64, String> {
    match a.flags.get("tunnel-idle") {
        Some(s) => Ok(crate::query::parse_duration(s).map_err(|e| e.replace("--timeout", "--tunnel-idle"))?.as_secs().max(1)),
        None => Ok(DEFAULT_IDLE),
    }
}

// ---------------------------------------------------------------- state

pub fn state_dir(env: Env) -> Result<PathBuf, String> {
    if let Some(d) = env("PGBX_STATE_DIR") {
        return Ok(PathBuf::from(d).join("tunnels"));
    }
    if cfg!(windows) {
        if let Some(a) = env("LOCALAPPDATA") {
            return Ok(PathBuf::from(a).join("pgbx").join("tunnels"));
        }
    }
    if let Some(x) = env("XDG_CACHE_HOME") {
        return Ok(PathBuf::from(x).join("pgbx").join("tunnels"));
    }
    let home = env("HOME").or_else(|| env("USERPROFILE")).ok_or("HOME (or USERPROFILE) is not set")?;
    Ok(PathBuf::from(home).join(".cache").join("pgbx").join("tunnels"))
}

/// Idle longer than allowed at `now`? (pure: the clock is injected)
pub fn expired(st: &Value, now: u64) -> bool {
    let last = st["last_used"].as_u64().unwrap_or(0);
    let idle = st["idle_secs"].as_u64().unwrap_or(DEFAULT_IDLE);
    now.saturating_sub(last) > idle
}

/// Seconds left before the helper closes it.
pub fn remaining(st: &Value, now: u64) -> u64 {
    let last = st["last_used"].as_u64().unwrap_or(0);
    (last + st["idle_secs"].as_u64().unwrap_or(DEFAULT_IDLE)).saturating_sub(now)
}

fn read_state(p: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
}

/// Atomic write (temp + rename), mode 0600.
fn write_state(p: &Path, v: &Value) -> Result<(), String> {
    let dir = p.parent().unwrap();
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    let tmp = p.with_extension(format!("tmp{}", std::process::id()));
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    use std::io::Write;
    o.open(&tmp).and_then(|mut f| f.write_all(v.to_string().as_bytes())).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, p).map_err(|e| format!("cannot write {}: {e}", p.display()))
}

fn pid_alive(pid: u64) -> bool {
    if pid == 0 {
        return false;
    }
    if cfg!(windows) {
        Command::new("tasklist").args(["/FI", &format!("PID eq {pid}"), "/NH"]).output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string())).unwrap_or(false)
    } else {
        Command::new("kill").args(["-0", &pid.to_string()]).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
    }
}

fn kill_pid(pid: u64) {
    if pid == 0 {
        return;
    }
    let _ = if cfg!(windows) {
        Command::new("taskkill").args(["/F", "/PID", &pid.to_string()]).stdout(Stdio::null()).stderr(Stdio::null()).status()
    } else {
        Command::new("kill").arg(pid.to_string()).stderr(Stdio::null()).status()
    };
}

fn port_open(port: u16) -> bool {
    TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(300)).is_ok()
}

fn alive(st: &Value) -> bool {
    st["error"].is_null() && pid_alive(st["pid"].as_u64().unwrap_or(0)) && port_open(st["port"].as_u64().unwrap_or(0) as u16)
}

/// A lock file created exclusively; a lock older than 60 s is from a crashed command and is taken over.
struct Lock(PathBuf);

impl Lock {
    fn take(p: PathBuf) -> Result<Lock, String> {
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d).map_err(|e| format!("cannot create {}: {e}", d.display()))?;
        }
        let t0 = Instant::now();
        loop {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&p) {
                Ok(_) => return Ok(Lock(p)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(&p).and_then(|m| m.modified()).ok()
                        .and_then(|m| m.elapsed().ok()).is_some_and(|age| age > Duration::from_secs(60));
                    if stale {
                        let _ = std::fs::remove_file(&p);
                    } else if t0.elapsed() > Duration::from_secs(40) {
                        return Err(format!("another pgbx is starting this tunnel (lock {})", p.display()));
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => return Err(format!("cannot lock {}: {e}", p.display())),
            }
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

// ---------------------------------------------------------------- client side

/// The local port of a live forward for this profile, starting the helper if needed.
pub fn ensure(a: &Args, profile: Option<&str>) -> Result<u16, String> {
    let dir = state_dir(&sys_env)?;
    let k = key(a, profile);
    let file = dir.join(format!("{k}.json"));
    let want = spec(a)?;
    let _lock = Lock::take(dir.join(format!("{k}.lock")))?;
    if let Some(mut st) = read_state(&file) {
        if st["spec"] == json!(want) && !expired(&st, now()) && alive(&st) {
            st["last_used"] = json!(now());
            write_state(&file, &st)?;
            return Ok(st["port"].as_u64().unwrap_or(0) as u16);
        }
        close_file(&file, &st); // stale, expired, or now pointing elsewhere
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut c = Command::new(exe);
    c.args(["tunnel", "--serve", &k, "--ssh", &a.flags["ssh"], "--tunnel-idle", &format!("{}s", idle_secs(a)?)]);
    for f in ["ssh-port", "ssh-jump", "host", "port"] {
        if let Some(v) = a.flags.get(f) {
            c.arg(format!("--{f}")).arg(v);
        }
    }
    c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    detach(&mut c);
    c.spawn().map_err(|e| format!("cannot start the tunnel helper: {e}"))?;
    let t0 = Instant::now();
    loop {
        if let Some(st) = read_state(&file) {
            if let Some(e) = st["error"].as_str() {
                let _ = std::fs::remove_file(&file);
                return Err(e.to_string());
            }
            if let Some(p) = st["port"].as_u64() {
                return Ok(p as u16);
            }
        }
        if t0.elapsed() > Duration::from_secs(30) {
            return Err(format!("ssh tunnel to {} did not come up within 30 s", a.flags["ssh"]));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Own process group / no console, so Ctrl-C on the command that started it does not kill the helper.
fn detach(c: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        c.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0000_0008 | 0x0000_0200 | 0x0800_0000); // DETACHED_PROCESS | NEW_PROCESS_GROUP | NO_WINDOW
    }
}

fn close_file(file: &Path, st: &Value) {
    let _ = std::fs::remove_file(file); // the helper sees this and kills ssh; kill directly too, for speed
    kill_pid(st["ssh_pid"].as_u64().unwrap_or(0));
    kill_pid(st["pid"].as_u64().unwrap_or(0));
}

fn describe(name: &str, st: &Value, now: u64) -> Value {
    json!({"name": name, "local_port": st["port"], "spec": st["spec"], "pid": st["pid"], "ssh_pid": st["ssh_pid"],
        "started": st["started"], "last_used": st["last_used"], "idle_secs": st["idle_secs"],
        "closes_in_secs": remaining(st, now), "alive": alive(st)})
}

fn states(dir: &Path) -> Vec<(String, PathBuf, Value)> {
    let mut v: Vec<(String, PathBuf, Value)> = std::fs::read_dir(dir).into_iter().flatten().flatten()
        .filter_map(|e| {
            let p = e.path();
            let name = p.file_name()?.to_str()?.strip_suffix(".json")?.to_string();
            Some((name, p.clone(), read_state(&p)?))
        })
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

/// `pgbx tunnel [open] | list | close [NAME | --all]`.
pub fn run(a: &Args, profile: Option<&str>) -> Result<Value, String> {
    let dir = state_dir(&sys_env)?;
    let n = now();
    match a.pos.first().map(String::as_str).unwrap_or("open") {
        "open" => {
            if !a.flags.contains_key("ssh") {
                return Err("pgbx tunnel needs --ssh user@host (or a profile with ssh)".into());
            }
            let port = ensure(a, profile)?;
            let k = key(a, profile);
            let st = read_state(&dir.join(format!("{k}.json"))).unwrap_or(Value::Null);
            let user = a.flags.get("user").map(String::as_str).unwrap_or("postgres");
            Ok(json!({"ok": true, "tunnel": describe(&k, &st, n), "local_host": "127.0.0.1", "local_port": port,
                "connect": format!("psql -h 127.0.0.1 -p {port} -U {user}")}))
        }
        "list" => {
            let ts: Vec<Value> = states(&dir).iter().map(|(k, _, st)| describe(k, st, n)).collect();
            Ok(json!({"ok": true, "dir": dir.display().to_string(), "tunnels": ts}))
        }
        "close" => {
            let all = a.flags.contains_key("all");
            let want = a.pos.get(1).cloned().or(profile.map(String::from));
            if !all && want.is_none() {
                return Err("pgbx tunnel close <profile> | --all".into());
            }
            let mut closed = vec![];
            for (k, p, st) in states(&dir) {
                if all || want.as_deref() == Some(k.as_str()) {
                    close_file(&p, &st);
                    closed.push(k);
                }
            }
            if !all && closed.is_empty() {
                return Err(format!("no open tunnel '{}' (pgbx tunnel list)", want.unwrap_or_default()));
            }
            Ok(json!({"ok": true, "closed": closed}))
        }
        x => Err(format!("unknown tunnel action '{x}' (open | list | close)")),
    }
}

// ---------------------------------------------------------------- helper side

fn free_port() -> Result<u16, String> {
    let l = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("no free local port: {e}"))?;
    Ok(l.local_addr().map_err(|e| e.to_string())?.port())
}

/// `pgbx tunnel --serve KEY ...` (hidden): own one ssh forward until idle, then clean up.
pub fn serve(a: &Args) -> i32 {
    let Ok(dir) = state_dir(&sys_env) else { return 1 };
    let file = dir.join(format!("{}.json", a.flags["serve"]));
    let fail = |e: String| {
        let _ = write_state(&file, &json!({"error": e, "pid": std::process::id()}));
        1
    };
    let (port, idle, sp) = match (free_port(), idle_secs(a), spec(a)) {
        (Ok(p), Ok(i), Ok(s)) => (p, i, s),
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => return fail(e),
    };
    let args = match tunnel_args(a, port) {
        Ok(v) => v,
        Err(e) => return fail(e),
    };
    let mut child = match Command::new(ssh_bin()).args(&args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => return fail(format!("cannot run ssh: {e} (install OpenSSH)")),
    };
    let target = a.flags["ssh"].clone();
    let t0 = Instant::now();
    loop {
        if let Ok(Some(st)) = child.try_wait() {
            let mut err = String::new();
            if let Some(mut e) = child.stderr.take() {
                let _ = e.read_to_string(&mut err);
            }
            return fail(format!("ssh tunnel to {target} failed ({st}): {}", err.trim()));
        }
        if port_open(port) {
            break;
        }
        if t0.elapsed() > Duration::from_secs(25) {
            let _ = child.kill();
            return fail(format!("ssh tunnel to {target} did not come up within 25 s"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    drop(child.stderr.take()); // nobody reads it from here on; ssh must not block on a full pipe
    let t = now();
    let st = json!({"pid": std::process::id(), "ssh_pid": child.id(), "port": port, "started": t, "last_used": t,
        "idle_secs": idle, "spec": sp});
    if let Err(e) = write_state(&file, &st) {
        let _ = child.kill();
        return fail(e);
    }
    let tick = Duration::from_millis((idle * 1000 / 4).clamp(200, 5000));
    loop {
        std::thread::sleep(tick);
        let ssh_done = matches!(child.try_wait(), Ok(Some(_)));
        let cur = read_state(&file);
        let mine = cur.as_ref().is_some_and(|s| s["pid"] == json!(std::process::id()));
        if ssh_done || !mine || cur.as_ref().is_some_and(|s| expired(s, now())) {
            let _ = child.kill();
            let _ = child.wait();
            if mine {
                let _ = std::fs::remove_file(&file);
            }
            return 0;
        }
    }
}

// ---------------------------------------------------------------- remote commands

/// POSIX single-quote for the remote shell.
pub fn sq(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:@,+%".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Flags that describe how to REACH the server; they are not forwarded to the remote pgbx.
const LOCAL_ONLY: &[&str] = &["profile", "ssh", "ssh-port", "ssh-jump", "tunnel-idle", "json", "host", "port"];

pub fn remote_cmdline(cmd: &str, a: &Args) -> String {
    let mut w = vec![];
    if cmd == "setup" {
        w.push("sudo -n".to_string()); // setup writes postgresql conf.d and /etc/pgbx; no password prompt possible
    }
    w.push("pgbx".into());
    w.push(cmd.into());
    w.extend(a.pos.iter().map(|p| sq(p)));
    let mut keys: Vec<&String> = a.flags.keys().filter(|k| !LOCAL_ONLY.contains(&k.as_str())).collect();
    keys.sort();
    for k in keys {
        w.push(format!("--{k}"));
        let v = &a.flags[k];
        if !v.is_empty() {
            w.push(sq(v));
        }
    }
    w.push("--json".into());
    w.join(" ")
}

/// Run `pgbx <cmd>` on the ssh host and return its JSON reply.
pub fn run_remote(cmd: &str, a: &Args) -> Result<Value, String> {
    let line = remote_cmdline(cmd, a);
    let out = Command::new(ssh_bin())
        .args(ssh_base(a)?)
        .arg(&line)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run ssh: {e} (install OpenSSH)"))?;
    let target = &a.flags["ssh"];
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let code = out.status.code();
    if code == Some(127) || stderr.contains("pgbx: command not found") || stderr.contains("pgbx: not found") {
        return Err(format!("pgbx is not installed on {target}; install it there with: ssh {target} '{INSTALL}'"));
    }
    if code == Some(255) {
        return Err(format!("ssh to {target} failed: {}", stderr.trim()));
    }
    if cmd == "setup" && stdout.trim().is_empty() && stderr.contains("sudo") {
        return Err(format!("pgbx setup needs root on {target} and sudo wants a password: run `ssh -t {target} sudo pgbx setup` yourself"));
    }
    let mut v: Value = serde_json::from_str(stdout.trim()).map_err(|_| {
        format!("remote pgbx on {target} did not answer with JSON (exit {code:?}): {}", stdout.trim().chars().take(300).collect::<String>())
    })?;
    v["remote"] = json!({"ssh": target, "command": line});
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_args;

    fn p(s: &[&str]) -> Args {
        parse_args(s.iter().map(|x| x.to_string())).unwrap()
    }

    #[test]
    fn tunnel_command_line() {
        let a = p(&["status", "--ssh", "ops@db1", "--ssh-port", "2222", "--ssh-jump", "bastion", "--port", "6432"]);
        assert_eq!(tunnel_args(&a, 40000).unwrap().join(" "),
            "-N -o ExitOnForwardFailure=yes -o ServerAliveInterval=30 -L 127.0.0.1:40000:localhost:6432 \
             -o BatchMode=yes -o ConnectTimeout=10 -p 2222 -J bastion ops@db1");
        let a = p(&["status", "--ssh", "db1", "--host", "10.0.0.5"]);
        assert!(tunnel_args(&a, 1).unwrap().contains(&"127.0.0.1:1:10.0.0.5:5432".to_string()));
        let a = p(&["status", "--ssh", "db1", "--host", "/var/run/postgresql"]);
        assert_eq!(remote_pg(&a).0, "localhost");
        assert!(ssh_base(&p(&["status", "--ssh", "-oProxyCommand=x"])).is_err());
        assert!(ssh_base(&p(&["status", "--ssh", "h", "--ssh-port", "x"])).is_err());
    }

    #[test]
    fn keys_specs_and_idle() {
        let a = p(&["status", "--ssh", "ops@db1"]);
        assert_eq!(key(&a, Some("prod")), "prod");
        assert_eq!(key(&a, None), "ssh-ops_db1-5432");
        assert_ne!(spec(&a).unwrap(), spec(&p(&["status", "--ssh", "ops@db1", "--port", "6432"])).unwrap());
        assert_eq!(idle_secs(&a).unwrap(), 600);
        assert_eq!(idle_secs(&p(&["status", "--tunnel-idle", "3s"])).unwrap(), 3);
        assert!(idle_secs(&p(&["status", "--tunnel-idle", "never"])).is_err());
    }

    #[test]
    fn expiry_with_injected_clock() {
        let st = json!({"last_used": 1000, "idle_secs": 600});
        assert!(!expired(&st, 1000));
        assert!(!expired(&st, 1600));
        assert!(expired(&st, 1601));
        assert_eq!(remaining(&st, 1100), 500);
        assert_eq!(remaining(&st, 9999), 0);
        assert!(expired(&json!({"last_used": 0}), DEFAULT_IDLE + 1)); // defaults
    }

    #[test]
    fn state_files_and_lock() {
        let d = std::env::temp_dir().join(format!("pgbx-tun-{}", std::process::id()));
        let f = d.join("prod.json");
        write_state(&f, &json!({"pid": 0, "port": 1, "last_used": 5, "idle_secs": 10})).unwrap();
        assert_eq!(read_state(&f).unwrap()["last_used"], 5);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert!(!alive(&read_state(&f).unwrap())); // pid 0 / closed port
        assert_eq!(states(&d).len(), 1);
        let l = Lock::take(d.join("prod.lock")).unwrap();
        assert!(d.join("prod.lock").exists());
        drop(l);
        assert!(!d.join("prod.lock").exists());
        let env = |k: &str| (k == "PGBX_STATE_DIR").then(|| "/s".to_string());
        assert_eq!(state_dir(&env).unwrap(), PathBuf::from("/s/tunnels"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn remote_routing_and_quoting() {
        assert!(runs_remotely("doctor", &p(&["doctor", "--ssh", "h"])));
        assert!(!runs_remotely("doctor", &p(&["doctor"])));
        assert!(!runs_remotely("status", &p(&["status", "--ssh", "h"])));
        assert!(runs_remotely("setup", &p(&["setup", "server", "--ssh", "h"])));
        assert!(!runs_remotely("setup", &p(&["setup", "client", "x", "--ssh", "h"])));
        let a = p(&["logs", "--ssh", "h", "--profile", "x", "--host", "y", "--lines", "5", "--log", "/var/log/a b.log", "--json"]);
        assert_eq!(remote_cmdline("logs", &a), "pgbx logs --lines 5 --log '/var/log/a b.log' --json");
        assert_eq!(remote_cmdline("setup", &p(&["setup", "--ssh", "h", "--yes"])), "sudo -n pgbx setup --yes --json");
        assert_eq!(sq("it's"), "'it'\\''s'");
        assert_eq!(sq("$(rm -rf /)"), "'$(rm -rf /)'");
    }
}
