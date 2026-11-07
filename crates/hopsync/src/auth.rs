//! Admin authentication (Phase 2 step 4, plan.md §7): first-run setup, password login, sessions,
//! throttling and the CSRF origin rule. HTTP lives in `api`; this module only decides.
//!
//! - No default password. Until setup is done, the daemon holds a one-time setup token, written
//!   to a 0600 file in the data directory (never to the log). Setup needs that token, sets the
//!   admin password, and deletes the file.
//! - Passwords are hashed with argon2id (m = 19 MiB, t = 2, p = 1). One hash runs at a time, so
//!   a burst of logins cannot multiply that memory.
//! - A session token is 256 random bits. The store keeps only its SHA-256, so a leaked database
//!   gives no usable session. Sessions end after 12 hours idle or 7 days, and all of them end when
//!   the password changes.
//! - Failed logins from one address are throttled: after 3 failures, each further attempt must
//!   wait twice as long as the last (capped at 5 minutes). A successful login clears the count.

use std::collections::HashMap;
use std::io::Write;
use std::net::IpAddr;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use argon2::{Algorithm, Argon2, Params, PasswordHasher, PasswordVerifier, Version};
use aws_lc_rs::digest;

use crate::store::{self, Store, StoreError};

pub const SESSION_IDLE_SECS: i64 = 12 * 3600;
pub const SESSION_MAX_SECS: i64 = 7 * 86400;
pub const MIN_PASSWORD: usize = 12;
/// argon2 hashes any length, but there is no reason to accept a megabyte of password.
pub const MAX_PASSWORD: usize = 1024;
const FREE_FAILURES: u32 = 3;
const MAX_DELAY: Duration = Duration::from_secs(300);
/// Distinct addresses tracked; the oldest entry goes when full, so a flood of addresses cannot
/// grow memory.
const THROTTLE_ENTRIES: usize = 4096;

#[derive(Debug)]
pub enum AuthError {
    /// Setup has not been done yet.
    SetupRequired,
    SetupDone,
    /// Wrong setup token, password, or session. Deliberately says nothing more.
    Denied,
    Throttled(Duration),
    WeakPassword(String),
    Store(StoreError),
    Internal(String),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SetupRequired => f.write_str("setup is required"),
            Self::SetupDone => f.write_str("setup is already done"),
            Self::Denied => f.write_str("not authorized"),
            Self::Throttled(d) => write!(f, "too many attempts; retry in {} s", d.as_secs().max(1)),
            Self::WeakPassword(why) => f.write_str(why),
            Self::Store(e) => write!(f, "{e}"),
            Self::Internal(e) => f.write_str(e),
        }
    }
}

impl std::error::Error for AuthError {}

impl From<StoreError> for AuthError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

struct Failures {
    count: u32,
    /// No attempt before this.
    until: Instant,
    last: Instant,
}

pub struct Auth {
    store: Arc<Store>,
    setup_token: Mutex<Option<String>>,
    /// Where the setup token is written for the admin to read.
    token_file: Option<PathBuf>,
    failures: Mutex<HashMap<IpAddr, Failures>>,
    /// One argon2 computation at a time.
    hashing: Mutex<()>,
}

fn hasher() -> Argon2<'static> {
    // These parameters are valid constants; `Params::new` only rejects out-of-range values.
    let params = Params::new(19 * 1024, 2, 1, None).unwrap_or_default();
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

