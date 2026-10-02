//! Native notifications: Slack, Telegram, generic webhook, email (SMTP). No pgrx here (unit-tested alone).
//!
//!   pgbx.notify              = 'slack:ops, telegram:oncall, webhook:pager, email:dba'   (names only)
//!   pgbx.notify_secrets_file = '/etc/pgbx/notify.secrets'   (chmod 600, owned by postgres)
//!
//! URLs and tokens live ONLY in the secrets file, never in a setting (settings are visible to every role):
//!   slack.ops.url        = https://hooks.slack.com/services/T000/B000/XXXX
//!   telegram.oncall.token   = 123456:ABC...        telegram.oncall.chat_id = -1001234
//!   webhook.pager.url    = https://example.com/hook       webhook.pager.header = Authorization: Bearer ...
//!   email.dba.smtp       = smtps://user:password@smtp.example.com:465   (smtp://host:587 uses STARTTLS)
//!   email.dba.from       = pgbx@example.com                email.dba.to = dba@example.com, ops@example.com
//! A channel without a name ("slack") uses the name "default" (slack.default.url = ...).
//!
//! What is sent: a failed job (backup / restore / verify) once per incident — the same database + job kind +
//! error is not repeated within REPEAT_AFTER — and one "OK again" message when that database's job of the
//! same kind next succeeds. pgbx.alert_command keeps running on every failure, unchanged.

use std::collections::HashMap;
use std::time::{Duration, Instant};

pub const REPEAT_AFTER: Duration = Duration::from_secs(6 * 3600);
const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq)]
pub struct Channel {
    pub kind: String, // slack | telegram | webhook | email
    pub name: String,
}

pub fn parse_channels(spec: &str) -> Result<Vec<Channel>, String> {
    let mut out = vec![];
    for part in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let (kind, name) = part.split_once(':').map(|(k, n)| (k.trim(), n.trim())).unwrap_or((part, "default"));
        let kind = kind.to_ascii_lowercase();
        if !["slack", "telegram", "webhook", "email"].contains(&kind.as_str()) {
            return Err(format!("pgbx.notify: unknown channel '{kind}' (slack, telegram, webhook, email)"));
        }
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            return Err(format!(
                // never echo what was written: it may be the secret URL itself
                "pgbx.notify: the {kind} channel needs a NAME (letters, digits, _ -), not a URL — URLs and tokens go in pgbx.notify_secrets_file"
            ));
        }
        out.push(Channel { kind, name: name.to_string() });
    }
    Ok(out)
}

pub fn parse_secrets(text: &str) -> HashMap<String, String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_once('=').map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string())))
        .collect()
}

pub fn load_secrets(path: &str) -> Result<HashMap<String, String>, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let m = std::fs::metadata(path).map_err(|e| format!("notify secrets file {path}: {e}"))?;
        if m.permissions().mode() & 0o077 != 0 {
            return Err(format!("notify secrets file {path} is readable by group/others; chmod 600 it"));
        }
    }
    Ok(parse_secrets(&std::fs::read_to_string(path).map_err(|e| format!("notify secrets file {path}: {e}"))?))
}

#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub failed: bool, // false = recovered
    pub server: String,
    pub database: String,
    pub kind: String,
    pub job_id: i64,
    pub error: String,
    pub at: String,
}

impl Event {
    pub fn title(&self) -> String {
        if self.failed {
            format!("pgbx: {} of {} on {} FAILED", self.kind, self.database, self.server)
        } else {
            format!("pgbx: {} of {} on {} is OK again", self.kind, self.database, self.server)
        }
    }
    pub fn text(&self) -> String {
        if self.failed {
            format!("{}\n{}\njob {} at {} — check: pgbx status --db {}", self.title(), self.error, self.job_id, self.at, self.database)
        } else {
            format!("{}\njob {} at {}", self.title(), self.job_id, self.at)
        }
    }
    pub fn json(&self) -> String {
        format!(
            "{{\"event\":\"{}\",\"server\":{},\"database\":{},\"kind\":{},\"job_id\":{},\"error\":{},\"at\":{},\"text\":{}}}",
            if self.failed { "failure" } else { "recovered" },
            js(&self.server), js(&self.database), js(&self.kind), self.job_id,
            if self.failed { js(&self.error) } else { "null".into() }, js(&self.at), js(&self.text())
        )
    }
}

