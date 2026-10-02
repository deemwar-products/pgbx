//! TLS to Postgres, with libpq's `sslmode` semantics (rustls; no OpenSSL):
//!
//!   disable              plain only
//!   allow, prefer        TLS when the server offers it, else plain (prefer is the default, as in libpq)
//!   require              TLS, the certificate is not checked
//!   verify-ca            TLS, the certificate chains to a trusted root (the host name is not checked)
//!   verify-full          TLS, trusted root and the certificate names the host
//!
//! Trusted roots: `sslrootcert` (a PEM file; `system` = the defaults), else ~/.postgresql/root.crt when it exists
//! (as libpq), else the webpki roots plus the OS store.
//!
//! Where the settings come from, first match wins (per setting): the connection string (`?sslmode=` in a URL or
//! `sslmode=` in key=value form), the profile's `sslmode:` / `sslrootcert:` keys, PGSSLMODE / PGSSLROOTCERT, defaults.
//! Unix sockets never use TLS (libpq ignores sslmode there too).

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::WebPkiServerVerifier;
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, DigitallySignedStruct, Error as TlsError, RootCertStore, SignatureScheme};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_postgres_rustls::MakeRustlsConnect;

pub type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Disable,
    Allow,
    Prefer,
    Require,
    VerifyCa,
    VerifyFull,
}

impl Mode {
    pub fn parse(s: &str) -> Result<Mode, String> {
        Ok(match s.trim() {
            "disable" => Mode::Disable,
            "allow" => Mode::Allow,
            "prefer" => Mode::Prefer,
            "require" => Mode::Require,
            "verify-ca" => Mode::VerifyCa,
            "verify-full" => Mode::VerifyFull,
            x => return Err(format!("bad sslmode '{x}' (disable, allow, prefer, require, verify-ca, verify-full)")),
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Disable => "disable",
            Mode::Allow => "allow",
            Mode::Prefer => "prefer",
            Mode::Require => "require",
            Mode::VerifyCa => "verify-ca",
            Mode::VerifyFull => "verify-full",
        }
    }

    pub fn verifies(self) -> bool {
        matches!(self, Mode::VerifyCa | Mode::VerifyFull)
    }

    /// What the postgres crate is told: it knows disable/prefer/require; verification is ours.
    pub fn wire(self) -> postgres::config::SslMode {
        match self {
            Mode::Disable => postgres::config::SslMode::Disable,
            Mode::Allow | Mode::Prefer => postgres::config::SslMode::Prefer,
            Mode::Require | Mode::VerifyCa | Mode::VerifyFull => postgres::config::SslMode::Require,
        }
    }
}

/// sslmode / sslrootcert as written somewhere (a connection string, a profile), not yet checked.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Params {
    pub mode: Option<String>,
    pub rootcert: Option<String>,
}

impl Params {
    fn take(&mut self, k: &str, v: String) -> bool {
        match k {
            "sslmode" => self.mode = Some(v),
            "sslrootcert" => self.rootcert = Some(v),
            _ => return false,
        }
        true
    }
}

/// Take sslmode / sslrootcert out of a connection string (the postgres crate knows neither verify-* nor sslrootcert).
/// The rest of the string is returned unchanged.
pub fn split(conn: &str) -> (String, Params) {
    let mut p = Params::default();
    if conn.contains("://") {
        let (base, frag) = match conn.split_once('#') {
            Some((b, f)) => (b, Some(f)),
            None => (conn, None),
        };
        let Some((head, query)) = base.split_once('?') else { return (conn.to_string(), p) };
        let kept: Vec<&str> = query.split('&').filter(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            let dec = |s: &str| crate::ui::pct_decode(&s.replace('+', " ")).unwrap_or_else(|| s.to_string());
            !p.take(&dec(k), dec(v))
        }).collect();
        let mut out = head.to_string();
        if !kept.is_empty() {
            out.push('?');
            out.push_str(&kept.join("&"));
        }
        if let Some(f) = frag {
            out.push('#');
            out.push_str(f);
        }
        return (out, p);
    }
    // key=value form: values may be 'single quoted' with \' and \\ escapes, spaces allowed around '='
    let b = conn.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let ks = i;
        while i < b.len() && b[i] != b'=' && !b[i].is_ascii_whitespace() {
            i += 1;
        }
        let key = &conn[ks..i];
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() || b[i] != b'=' {
            if !conn[ks..].trim().is_empty() {
                out.push(' ');
                out.push_str(&conn[ks..]); // not key=value: leave it for the postgres crate to report
            }
            break;
        }
        i += 1;
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut val = String::new();
        if i < b.len() && b[i] == b'\'' {
            i += 1;
            while i < b.len() && b[i] != b'\'' {
                if b[i] == b'\\' && i + 1 < b.len() {
                    i += 1;
                }
                let c = conn[i..].chars().next().unwrap();
                val.push(c);
                i += c.len_utf8();
            }
            i = (i + 1).min(b.len());
        } else {
            let vs = i;
            while i < b.len() && !b[i].is_ascii_whitespace() {
                i += 1;
            }
            val.push_str(&conn[vs..i]);
        }
        if !p.take(key, val) {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(&conn[ks..i]);
        }
    }
    (out.trim().to_string(), p)
}

