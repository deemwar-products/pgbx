//! The adapter protocol through the real pgbx binary (Unix): a one-off command stops its adapter, a killed pgbx
//! leaves its adapter to see EOF and exit, and the password never reaches output or disk.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const PW: &str = "It-Secret-Pw-123";

const FAKE: &str = r#"#!/bin/sh
IFS= read -r line || exit 0
field() { printf '%s' "$line" | sed -n "s/.*\"$1\":\"\([^\"]*\)\".*/\1/p"; }
dir=$(field dir); url=$(field pg_url)
name=$(printf '%s' "$line" | sed -n 's/.*"name":"\([^"]*\)".*/\1/p')
echo "fake: connecting with $url" >&2
printf '{"url":"%s","state":"ready","name":"%s"}\n' "$url" "$name"
echo $$ > "$dir/started"
while IFS= read -r l; do
  case $l in *stop*) echo stop > "$dir/stopped"; exit 0 ;; esac
done
echo eof > "$dir/eof"
"#;

fn setup(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pgbx-it-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("cfg")).unwrap();
    std::fs::write(d.join("fake.sh"), FAKE).unwrap();
    // the url points at a closed port: the connection fails, which is all these tests need
    std::fs::write(d.join("cfg/config.yaml"), format!(
        "adapters:\n  fake: sh {}\nprofiles:\n  p:\n    adapter: fake\n    dir: {}\n    pg_url: postgres://app:$PGBX_IT_PW@127.0.0.1:1/shop\n  \
         u:\n    url: postgres://app:$PGBX_IT_PW@127.0.0.1:1/shop\n",
        d.join("fake.sh").display(), d.display())).unwrap();
    d
}

fn pgbx(d: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_pgbx"));
    c.env("PGBX_CONFIG_DIR", d.join("cfg")).env("PGBX_IT_PW", PW).env_remove("PGBX_PROFILE").env_remove("PGBX_URL")
        .env("PGBX_MEMORY_DIR", d.join("mem"));
    c
}

fn wait_for(p: &Path, max: Duration) -> bool {
    let t0 = Instant::now();
    while t0.elapsed() < max {
        if p.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn no_secret_on_disk(d: &Path) {
    for e in std::fs::read_dir(d).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            no_secret_on_disk(&p);
        } else if let Ok(t) = std::fs::read_to_string(&p) {
            assert!(!t.contains(PW), "{} holds the password", p.display());
        }
    }
}

#[test]
fn one_off_command_stops_its_adapter_and_never_prints_the_password() {
    let d = setup("oneoff");
    let o = pgbx(&d).args(["query", "SELECT 1", "--profile", "p", "--json"]).output().unwrap();
    let out = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
    assert!(!o.status.success());
    assert!(out.contains("127.0.0.1:1") && out.contains("via adapter fake"), "{out}");
    assert!(!out.contains(PW), "{out}");
    assert!(d.join("started").exists() && d.join("stopped").exists(), "started, then got stop when the command ended");
    let pid = std::fs::read_to_string(d.join("started")).unwrap().trim().to_string();
    assert!(!Command::new("kill").args(["-0", &pid]).stderr(Stdio::null()).status().unwrap().success(), "adapter is gone");
    // a url profile: same, no adapter
    let o = pgbx(&d).args(["status", "--profile", "u", "--json"]).output().unwrap();
    let out = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
    assert!(out.contains("cannot connect") && !out.contains(PW), "{out}");
    // --url for one run, and a missing variable named, never valued
    let o = pgbx(&d).args(["status", "--url", "postgres://app:$PGBX_IT_MISSING@127.0.0.1:1/x", "--json"]).output().unwrap();
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(out.contains("$PGBX_IT_MISSING is not set"), "{out}");
    // profile show / list: references only
    let o = pgbx(&d).args(["profile", "list", "--json"]).output().unwrap();
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(out.contains("$PGBX_IT_PW") && !out.contains(PW), "{out}");
    no_secret_on_disk(&d);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn killed_pgbx_leaves_the_adapter_to_see_eof() {
    let d = setup("kill");
    let mut child = pgbx(&d).args(["serve", "--profile", "p", "--no-open", "--json"])
        .stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    assert!(wait_for(&d.join("started"), Duration::from_secs(10)), "serve started the adapter");
    child.kill().unwrap(); // SIGKILL: pgbx gets no chance to say stop
    let _ = child.wait();
    assert!(wait_for(&d.join("eof"), Duration::from_secs(5)), "the adapter saw stdin close and exited");
    assert!(!d.join("stopped").exists());
    no_secret_on_disk(&d);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn ctrl_c_on_serve_stops_the_adapter_cleanly() {
    let d = setup("sigint");
    let mut child = pgbx(&d).args(["serve", "--profile", "p", "--no-open", "--json"])
        .stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    assert!(wait_for(&d.join("started"), Duration::from_secs(10)), "serve started the adapter");
    Command::new("kill").args(["-INT", &child.id().to_string()]).status().unwrap();
    let st = child.wait().unwrap();
    assert_eq!(st.code(), Some(130));
    assert!(d.join("stopped").exists(), "the adapter got {{\"action\":\"stop\"}} before pgbx exited");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn host_side_commands_and_removed_ssh_say_what_to_do() {
    let d = setup("host");
    for cmd in [vec!["diagnose", "--profile", "p", "--json"], vec!["setup", "server", "--profile", "p", "--json"]] {
        let o = pgbx(&d).args(&cmd).output().unwrap();
        let out = String::from_utf8_lossy(&o.stdout);
        assert!(!o.status.success() && out.contains("on the database host"), "{out}");
    }
    assert!(!d.join("started").exists(), "no adapter started for a host-side command");
    let o = pgbx(&d).args(["status", "--ssh", "ops@db1", "--json"]).output().unwrap();
    assert!(String::from_utf8_lossy(&o.stdout).contains("ssh adapter"));
    let o = pgbx(&d).args(["tunnel", "list"]).output().unwrap();
    assert!(!o.status.success() && String::from_utf8_lossy(&o.stderr).contains("no built-in SSH"));
    let _ = std::fs::remove_dir_all(&d);
}