pub fn js(s: &str) -> String {
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

/// One incident per (database, kind): decides whether an event is sent.
#[derive(Default)]
pub struct Dedup {
    open: HashMap<(String, String), (String, Instant)>, // -> (error, last sent)
}

impl Dedup {
    /// Failure: send if no open incident, the error changed, or REPEAT_AFTER passed. Success: send "OK again"
    /// only when an incident was open (and close it).
    pub fn should_send(&mut self, db: &str, kind: &str, failed: bool, error: &str, now: Instant) -> bool {
        let k = (db.to_string(), kind.to_string());
        if !failed {
            return self.open.remove(&k).is_some();
        }
        match self.open.get(&k) {
            Some((e, t)) if e == error && now.duration_since(*t) < REPEAT_AFTER => false,
            _ => {
                self.open.insert(k, (error.to_string(), now));
                true
            }
        }
    }
}

fn need<'a>(s: &'a HashMap<String, String>, k: &str) -> Result<&'a str, String> {
    s.get(k).map(String::as_str).filter(|v| !v.is_empty()).ok_or(format!("{k} is missing in pgbx.notify_secrets_file"))
}

fn post_json(url: &str, body: &str, header: Option<&str>) -> Result<(), String> {
    let agent = ureq::AgentBuilder::new().timeout(TIMEOUT).build();
    let mut req = agent.post(url).set("Content-Type", "application/json");
    if let Some((k, v)) = header.and_then(|h| h.split_once(':')) {
        req = req.set(k.trim(), v.trim());
    }
    match req.send_string(body) {
        Ok(_) => Ok(()),
        // never echo the URL: it is the secret for Slack/webhooks
        Err(ureq::Error::Status(code, _)) => Err(format!("HTTP {code}")),
        Err(e) => Err(redact_url(&e.to_string(), url)),
    }
}

fn redact_url(msg: &str, url: &str) -> String {
    let m = msg.replace(url, "<url>");
    m.split_whitespace().map(|w| if w.contains("://") { "<url>" } else { w }).collect::<Vec<_>>().join(" ")
}

/// Send to one channel. Errors never contain a URL or token.
pub fn send(ch: &Channel, s: &HashMap<String, String>, ev: &Event) -> Result<(), String> {
    let p = format!("{}.{}.", ch.kind, ch.name);
    match ch.kind.as_str() {
        "slack" => post_json(need(s, &format!("{p}url"))?, &format!("{{\"text\":{}}}", js(&ev.text())), None),
        "webhook" => post_json(need(s, &format!("{p}url"))?, &ev.json(), s.get(&format!("{p}header")).map(String::as_str)),
        "telegram" => {
            let api = s.get(&format!("{p}api")).map(String::as_str).unwrap_or("https://api.telegram.org");
            let url = format!("{}/bot{}/sendMessage", api.trim_end_matches('/'), need(s, &format!("{p}token"))?);
            let body = format!(
                "{{\"chat_id\":{},\"text\":{},\"disable_web_page_preview\":true}}",
                js(need(s, &format!("{p}chat_id"))?), js(&ev.text())
            );
            post_json(&url, &body, None)
        }
        "email" => email(need(s, &format!("{p}smtp"))?, need(s, &format!("{p}from"))?, need(s, &format!("{p}to"))?, ev),
        k => Err(format!("unknown channel {k}")),
    }
}