/// The settings in effect for one connection.
#[derive(Debug, Clone, PartialEq)]
pub struct Tls {
    pub mode: Mode,
    /// a PEM file of trusted roots (verify-* only); None = the webpki roots plus the OS store
    pub rootcert: Option<PathBuf>,
}

fn default_rootcert(env: Env) -> Option<PathBuf> {
    let p = if cfg!(windows) {
        PathBuf::from(env("APPDATA")?).join("postgresql").join("root.crt")
    } else {
        PathBuf::from(env("HOME")?).join(".postgresql").join("root.crt")
    };
    p.is_file().then_some(p)
}

/// Combine the connection string's settings, the profile's and the environment's (see the module doc).
pub fn resolve(url: &Params, profile: &Params, env: Env) -> Result<Tls, String> {
    let nonempty = |s: &Option<String>| s.clone().filter(|s| !s.trim().is_empty());
    let root = nonempty(&url.rootcert).or(nonempty(&profile.rootcert)).or(env("PGSSLROOTCERT"));
    let mode = match nonempty(&url.mode).or(nonempty(&profile.mode)).or(env("PGSSLMODE")) {
        Some(m) => Mode::parse(&m)?,
        None if root.as_deref() == Some("system") => Mode::VerifyFull, // libpq 16: sslrootcert=system defaults to verify-full
        None => Mode::Prefer,
    };
    let rootcert = match root.as_deref() {
        Some("system") => None,
        Some(r) => Some(PathBuf::from(crate::config::tilde(r))),
        None if mode.verifies() => default_rootcert(env),
        None => None,
    };
    Ok(Tls { mode, rootcert })
}

impl Tls {
    /// For libpq child tools (pg_restore): the same settings as environment variables.
    pub fn tool_env(&self) -> Vec<(&'static str, String)> {
        let mut v = vec![("PGSSLMODE", self.mode.as_str().to_string())];
        match &self.rootcert {
            Some(r) => v.push(("PGSSLROOTCERT", r.display().to_string())),
            // libpq would look for ~/.postgresql/root.crt; "system" (libpq 16+) is the trust store pgbx used
            None if self.mode.verifies() => v.push(("PGSSLROOTCERT", "system".to_string())),
            None => {}
        }
        v
    }

    /// The rustls connector for this mode.
    pub fn connector(&self) -> Result<MakeRustlsConnect, String> {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let verifier: Arc<dyn ServerCertVerifier> = if self.mode.verifies() {
            let roots = Arc::new(self.roots()?);
            let webpki = WebPkiServerVerifier::builder_with_provider(roots, provider.clone())
                .build()
                .map_err(|e| format!("TLS setup: {e}"))?;
            if self.mode == Mode::VerifyFull {
                webpki
            } else {
                Arc::new(VerifyCa(webpki))
            }
        } else {
            Arc::new(NoVerify(provider.clone()))
        };
        let cfg = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| format!("TLS setup: {e}"))?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();
        Ok(MakeRustlsConnect::new(cfg))
    }

    fn roots(&self) -> Result<RootCertStore, String> {
        let mut store = RootCertStore::empty();
        if let Some(p) = &self.rootcert {
            let certs = read_pem(p)?;
            let (added, _) = store.add_parsable_certificates(certs);
            if added == 0 {
                return Err(format!("sslrootcert {}: no usable certificate in it", p.display()));
            }
            return Ok(store);
        }
        store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        store.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
        Ok(store)
    }
}

