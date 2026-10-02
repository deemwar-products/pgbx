//! Adapter protocol v1 (ADR 0003): an adapter is any command that hands pgbx a connection string.
//!
//!  1. pgbx spawns the command (argv, no shell) in its own process group (Unix: a new process group; Windows: a
//!     Job Object that kills everything in it when pgbx's handle closes), stdin/stdout piped, stderr captured.
//!  2. pgbx writes ONE line: {"action":"start","name":"<profile>","config":{...$VARs expanded...}}
//!  3. the adapter prints exactly ONE stdout line: {"url":"postgres://...","state":"ready","name":"<profile>"}
//!     within `ready_timeout` (30 s). Any other state, a different name, or anything that is not that JSON line
//!     is an error, shown with the adapter's (redacted) stderr. Logs belong on stderr, never stdout.
//!  4. pgbx uses the URL in memory only and never talks to the adapter again until:
//!  5. stop: {"action":"stop"} on stdin and stdin closed (EOF also means stop, so if pgbx dies the adapter exits),
//!     a 5 s grace, then the whole process group is killed (SIGTERM, then SIGKILL; Windows: TerminateJobObject).
//!
//! A one-off command stops its adapter when it finishes (main calls `stop_all`, also on Ctrl-C);
//! `pgbx serve` keeps one adapter per connection alive for its whole run.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

pub const READY_TIMEOUT: Duration = Duration::from_secs(30);
pub const GRACE: Duration = Duration::from_secs(5);
const FAILED_GRACE: Duration = Duration::from_secs(1);
const STDERR_KEEP: usize = 16 * 1024;

/// Put a child in its own process group, so pgbx can kill it with everything it started.
pub fn own_group(c: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        c.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0000_0200 | 0x0800_0000); // CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW
    }
}