fn email(smtp: &str, from: &str, to: &str, ev: &Event) -> Result<(), String> {
    use lettre::message::{header::ContentType, Mailbox, Message};
    use lettre::transport::smtp::{authentication::Credentials, client::Tls, client::TlsParameters, SmtpTransport};
    use lettre::Transport;
    let (scheme, rest) = smtp.split_once("://").ok_or("email smtp must look like smtps://user:pass@host:465")?;
    let (auth, hostport) = match rest.rsplit_once('@') {
        Some((a, h)) => (Some(a), h),
        None => (None, rest),
    };
    let (host, port) = match hostport.trim_end_matches('/').rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().map_err(|_| "email smtp: bad port")?),
        None => (hostport.trim_end_matches('/'), if scheme == "smtps" { 465 } else { 587 }),
    };
    let tls = || TlsParameters::new(host.to_string()).map_err(|e| format!("email tls: {e}"));
    let mut b = SmtpTransport::builder_dangerous(host).port(port).timeout(Some(TIMEOUT));
    b = match scheme {
        "smtps" => b.tls(Tls::Wrapper(tls()?)),
        "smtp" => b.tls(Tls::Opportunistic(tls()?)),
        "smtp+plain" => b.tls(Tls::None), // local relays / tests only
        s => return Err(format!("email smtp scheme '{s}': use smtps:// (465) or smtp:// (STARTTLS)")),
    };
    if let Some((u, pw)) = auth.and_then(|a| a.split_once(':')) {
        b = b.credentials(Credentials::new(pct(u), pct(pw)));
    }
    let mut m = Message::builder().from(from.parse::<Mailbox>().map_err(|_| "email from: not an address")?).subject(ev.title());
    for t in to.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        m = m.to(t.parse::<Mailbox>().map_err(|_| format!("email to: '{t}' is not an address"))?);
    }
    let msg = m.header(ContentType::TEXT_PLAIN).body(ev.text()).map_err(|e| format!("email: {e}"))?;
    b.build().send(&msg).map(|_| ()).map_err(|e| format!("email: {}", redact_url(&e.to_string(), smtp)))
}