fn random_hex(bytes: usize) -> Result<String, AuthError> {
    let mut buf = vec![0u8; bytes];
    aws_lc_rs::rand::fill(&mut buf).map_err(|_| AuthError::Internal("random failed".into()))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

fn sha256(s: &str) -> Vec<u8> {
    digest::digest(&digest::SHA256, s.as_bytes())
        .as_ref()
        .to_vec()
}

pub fn check_password_rules(p: &str) -> Result<(), AuthError> {
    if p.chars().count() < MIN_PASSWORD {
        return Err(AuthError::WeakPassword(format!(
            "the password needs at least {MIN_PASSWORD} characters"
        )));
    }
    if p.len() > MAX_PASSWORD {
        return Err(AuthError::WeakPassword(format!(
            "the password may have at most {MAX_PASSWORD} bytes"
        )));
    }
    Ok(())
}

impl Auth {
    /// Before setup, creates the one-time setup token (see `setup_token`) and, given
    /// `token_file`, writes it there with mode 0600, replacing a token from an earlier start.
    pub fn new(store: Arc<Store>, token_file: Option<&Path>) -> Result<Self, AuthError> {
        let token = match store.admin_hash()? {
            None => Some(random_hex(16)?),
            Some(_) => None,
        };
        if let Some(path) = token_file {
            let write = |t: &str| -> std::io::Result<()> {
                match std::fs::remove_file(path) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
                    _ => {}
                }
                let mut f = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(path)?;
                writeln!(f, "{t}")
            };
            match &token {
                Some(t) => {
                    write(t).map_err(|e| AuthError::Internal(format!("{}: {e}", path.display())))?
                }
                // Setup is done: a leftover token file is useless; remove it.
                None => {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
        Ok(Self {
            store,
            token_file: token_file.map(Path::to_path_buf),
            setup_token: Mutex::new(token),
            failures: Mutex::new(HashMap::new()),
            hashing: Mutex::new(()),
        })
    }

    /// The token setup needs, while setup is pending. The daemon prints it once at start.
    pub fn setup_token(&self) -> Option<String> {
        self.setup_token.lock().ok().and_then(|t| t.clone())
    }

    pub fn needs_setup(&self) -> bool {
        self.setup_token().is_some()
    }

    fn hash(&self, password: &str) -> Result<String, AuthError> {
        let _one = self.hashing.lock().unwrap_or_else(|p| p.into_inner());
        let mut salt = [0u8; 16];
        aws_lc_rs::rand::fill(&mut salt)
            .map_err(|_| AuthError::Internal("random failed".into()))?;
        hasher()
            .hash_password_with_salt(password.as_bytes(), &salt)
            .map(|h| h.to_string())
            .map_err(|e| AuthError::Internal(format!("password hash: {e}")))
    }

    fn verify(&self, password: &str, hash: &str) -> bool {
        let _one = self.hashing.lock().unwrap_or_else(|p| p.into_inner());
        let matches = hasher().verify_password(password.as_bytes(), hash).is_ok();
        // Argon2 has just used 19 MiB, which an idle daemon should not keep.
        crate::pool::release_free_memory();
        matches
    }

    /// Sets the admin password with the setup token; returns a session token.
    pub fn setup(&self, token: &str, password: &str, from: IpAddr) -> Result<String, AuthError> {
        self.throttle(from)?;
        let expected = self.setup_token().ok_or(AuthError::SetupDone)?;
        // Compare digests so the comparison takes the same time whatever the input length.
        let ok =
            aws_lc_rs::constant_time::verify_slices_are_equal(&sha256(token), &sha256(&expected))
                .is_ok();
        if !ok {
            self.failed(from);
            return Err(AuthError::Denied);
        }
        check_password_rules(password)?;
        let hash = self.hash(password)?;
        self.store.create_admin(&hash).map_err(|e| match e {
            StoreError::Conflict(_) => AuthError::SetupDone,
            e => e.into(),
        })?;
        if let Ok(mut t) = self.setup_token.lock() {
            *t = None;
        }
        if let Some(path) = &self.token_file {
            let _ = std::fs::remove_file(path);
        }
        self.store.audit("setup", "admin password set")?;
        self.succeeded(from);
        self.new_session()
    }

    pub fn login(&self, password: &str, from: IpAddr) -> Result<String, AuthError> {
        if self.needs_setup() {
            return Err(AuthError::SetupRequired);
        }
        self.throttle(from)?;
        let hash = self.store.admin_hash()?.ok_or(AuthError::SetupRequired)?;
        if password.len() > MAX_PASSWORD || !self.verify(password, &hash) {
            self.failed(from);
            let _ = self.store.audit("login-failed", &from.to_string());
            return Err(AuthError::Denied);
        }
        self.succeeded(from);
        self.store.audit("login", &from.to_string())?;
        self.new_session()
    }

    fn new_session(&self) -> Result<String, AuthError> {
        let token = random_hex(32)?;
        self.store.add_session(&sha256(&token))?;
        Ok(token)
    }

    /// Accepts a live session and records its use.
    pub fn check(&self, token: &str) -> Result<(), AuthError> {
        if token.len() != 64 {
            return Err(AuthError::Denied);
        }
        let key = sha256(token);
        let (created, seen) = self.store.session(&key)?.ok_or(AuthError::Denied)?;
        let now = store::now();
        if now - created > SESSION_MAX_SECS || now - seen > SESSION_IDLE_SECS {
            self.store.delete_session(&key)?;
            return Err(AuthError::Denied);
        }
        // Writing on every request would cost a disk write per API call; a minute is precise
        // enough for a 12-hour idle limit.
        if now - seen >= 60 {
            self.store.touch_session(&key, now)?;
        }
        Ok(())
    }

    /// Whether a session is still live, without recording a use: an open event stream must not
    /// keep an idle session alive.
    pub fn session_live(&self, token: &str) -> bool {
        if token.len() != 64 {
            return false;
        }
        let now = store::now();
        matches!(
            self.store.session(&sha256(token)),
            Ok(Some((created, seen))) if now - created <= SESSION_MAX_SECS && now - seen <= SESSION_IDLE_SECS
        )
    }

    pub fn logout(&self, token: &str) -> Result<(), AuthError> {
        self.store.delete_session(&sha256(token))?;
        Ok(())
    }

    /// Changes the password after checking the current one; every session ends, and the caller
    /// gets a fresh one.
    pub fn change_password(
        &self,
        current: &str,
        new: &str,
        from: IpAddr,
    ) -> Result<String, AuthError> {
        self.throttle(from)?;
        let hash = self.store.admin_hash()?.ok_or(AuthError::SetupRequired)?;
        if current.len() > MAX_PASSWORD || !self.verify(current, &hash) {
            self.failed(from);
            return Err(AuthError::Denied);
        }
        check_password_rules(new)?;
        let new_hash = self.hash(new)?;
        self.store.set_admin_hash(&new_hash)?;
        self.store.audit("password-changed", &from.to_string())?;
        self.new_session()
    }

    pub fn prune_sessions(&self) -> Result<usize, AuthError> {
        let now = store::now();
        Ok(self
            .store
            .prune_sessions(now - SESSION_MAX_SECS, now - SESSION_IDLE_SECS)?)
    }

    fn throttle(&self, from: IpAddr) -> Result<(), AuthError> {
        let failures = self.failures.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(f) = failures.get(&from) {
            let now = Instant::now();
            if f.until > now {
                return Err(AuthError::Throttled(f.until - now));
            }
        }
        Ok(())
    }

    fn failed(&self, from: IpAddr) {
        let mut failures = self.failures.lock().unwrap_or_else(|p| p.into_inner());
        if failures.len() >= THROTTLE_ENTRIES && !failures.contains_key(&from) {
            let oldest = failures
                .iter()
                .min_by_key(|(_, f)| f.last)
                .map(|(ip, _)| *ip);
            if let Some(ip) = oldest {
                failures.remove(&ip);
            }
        }
        let now = Instant::now();
        let f = failures.entry(from).or_insert(Failures {
            count: 0,
            until: now,
            last: now,
        });
        f.count += 1;
        f.last = now;
        if f.count >= FREE_FAILURES {
            let doublings = (f.count - FREE_FAILURES).min(16);
            f.until = now + Duration::from_secs(1u64 << doublings).min(MAX_DELAY);
        }
    }

    fn succeeded(&self, from: IpAddr) {
        let mut failures = self.failures.lock().unwrap_or_else(|p| p.into_inner());
        failures.remove(&from);
    }
}

/// The CSRF rule for state-changing requests (anything but GET and HEAD): the browser must say
/// the request comes from this origin. `Sec-Fetch-Site: same-origin` is enough; otherwise the
/// `Origin` header's host must equal the `Host` header. A request with neither is refused, except
/// from the local CLI, which the caller never passes here.
pub fn same_origin(origin: Option<&str>, sec_fetch_site: Option<&str>, host: Option<&str>) -> bool {
    if sec_fetch_site == Some("same-origin") {
        return true;
    }
    if sec_fetch_site.is_some_and(|s| s != "none") {
        // cross-site or same-site: a different origin sent it.
        return false;
    }
    let (Some(origin), Some(host)) = (origin, host) else {
        return false;
    };
    let Some(rest) = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
    else {
        return false;
    };
    rest.eq_ignore_ascii_case(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    const IP: IpAddr = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
    const OTHER: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(192, 0, 2, 1));

    fn auth() -> (tempfile::TempDir, Arc<Store>, Auth) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(Store::open(&dir.path().join("hopsync.db")).expect("store"));
        let auth = Auth::new(store.clone(), None).expect("auth");
        (dir, store, auth)
    }

    #[test]
    fn setup_needs_the_token_and_runs_once() {
        let (_d, store, auth) = auth();
        let token = auth.setup_token().expect("token before setup");
        assert_eq!(token.len(), 32);
        assert!(matches!(
            auth.login("correct horse battery", IP),
            Err(AuthError::SetupRequired)
        ));
        assert!(matches!(
            auth.setup("wrong", "correct horse battery", IP),
            Err(AuthError::Denied)
        ));
        assert!(matches!(
            auth.setup(&token, "short", IP),
            Err(AuthError::WeakPassword(_))
        ));
        let session = auth
            .setup(&token, "correct horse battery", IP)
            .expect("setup");
        auth.check(&session).expect("setup logs in");
        assert!(auth.setup_token().is_none());
        assert!(matches!(
            auth.setup(&token, "another long password", IP),
            Err(AuthError::SetupDone)
        ));
        let hash = store.admin_hash().expect("hash").expect("set");
        assert!(
            hash.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"),
            "{hash}"
        );
        assert!(!hash.contains("correct horse"));
        // A restarted daemon needs no setup token once setup is done.
        assert!(
            Auth::new(store, None)
                .expect("auth")
                .setup_token()
                .is_none()
        );
    }

    #[test]
    fn login_checks_the_password_and_sessions_end() {
        let (_d, store, auth) = auth();
        let token = auth.setup_token().expect("token");
        auth.setup(&token, "correct horse battery", IP)
            .expect("setup");
        assert!(matches!(
            auth.login("wrong password!", IP),
            Err(AuthError::Denied)
        ));
        let s = auth.login("correct horse battery", IP).expect("login");
        auth.check(&s).expect("live");
        assert!(matches!(
            auth.check("0".repeat(64).as_str()),
            Err(AuthError::Denied)
        ));
        assert!(matches!(auth.check("short"), Err(AuthError::Denied)));
        auth.logout(&s).expect("logout");
        assert!(matches!(auth.check(&s), Err(AuthError::Denied)));

        let idle = auth.login("correct horse battery", IP).expect("login");
        store
            .touch_session(&sha256(&idle), store::now() - SESSION_IDLE_SECS - 1)
            .expect("age");
        assert!(matches!(auth.check(&idle), Err(AuthError::Denied)));
    }

    #[test]
    fn a_password_change_ends_every_other_session() {
        let (_d, _store, auth) = auth();
        let token = auth.setup_token().expect("token");
        let old = auth
            .setup(&token, "correct horse battery", IP)
            .expect("setup");
        assert!(matches!(
            auth.change_password("not it at all", "a brand new password", IP),
            Err(AuthError::Denied)
        ));
        let new = auth
            .change_password("correct horse battery", "a brand new password", IP)
            .expect("change");
        assert!(matches!(auth.check(&old), Err(AuthError::Denied)));
        auth.check(&new).expect("fresh session");
        auth.login("a brand new password", IP)
            .expect("new password works");
    }

    #[test]
    fn repeated_failures_are_throttled_per_address() {
        let (_d, _store, auth) = auth();
        let token = auth.setup_token().expect("token");
        auth.setup(&token, "correct horse battery", IP)
            .expect("setup");
        for _ in 0..FREE_FAILURES {
            assert!(matches!(
                auth.login("wrong password!", OTHER),
                Err(AuthError::Denied)
            ));
        }
        assert!(matches!(
            auth.login("correct horse battery", OTHER),
            Err(AuthError::Throttled(_))
        ));
        // Another address is not affected.
        auth.login("correct horse battery", IP)
            .expect("other address");
    }

    #[test]
    fn the_origin_rule() {
        assert!(same_origin(None, Some("same-origin"), None));
        assert!(!same_origin(
            Some("http://h:1"),
            Some("cross-site"),
            Some("h:1")
        ));
        assert!(same_origin(Some("http://h:1"), None, Some("h:1")));
        assert!(same_origin(Some("https://H:1"), None, Some("h:1")));
        assert!(!same_origin(Some("http://evil:1"), None, Some("h:1")));
        assert!(!same_origin(Some("null"), None, Some("h:1")));
        assert!(!same_origin(None, None, Some("h:1")));
        assert!(!same_origin(Some("http://h:1"), None, None));
    }
}