/// Kill a child started with `own_group` and its whole group, then reap it (used for timeouts).
pub fn kill_tree(child: &mut Child) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(windows)]
mod job {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::*;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};

    /// A Job Object holding the adapter and everything it starts; closing pgbx's handle kills them all.
    pub struct Job(pub HANDLE);
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        pub fn for_pid(pid: u32) -> Option<Job> {
            unsafe {
                let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if h.is_null() {
                    return None;
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                SetInformationJobObject(h, JobObjectExtendedLimitInformation, &info as *const _ as *const _,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32);
                let p = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
                if !p.is_null() {
                    AssignProcessToJobObject(h, p);
                    CloseHandle(p);
                }
                Some(Job(h))
            }
        }
        pub fn terminate(&self) {
            unsafe {
                TerminateJobObject(self.0, 1);
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

/// A started adapter: kept until `stop`.
pub struct Running {
    child: Child,
    stdin: Option<ChildStdin>,
    stderr: Arc<Mutex<Vec<u8>>>,
    pub grace: Duration,
    stopped: bool,
    #[cfg(windows)]
    job: Option<job::Job>,
}

/// The parsed result line.
#[derive(Debug, PartialEq)]
pub struct Ready {
    pub url: String,
}

/// Check one stdout line against the protocol.
pub fn parse_result(line: &str, want_name: &str) -> Result<Ready, String> {
    let l = line.trim();
    let v: Value = serde_json::from_str(l).map_err(|_| {
        format!("protocol error: the adapter printed a line that is not its JSON result: '{}'. \
                 stdout carries only the one result line; logs belong on stderr",
            crate::vars::scrub(&l.chars().take(200).collect::<String>()))
    })?;
    if !v.is_object() {
        return Err("protocol error: the result line must be a JSON object {url, state, name}".into());
    }
    let state = v["state"].as_str().unwrap_or("");
    let name = v["name"].as_str().unwrap_or("");
    let url = v["url"].as_str().unwrap_or("");
    if !url.is_empty() {
        crate::conn::register_url_secrets(url);
    }
    if state != "ready" {
        let st = if state.is_empty() { "(no state)".to_string() } else { crate::vars::scrub(state) };
        return Err(format!("adapter state: {st}"));
    }
    if name != want_name {
        return Err(format!("protocol error: the adapter answered for '{name}', not for profile '{want_name}'"));
    }
    if url.is_empty() {
        return Err("protocol error: state is ready but url is empty".into());
    }
    Ok(Ready { url: url.to_string() })
}

impl Running {
    /// Start `argv`, send start, wait for the one result line. On any failure the adapter is stopped.
    pub fn start(argv: &[String], name: &str, config: &Value, ready_timeout: Duration, cwd: Option<&std::path::Path>)
        -> Result<(Running, Ready), String> {
        let cmd = argv.first().ok_or("empty adapter command")?;
        let mut c = Command::new(cmd);
        c.args(&argv[1..]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        if let Some(d) = cwd.filter(|d| d.is_dir()) {
            c.current_dir(d);
        }
        own_group(&mut c);
        install_signal_stop();
        let mut child = c.spawn().map_err(|e| format!("cannot start adapter `{}`: {e}", argv.join(" ")))?;
        #[cfg(windows)]
        let job = job::Job::for_pid(child.id());
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let mut errpipe = child.stderr.take().unwrap();
        let stderr = Arc::new(Mutex::new(Vec::new()));
        {
            // keep draining stderr for the adapter's whole life (a full pipe would block it); keep the tail
            let buf = Arc::clone(&stderr);
            std::thread::spawn(move || {
                let mut chunk = [0u8; 4096];
                while let Ok(n) = errpipe.read(&mut chunk) {
                    if n == 0 {
                        break;
                    }
                    let mut b = buf.lock().unwrap_or_else(|e| e.into_inner());
                    b.extend_from_slice(&chunk[..n]);
                    if b.len() > STDERR_KEEP {
                        let cut = b.len() - STDERR_KEEP;
                        b.drain(..cut);
                    }
                }
            });
        }
        let (tx, rx) = mpsc::channel::<std::io::Result<String>>();
        std::thread::spawn(move || {
            let mut r = BufReader::new(stdout);
            let mut first = true;
            loop {
                let mut l = String::new();
                match r.read_line(&mut l) {
                    Ok(0) => {
                        if first {
                            let _ = tx.send(Ok(String::new()));
                        }
                        break;
                    }
                    Ok(_) if first => {
                        first = false;
                        let _ = tx.send(Ok(l));
                    }
                    Ok(_) => {} // nothing more is expected on stdout: drained and dropped
                    Err(e) => {
                        if first {
                            let _ = tx.send(Err(e));
                        }
                        break;
                    }
                }
            }
        });
        let mut run = Running {
            child, stdin, stderr, grace: GRACE, stopped: false,
            #[cfg(windows)]
            job,
        };
        let msg = json!({"action": "start", "name": name, "config": config}).to_string() + "\n";
        if let Some(i) = run.stdin.as_mut() {
            let _ = i.write_all(msg.as_bytes()).and_then(|_| i.flush());
        }
        let label = format!("adapter for profile '{name}' (`{}`)", argv.join(" "));
        let r = match rx.recv_timeout(ready_timeout) {
            Ok(Ok(l)) if l.is_empty() => {
                std::thread::sleep(Duration::from_millis(100)); // let stderr arrive
                let code = run.child.try_wait().ok().flatten().map(|s| s.to_string()).unwrap_or_else(|| "closed stdout".into());
                Err(format!("{label} exited without a result ({code})"))
            }
            Ok(Ok(l)) => parse_result(&l, name).map_err(|e| format!("{label}: {e}")),
            Ok(Err(e)) => Err(format!("{label}: cannot read its stdout: {e}")),
            Err(_) => Err(format!("{label} gave no result within {} s", ready_timeout.as_secs_f32())),
        };
        match r {
            Ok(ready) => Ok((run, ready)),
            Err(e) => {
                run.grace = FAILED_GRACE; // it never became ready: a short grace, then its group goes
                run.stop();
                Err(format!("{e}{}", run.stderr_tail()))
            }
        }
    }

    /// The adapter's stderr so far, redacted, as "; adapter stderr: ...".
    pub fn stderr_tail(&self) -> String {
        let b = self.stderr.lock().unwrap_or_else(|e| e.into_inner());
        let t = crate::vars::scrub(String::from_utf8_lossy(&b).trim());
        if t.is_empty() {
            String::new()
        } else {
            let last: String = t.chars().rev().take(1000).collect::<Vec<_>>().into_iter().rev().collect();
            format!("; adapter stderr: {last}")
        }
    }

    #[cfg(test)]
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// stop + close stdin, a grace period, then kill the process group. Idempotent.
    pub fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        if let Some(mut i) = self.stdin.take() {
            let _ = i.write_all(b"{\"action\":\"stop\"}\n").and_then(|_| i.flush());
            drop(i); // EOF
        }
        let t0 = Instant::now();
        let exited = loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break true,
                Ok(None) if t0.elapsed() < self.grace => std::thread::sleep(Duration::from_millis(20)),
                _ => break false,
            }
        };
        self.kill_group(exited);
    }

    #[cfg(unix)]
    fn kill_group(&mut self, exited: bool) {
        let pg = self.child.id() as i32;
        unsafe {
            // whatever the adapter left in its group (its tunnel, a proxy) goes too
            if libc::kill(-pg, libc::SIGTERM) == 0 {
                let t0 = Instant::now();
                while t0.elapsed() < Duration::from_millis(if exited { 300 } else { 1000 }) && libc::kill(-pg, 0) == 0 {
                    if !exited {
                        let _ = self.child.try_wait();
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                libc::kill(-pg, libc::SIGKILL);
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    #[cfg(windows)]
    fn kill_group(&mut self, _exited: bool) {
        if let Some(j) = &self.job {
            j.terminate();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    #[cfg(not(any(unix, windows)))]
    fn kill_group(&mut self, _exited: bool) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------- every running adapter, for exit and Ctrl-C

type Slot = Arc<Mutex<Option<Running>>>;
static RUNNING: Mutex<Vec<Slot>> = Mutex::new(Vec::new());

/// The registry's handle to an adapter: `conn` keeps it; `stop_all` stops whatever is still running.
pub struct Handle(Slot);

impl Handle {
    pub fn stop(&self) {
        if let Some(mut r) = self.0.lock().unwrap_or_else(|e| e.into_inner()).take() {
            r.stop();
        }
    }
}

/// Move a started adapter into the registry.
pub fn keep(r: Running) -> Handle {
    let slot: Slot = Arc::new(Mutex::new(Some(r)));
    RUNNING.lock().unwrap_or_else(|e| e.into_inner()).push(Arc::clone(&slot));
    Handle(slot)
}

/// Stop every adapter this process started (in parallel: each gets its own grace period).
pub fn stop_all() {
    let slots: Vec<Slot> = std::mem::take(&mut *RUNNING.lock().unwrap_or_else(|e| e.into_inner()));
    let ts: Vec<_> = slots.into_iter().map(|s| std::thread::spawn(move || Handle(s).stop())).collect();
    for t in ts {
        let _ = t.join();
    }
}

/// Exit the process after stopping every adapter (process::exit runs no destructors).
pub fn exit(code: i32) -> ! {
    stop_all();
    std::process::exit(code)
}

// ---------------------------------------------------------------- Ctrl-C / SIGTERM: stop adapters, then exit

#[cfg(unix)]
static SIG_PIPE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

#[cfg(unix)]
extern "C" fn on_signal(_: libc::c_int) {
    let fd = SIG_PIPE.load(std::sync::atomic::Ordering::SeqCst);
    if fd >= 0 {
        unsafe {
            libc::write(fd, b"x".as_ptr() as *const libc::c_void, 1); // async-signal-safe; the thread does the rest
        }
    }
}

/// Installed with the first adapter: a signal now stops the adapters (stop, grace, kill group) before exiting.
fn install_signal_stop() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        #[cfg(unix)]
        unsafe {
            let mut fds = [0i32; 2];
            if libc::pipe(fds.as_mut_ptr()) != 0 {
                return;
            }
            SIG_PIPE.store(fds[1], std::sync::atomic::Ordering::SeqCst);
            let rd = fds[0];
            std::thread::spawn(move || {
                let mut b = [0u8; 1];
                if libc::read(rd, b.as_mut_ptr() as *mut libc::c_void, 1) == 1 {
                    stop_all();
                    std::process::exit(130);
                }
            });
            for s in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
                libc::signal(s, on_signal as *const () as libc::sighandler_t);
            }
        }
        #[cfg(windows)]
        unsafe {
            unsafe extern "system" fn on_ctrl(_: u32) -> i32 {
                stop_all();
                std::process::exit(130);
            }
            windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(on_ctrl), 1);
        }
    });
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// A fake adapter (sh): its behaviour is the "mode" in its config. It records what it saw in `dir`.
    pub const FAKE: &str = r#"#!/bin/sh
IFS= read -r line || exit 0
field() { printf '%s' "$line" | sed -n "s/.*\"$1\":\"\([^\"]*\)\".*/\1/p"; }
mode=$(field mode); dir=$(field dir); url=$(field url)
name=$(printf '%s' "$line" | sed -n 's/.*"name":"\([^"]*\)".*/\1/p')
echo "fake adapter: connecting with $url" >&2
case $mode in
  ready) printf '{"url":"%s","state":"ready","name":"%s"}\n' "$url" "$name" ;;
  error) echo "fake adapter: auth failed for $url" >&2; printf '{"url":"","state":"error: no route to db","name":"%s"}\n' "$name"; exit 1 ;;
  never) echo "fake adapter: still waiting for the tunnel" >&2; sleep 60; exit 0 ;;
  log_first) echo "connecting to the bastion..."; printf '{"url":"%s","state":"ready","name":"%s"}\n' "$url" "$name" ;;
  wrong_name) printf '{"url":"%s","state":"ready","name":"someone-else"}\n' "$url" ;;
  ignore_stop) trap '' TERM INT HUP; sleep 60 & echo $! > "$dir/grandchild"; printf '{"url":"%s","state":"ready","name":"%s"}\n' "$url" "$name" ;;
  exit_now) exit 4 ;;