fn read_pem(p: &Path) -> Result<Vec<CertificateDer<'static>>, String> {
    let it = CertificateDer::pem_file_iter(p).map_err(|e| format!("sslrootcert {}: {e}", p.display()))?;
    it.collect::<Result<Vec<_>, _>>().map_err(|e| format!("sslrootcert {}: {e}", p.display()))
}

/// require / prefer / allow: encrypted, but any certificate is accepted (libpq does the same). The handshake
/// signatures are still checked, so the session key belongs to whoever holds the certificate's key.
#[derive(Debug)]
struct NoVerify(Arc<CryptoProvider>);

impl ServerCertVerifier for NoVerify {
    fn verify_server_cert(&self, _: &CertificateDer<'_>, _: &[CertificateDer<'_>], _: &ServerName<'_>, _: &[u8], _: UnixTime)
        -> Result<ServerCertVerified, TlsError> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// verify-ca: the full webpki check, except that a certificate for another host name is accepted
/// (the chain is checked before the name, so a name error means the chain was good).
#[derive(Debug)]
struct VerifyCa(Arc<WebPkiServerVerifier>);

impl ServerCertVerifier for VerifyCa {
    fn verify_server_cert(&self, ee: &CertificateDer<'_>, im: &[CertificateDer<'_>], name: &ServerName<'_>, ocsp: &[u8], now: UnixTime)
        -> Result<ServerCertVerified, TlsError> {
        match self.0.verify_server_cert(ee, im, name, ocsp, now) {
            Err(TlsError::InvalidCertificate(CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. })) => {
                Ok(ServerCertVerified::assertion())
            }
            r => r,
        }
    }
    fn verify_tls12_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, TlsError> {
        self.0.verify_tls12_signature(m, c, d)
    }
    fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, TlsError> {
        self.0.verify_tls13_signature(m, c, d)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(mode: Option<&str>, root: Option<&str>) -> Params {
        Params { mode: mode.map(String::from), rootcert: root.map(String::from) }
    }

    #[test]
    fn modes_parse_and_map() {
        for m in ["disable", "allow", "prefer", "require", "verify-ca", "verify-full"] {
            assert_eq!(Mode::parse(m).unwrap().as_str(), m);
        }
        assert!(Mode::parse("verify_full").unwrap_err().contains("bad sslmode 'verify_full'"));
        assert!(Mode::parse("").is_err());
        use postgres::config::SslMode as W;
        assert_eq!(Mode::Allow.wire(), W::Prefer);
        assert_eq!(Mode::VerifyCa.wire(), W::Require);
        assert_eq!(Mode::Disable.wire(), W::Disable);
        assert!(Mode::VerifyFull.verifies() && Mode::VerifyCa.verifies() && !Mode::Require.verifies());
    }

    #[test]
    fn split_urls() {
        let (u, q) = split("postgres://u:p@h:5/d?sslmode=verify-full&connect_timeout=3&sslrootcert=%2Ftmp%2Fca.pem");
        assert_eq!(u, "postgres://u:p@h:5/d?connect_timeout=3");
        assert_eq!(q, p(Some("verify-full"), Some("/tmp/ca.pem")));
        let (u, q) = split("postgresql://h/d?sslmode=require");
        assert_eq!((u.as_str(), q), ("postgresql://h/d", p(Some("require"), None)));
        let (u, q) = split("postgres://h/d");
        assert_eq!((u.as_str(), q), ("postgres://h/d", Params::default()));
        // what is left parses with the postgres crate, which knows neither verify-full nor sslrootcert
        assert!(split("postgres://h/d?sslmode=verify-ca&sslrootcert=x").0.parse::<postgres::Config>().is_ok());
    }

    #[test]
    fn split_key_value() {
        let (s, q) = split("host=db sslmode=verify-ca  user=app sslrootcert='/a b/c\\'s.pem' dbname = shop");
        assert_eq!(s, "host=db user=app dbname = shop");
        assert_eq!(q, p(Some("verify-ca"), Some("/a b/c's.pem")));
        let c: postgres::Config = s.parse().unwrap();
        assert_eq!(c.get_dbname(), Some("shop"));
        let (s, q) = split("host=db");
        assert_eq!((s.as_str(), q), ("host=db", Params::default()));
    }

    #[test]
    fn precedence_url_then_profile_then_env_then_prefer() {
        let none = |_: &str| None;
        let env = |k: &str| match k {
            "PGSSLMODE" => Some("require".to_string()),
            "PGSSLROOTCERT" => Some("/env/ca.pem".to_string()),
            _ => None,
        };
        // nothing anywhere: prefer, like libpq
        assert_eq!(resolve(&Params::default(), &Params::default(), &none).unwrap(), Tls { mode: Mode::Prefer, rootcert: None });
        // env
        assert_eq!(resolve(&Params::default(), &Params::default(), &env).unwrap().mode, Mode::Require);
        // profile beats env
        let t = resolve(&Params::default(), &p(Some("verify-ca"), None), &env).unwrap();
        assert_eq!((t.mode, t.rootcert), (Mode::VerifyCa, Some(PathBuf::from("/env/ca.pem"))));
        // url beats profile, per setting
        let t = resolve(&p(Some("disable"), None), &p(Some("verify-ca"), Some("/prof/ca.pem")), &env).unwrap();
        assert_eq!((t.mode, t.rootcert), (Mode::Disable, Some(PathBuf::from("/prof/ca.pem"))));
        let t = resolve(&p(None, Some("/url/ca.pem")), &p(Some("verify-full"), Some("/prof/ca.pem")), &none).unwrap();
        assert_eq!((t.mode, t.rootcert), (Mode::VerifyFull, Some(PathBuf::from("/url/ca.pem"))));
        // an empty value does not count
        assert_eq!(resolve(&p(Some(""), None), &Params::default(), &env).unwrap().mode, Mode::Require);
        // sslrootcert=system: the default roots, verify-full unless a mode is given
        assert_eq!(resolve(&p(None, Some("system")), &Params::default(), &none).unwrap(), Tls { mode: Mode::VerifyFull, rootcert: None });
        assert_eq!(resolve(&p(Some("verify-ca"), Some("system")), &Params::default(), &none).unwrap().mode, Mode::VerifyCa);
        // a bad mode is an error wherever it comes from
        let bad = |k: &str| (k == "PGSSLMODE").then(|| "on".to_string());
        assert!(resolve(&Params::default(), &Params::default(), &bad).unwrap_err().contains("bad sslmode 'on'"));
    }

    #[test]
    fn default_root_crt_like_libpq() {
        let d = std::env::temp_dir().join(format!("pgbx-tls-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let ds = d.display().to_string();
        let env = move |k: &str| matches!(k, "HOME" | "APPDATA").then(|| ds.clone());
        let full = p(Some("verify-full"), None);
        assert_eq!(resolve(&full, &Params::default(), &env).unwrap().rootcert, None, "no file: the default roots");
        let f = if cfg!(windows) { d.join("postgresql") } else { d.join(".postgresql") };
        std::fs::create_dir_all(&f).unwrap();
        std::fs::write(f.join("root.crt"), "x").unwrap();
        assert_eq!(resolve(&full, &Params::default(), &env).unwrap().rootcert, Some(f.join("root.crt")));
        assert_eq!(resolve(&p(Some("require"), None), &Params::default(), &env).unwrap().rootcert, None, "only verify-* reads it");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn tool_env_and_connectors() {
        let t = Tls { mode: Mode::Require, rootcert: None };
        assert_eq!(t.tool_env(), [("PGSSLMODE", "require".to_string())]);
        assert!(t.connector().is_ok());
        let t = Tls { mode: Mode::VerifyFull, rootcert: None };
        assert_eq!(t.tool_env()[1], ("PGSSLROOTCERT", "system".to_string()));
        assert!(t.connector().is_ok(), "webpki + OS roots");
        let t = Tls { mode: Mode::VerifyCa, rootcert: Some(PathBuf::from("/nonexistent/pgbx/ca.pem")) };
        assert_eq!(t.tool_env()[1], ("PGSSLROOTCERT", "/nonexistent/pgbx/ca.pem".to_string()));
        assert!(t.connector().err().unwrap().contains("sslrootcert /nonexistent/pgbx/ca.pem"));
    }
}