fn pct(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Send an event to every configured channel; returns one line per channel that failed (for the log).
pub fn send_all(spec: &str, secrets_file: Option<&str>, ev: &Event) -> Vec<String> {
    let chans = match parse_channels(spec) {
        Ok(c) if c.is_empty() => return vec![],
        Ok(c) => c,
        Err(e) => return vec![e],
    };
    let secrets = match secrets_file.map(load_secrets) {
        Some(Ok(s)) => s,
        Some(Err(e)) => return vec![e],
        None => return vec!["pgbx.notify is set but pgbx.notify_secrets_file is not".into()],
    };
    chans
        .iter()
        .filter_map(|c| send(c, &secrets, ev).err().map(|e| format!("notify {}:{}: {e}", c.kind, c.name)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    fn ev(failed: bool) -> Event {
        Event { failed, server: "db1".into(), database: "shop".into(), kind: "backup".into(), job_id: 42,
                error: "pg_dump: \"x\" failed".into(), at: "2026-10-01T02:00:00Z".into() }
    }

    #[test]
    fn channels_are_names_not_urls() {
        let c = parse_channels("slack:ops, telegram , webhook:pager,email:dba").unwrap();
        assert_eq!(c[0], Channel { kind: "slack".into(), name: "ops".into() });
        assert_eq!(c[1].name, "default");
        let e = parse_channels("slack:https://hooks.slack.com/x").unwrap_err();
        assert!(e.contains("secrets_file") && !e.contains("hooks.slack.com"), "{e}");
        assert!(parse_channels("pager:x").unwrap_err().contains("unknown channel"));
        assert!(parse_channels("").unwrap().is_empty());
        let s = parse_secrets("# c\nslack.ops.url = https://h/x=y\n\nEMAIL.dba.to=a@b\n");
        assert_eq!(s["slack.ops.url"], "https://h/x=y");
        assert_eq!(s["email.dba.to"], "a@b");
    }

    #[test]
    fn dedup_one_message_per_incident() {
        let mut d = Dedup::default();
        let t = Instant::now();
        assert!(d.should_send("shop", "backup", true, "e1", t));
        assert!(!d.should_send("shop", "backup", true, "e1", t + Duration::from_secs(60)));
        assert!(d.should_send("shop", "verify", true, "e1", t), "other kind = other incident");
        assert!(d.should_send("shop", "backup", true, "e2", t), "new error is news");
        assert!(d.should_send("shop", "backup", true, "e2", t + REPEAT_AFTER + Duration::from_secs(1)), "reminder");
        assert!(d.should_send("shop", "backup", false, "", t), "recovered once");
        assert!(!d.should_send("shop", "backup", false, "", t), "no recovery spam");
        assert!(!d.should_send("other", "backup", false, "", t), "success without incident is silent");
    }

    #[test]
    fn message_presets() {
        let j = ev(true).json();
        assert!(j.contains("\"event\":\"failure\"") && j.contains("\"database\":\"shop\"") && j.contains("\\\"x\\\""));
        assert!(ev(false).json().contains("\"error\":null"));
        assert!(ev(true).text().contains("FAILED") && ev(false).title().contains("OK again"));
    }

    /// A one-shot HTTP server: returns (base url, handle yielding (request line, headers, body)).
    fn http_once() -> (String, std::thread::JoinHandle<(String, String, String)>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let h = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            let (mut headers, mut len) = (String::new(), 0usize);
            loop {
                let mut h = String::new();
                r.read_line(&mut h).unwrap();
                if h.trim().is_empty() {
                    break;
                }
                if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
                headers.push_str(&h);
            }
            let mut body = vec![0; len];
            r.read_exact(&mut body).unwrap();
            let mut s = s;
            s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").unwrap();
            (line, headers, String::from_utf8(body).unwrap())
        });
        (url, h)
    }

    #[test]
    fn slack_webhook_telegram_payloads() {
        let (url, h) = http_once();
        let s = parse_secrets(&format!("slack.ops.url={url}/services/T/B/X"));
        send(&Channel { kind: "slack".into(), name: "ops".into() }, &s, &ev(true)).unwrap();
        let (line, _, body) = h.join().unwrap();
        assert!(line.starts_with("POST /services/T/B/X"));
        assert!(body.starts_with("{\"text\":\"pgbx: backup of shop on db1 FAILED"));

        let (url, h) = http_once();
        let s = parse_secrets(&format!("webhook.p.url={url}/hook\nwebhook.p.header=Authorization: Bearer t0k"));
        send(&Channel { kind: "webhook".into(), name: "p".into() }, &s, &ev(false)).unwrap();
        let (_, headers, body) = h.join().unwrap();
        assert!(headers.to_ascii_lowercase().contains("authorization: bearer t0k"));
        assert!(body.contains("\"event\":\"recovered\""));

        let (url, h) = http_once();
        let s = parse_secrets(&format!("telegram.default.token=123:ABC\ntelegram.default.chat_id=-100\ntelegram.default.api={url}"));
        send(&Channel { kind: "telegram".into(), name: "default".into() }, &s, &ev(true)).unwrap();
        let (line, _, body) = h.join().unwrap();
        assert!(line.starts_with("POST /bot123:ABC/sendMessage"));
        assert!(body.contains("\"chat_id\":\"-100\"") && body.contains("FAILED"));
    }

    #[test]
    fn errors_never_leak_the_url() {
        let s = parse_secrets("slack.ops.url=http://127.0.0.1:9/services/SECRET123");
        let e = send(&Channel { kind: "slack".into(), name: "ops".into() }, &s, &ev(true)).unwrap_err();
        assert!(!e.contains("SECRET123"), "{e}");
        let e = send(&Channel { kind: "slack".into(), name: "nope".into() }, &s, &ev(true)).unwrap_err();
        assert!(e.contains("slack.nope.url is missing"));
    }

    #[test]
    fn email_over_smtp() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut w = s;
            let mut got = String::new();
            w.write_all(b"220 fake ESMTP\r\n").unwrap();
            let mut data = false;
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                got.push_str(&line);
                if data {
                    if line == ".\r\n" {
                        data = false;
                        w.write_all(b"250 queued\r\n").unwrap();
                    }
                    continue;
                }
                let up = line.to_ascii_uppercase();
                let reply: &[u8] = if up.starts_with("EHLO") { b"250-fake\r\n250 OK\r\n" }
                    else if up.starts_with("DATA") { data = true; b"354 go\r\n" }
                    else if up.starts_with("QUIT") { w.write_all(b"221 bye\r\n").unwrap(); break }
                    else { b"250 OK\r\n" };
                w.write_all(reply).unwrap();
            }
            got
        });
        let s = parse_secrets(&format!(
            "email.dba.smtp=smtp+plain://127.0.0.1:{port}\nemail.dba.from=pgbx@example.com\nemail.dba.to=dba@example.com, ops@example.com"
        ));
        send(&Channel { kind: "email".into(), name: "dba".into() }, &s, &ev(true)).unwrap();
        let got = h.join().unwrap();
        assert!(got.contains("RCPT TO:<dba@example.com>") && got.contains("RCPT TO:<ops@example.com>"));
        assert!(got.contains("Subject: pgbx: backup of shop on db1 FAILED"));
    }
}