esac
while IFS= read -r l; do
  case $l in *stop*) echo stop > "$dir/stopped"; [ "$mode" = ignore_stop ] && while :; do sleep 1; done; exit 0 ;; esac
done
echo eof > "$dir/eof"
"#;

    pub fn fake(dir: &Path) -> Vec<String> {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join("fake-adapter.sh");
        std::fs::write(&p, FAKE).unwrap();
        vec!["sh".into(), p.display().to_string()]
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pgbx-adp-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn cfg(mode: &str, d: &Path) -> Value {
        json!({"mode": mode, "dir": d.display().to_string(), "url": "postgres://app:Adapter-Pw-77@127.0.0.1:1/shop"})
    }

    fn alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[test]
    fn ready_then_stop() {
        let d = tmp("ready");
        let (mut r, ready) = Running::start(&fake(&d), "prod", &cfg("ready", &d), Duration::from_secs(5), None).unwrap();
        assert_eq!(ready.url, "postgres://app:Adapter-Pw-77@127.0.0.1:1/shop");
        assert_eq!(crate::vars::scrub("x Adapter-Pw-77 y"), "x *** y", "the url's password is registered for redaction");
        let pid = r.pid() as i32;
        r.stop();
        assert!(d.join("stopped").exists(), "the adapter got {{\"action\":\"stop\"}}");
        assert!(!alive(pid));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn error_state_is_shown_with_redacted_stderr() {
        let d = tmp("err");
        let e = Running::start(&fake(&d), "prod", &cfg("error", &d), Duration::from_secs(5), None).err().unwrap();
        assert!(e.contains("adapter state: error: no route to db"), "{e}");
        assert!(e.contains("auth failed for postgres://app:***@127.0.0.1:1/shop"), "{e}");
        assert!(!e.contains("Adapter-Pw-77"), "{e}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn never_ready_times_out_and_shows_stderr() {
        let d = tmp("never");
        let t0 = Instant::now();
        let e = Running::start(&fake(&d), "prod", &cfg("never", &d), Duration::from_millis(700), None).err().unwrap();
        assert!(e.contains("gave no result within 0.7 s"), "{e}");
        assert!(e.contains("still waiting for the tunnel"), "{e}");
        assert!(!e.contains("Adapter-Pw-77"), "{e}");
        assert!(t0.elapsed() < Duration::from_secs(4), "stopped within ready_timeout + grace ({:?})", t0.elapsed());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn wrong_name_and_log_on_stdout_are_protocol_errors() {
        let d = tmp("proto");
        let e = Running::start(&fake(&d), "prod", &cfg("wrong_name", &d), Duration::from_secs(5), None).err().unwrap();
        assert!(e.contains("answered for 'someone-else', not for profile 'prod'"), "{e}");
        let e = Running::start(&fake(&d), "prod", &cfg("log_first", &d), Duration::from_secs(5), None).err().unwrap();
        assert!(e.contains("protocol error") && e.contains("connecting to the bastion...") && e.contains("logs belong on stderr"), "{e}");
        let e = Running::start(&fake(&d), "prod", &cfg("exit_now", &d), Duration::from_secs(5), None).err().unwrap();
        assert!(e.contains("exited without a result"), "{e}");
        let e = Running::start(&["/no/such/adapter".to_string()], "prod", &json!({}), Duration::from_secs(1), None).err().unwrap();
        assert!(e.contains("cannot start adapter"), "{e}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn ignoring_stop_gets_the_group_killed_after_the_grace() {
        let d = tmp("ignore");
        let (mut r, _) = Running::start(&fake(&d), "prod", &cfg("ignore_stop", &d), Duration::from_secs(5), None).unwrap();
        r.grace = Duration::from_millis(500);
        let pid = r.pid() as i32;
        let gc: i32 = std::fs::read_to_string(d.join("grandchild")).unwrap().trim().parse().unwrap();
        assert!(alive(gc));
        let t0 = Instant::now();
        r.stop();
        assert!(d.join("stopped").exists());
        assert!(t0.elapsed() >= Duration::from_millis(500));
        assert!(!alive(pid), "adapter killed");
        std::thread::sleep(Duration::from_millis(100));
        assert!(!alive(gc), "its child in the same process group killed too");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn result_line_parsing() {
        assert_eq!(parse_result(r#"{"url":"postgres://h/d","state":"ready","name":"p"}"#, "p").unwrap().url, "postgres://h/d");
        assert!(parse_result(r#"{"url":"","state":"ready","name":"p"}"#, "p").unwrap_err().contains("url is empty"));
        assert!(parse_result(r#"{"url":"x","name":"p"}"#, "p").unwrap_err().contains("(no state)"));
        assert!(parse_result("[1]", "p").unwrap_err().contains("JSON object"));
        let e = parse_result("starting postgres://u:Leaky-Pw-1@h/d", "p").unwrap_err();
        assert!(e.contains("postgres://u:***@h/d") && !e.contains("Leaky-Pw-1"), "{e}");
    }
}
