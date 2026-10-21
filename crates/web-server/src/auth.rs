//! Web-only control schema, local password provider and opaque sessions.
//! No university identity/credential ports are used here.
use crate::{
    WebError,
    control::{ControlDb, SharedControl},
    poc::PocDatabase,
};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use rand_core::{OsRng, RngCore};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;
use tokio::sync::Semaphore;
use zeroize::Zeroizing;

pub const COOKIE_NAME: &str = "am_loopback_session";
pub const IDLE_SECONDS: i64 = 30 * 60;
pub const ABSOLUTE_SECONDS: i64 = 12 * 60 * 60;
const LOGIN_LIMIT: i64 = 5;
pub const REAUTH_SECONDS: i64 = 5 * 60;

#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct UserId(pub(crate) String);

pub trait AuthClock: Send + Sync + 'static {
    fn seconds(&self) -> i64;
}
pub struct SystemAuthClock;
impl AuthClock for SystemAuthClock {
    fn seconds(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64
    }
}

// Password authentication is deliberately separate from session issuance. Future
// providers must resolve the same internal UserId, never return a DB path.
struct LocalPasswordProvider {
    dummy: String,
}
impl LocalPasswordProvider {
    fn new() -> Result<Self, WebError> {
        Ok(Self {
            dummy: password_hash(&random_token()?)?,
        })
    }
    fn authenticate(
        &self,
        db: &Connection,
        login: &str,
        password: &str,
    ) -> Result<UserId, WebError> {
        let account = db
            .query_row(
                "SELECT user_id, verifier, status FROM accounts WHERE login = ?1",
                [login],
                |r| {
                    Ok((
                        UserId(r.get(0)?),
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| WebError::Internal)?;
        let verifier = account
            .as_ref()
            .map_or(self.dummy.as_str(), |a| a.1.as_str());
        let parsed = PasswordHash::new(verifier).map_err(|_| WebError::Internal)?;
        // Only our versioned local policy is accepted. A corrupted PHC string
        // must not select unbounded memory/CPU parameters.
        if parsed.algorithm.as_str() != "argon2id"
            || parsed.version != Some(19)
            || parsed.params.get_decimal("m") != Some(19456)
            || parsed.params.get_decimal("t") != Some(2)
            || parsed.params.get_decimal("p") != Some(1)
        {
            return Err(WebError::Internal);
        }
        let valid = Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok();
        match account {
            Some((user, _, status)) if valid && status == "enabled" => Ok(user),
            _ => Err(WebError::Unauthorized), // Same response/work for unknown, disabled and wrong password.
        }
    }
}
fn password_hash(password: &str) -> Result<String, WebError> {
    let mut salt = [0u8; 16];
    OsRng
        .try_fill_bytes(&mut salt)
        .map_err(|_| WebError::Unavailable)?;
    let salt = SaltString::encode_b64(&salt).map_err(|_| WebError::Internal)?;
    let params = argon2::Params::new(19456, 2, 1, Some(32)).map_err(|_| WebError::Internal)?;
    Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|_| WebError::Internal)
}
fn random_token() -> Result<String, WebError> {
    let mut bytes = [0u8; 32];
    OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|_| WebError::Unavailable)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
fn token_hash(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}
fn valid_token(token: &str) -> bool {
    token.len() == 64
        && token
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
fn valid_login(login: &str) -> bool {
    !login.is_empty()
        && login.len() <= 64
        && login
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// Explicit local administration only. The binary prompts on the terminal; no
/// HTTP signup, environment password or password CLI argument is supported.
pub fn bootstrap_admin(
    database: &PocDatabase,
    login: &str,
    password: Zeroizing<String>,
) -> Result<UserId, WebError> {
    if !valid_login(login)
        || password.len() < 15
        || password.len() > 256
        || password.chars().any(char::is_control)
    {
        return Err(WebError::InvalidRequest);
    }
    let mut db = ControlDb::open(database)?;
    let tx = db
        .connection
        .transaction()
        .map_err(|_| WebError::Internal)?;
    let exists: bool = tx
        .query_row("SELECT EXISTS(SELECT 1 FROM accounts)", [], |r| r.get(0))
        .map_err(|_| WebError::Internal)?;
    if exists {
        return Err(WebError::AccountExists);
    }
    let verifier = password_hash(&password)?;
    let user = UserId(random_token()?);
    tx.execute(
        "INSERT INTO accounts VALUES (1, ?1, ?2, ?3, 'enabled')",
        params![user.0, login, verifier],
    )
    .map_err(|_| WebError::Internal)?;
    tx.commit().map_err(|_| WebError::Internal)?;
    Ok(user)
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AuthenticatedSession {
    pub authenticated: bool,
    pub user_id: UserId,
    pub csrf_token: String,
    pub expires_at: i64,
    pub absolute_expires_at: i64,
    #[serde(skip)]
    hash: Vec<u8>,
}
impl AuthenticatedSession {
    pub fn check_csrf(&self, supplied: &str) -> Result<(), WebError> {
        if bool::from(self.csrf_token.as_bytes().ct_eq(supplied.as_bytes())) {
            Ok(())
        } else {
            Err(WebError::InvalidCsrf)
        }
    }
}

struct Inner {
    control: SharedControl,
    provider: LocalPasswordProvider,
    clock: Arc<dyn AuthClock>,
    login_gate: Arc<Semaphore>,
}

// Accept only a provider-verified internal identity. This operation knows
// nothing about passwords or a future OIDC provider's credential format.
fn issue_session(
    db: &mut Connection,
    user: UserId,
    now: i64,
    old_token: Option<String>,
) -> Result<(String, AuthenticatedSession), WebError> {
    let token = random_token()?;
    let session = AuthenticatedSession {
        authenticated: true,
        user_id: user,
        csrf_token: random_token()?,
        expires_at: now + IDLE_SECONDS,
        absolute_expires_at: now + ABSOLUTE_SECONDS,
        hash: token_hash(&token),
    };
    let tx = db.transaction().map_err(|_| WebError::Internal)?;
    tx.execute(
        "DELETE FROM sessions WHERE absolute_expiry <= ?1 OR last_seen <= ?2",
        params![now, now - IDLE_SECONDS],
    )
    .map_err(|_| WebError::Internal)?;
    if let Some(old) = old_token {
        tx.execute(
            "DELETE FROM sessions WHERE token_hash=?1",
            [token_hash(&old)],
        )
        .map_err(|_| WebError::Internal)?;
    }
    // Bound records for this single-account PoC, including abandoned logins.
    tx.execute(
        "DELETE FROM sessions WHERE token_hash IN
         (SELECT token_hash FROM sessions ORDER BY created DESC LIMIT -1 OFFSET 15)",
        [],
    )
    .map_err(|_| WebError::Internal)?;
    tx.execute(
        "INSERT INTO sessions (token_hash,user_id,csrf,created,last_seen,absolute_expiry) VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
        params![
            session.hash,
            session.user_id.0,
            session.csrf_token,
            now,
            session.absolute_expires_at
        ],
    )
    .map_err(|_| WebError::Internal)?;
    tx.commit().map_err(|_| WebError::Internal)?;
    Ok((token, session))
}
#[derive(Clone)]
pub(crate) struct AuthService(Arc<Inner>);
impl AuthService {
    pub async fn open(
        database: &PocDatabase,
        clock: impl AuthClock,
    ) -> Result<(Self, UserId), WebError> {
        let path = database
            .path()
            .parent()
            .ok_or(WebError::Internal)?
            .to_path_buf();
        tokio::task::spawn_blocking(move || {
            let database = PocDatabase::prepare(&path)?;
            let db = ControlDb::open(&database)?;
            let owner = db
                .connection
                .query_row("SELECT user_id FROM accounts WHERE singleton=1", [], |r| {
                    Ok(UserId(r.get(0)?))
                })
                .optional()
                .map_err(|_| WebError::Internal)?
                .ok_or(WebError::SetupRequired)?;
            // Deliberately non-resumable across process restart (including crash).
            db.connection
                .execute("DELETE FROM sessions", [])
                .map_err(|_| WebError::Internal)?;
            Ok((
                Self(Arc::new(Inner {
                    control: SharedControl::new(db),
                    provider: LocalPasswordProvider::new()?,
                    clock: Arc::new(clock),
                    login_gate: Arc::new(Semaphore::new(1)),
                })),
                owner,
            ))
        })
        .await
        .map_err(|_| WebError::Unavailable)?
    }
    async fn run<T: Send + 'static>(
        &self,
        action: impl FnOnce(&mut Connection, &Inner) -> Result<T, WebError> + Send + 'static,
    ) -> Result<T, WebError> {
        let inner = self.0.clone();
        tokio::task::spawn_blocking(move || inner.control.with(|db| action(db, &inner)))
            .await
            .map_err(|_| WebError::Unavailable)?
    }
    pub async fn login(
        &self,
        login: String,
        password: Zeroizing<String>,
        old_token: Option<String>,
    ) -> Result<(String, AuthenticatedSession), WebError> {
        let permit = self
            .0
            .login_gate
            .clone()
            .try_acquire_owned()
            .map_err(|_| WebError::RateLimited)?;
        self.run(move |db, inner| {
            let _permit = permit; // Cancellation cannot release the hash-work limit early.
            let now = inner.clock.seconds();
            charge_attempt(db, now)?;
            if !valid_login(&login)
                || password.is_empty()
                || password.len() > 256
                || password.chars().any(char::is_control)
            {
                return Err(WebError::InvalidRequest);
            }
            let user = inner.provider.authenticate(db, &login, &password)?;
            issue_session(db, user, now, old_token)
        })
        .await
    }
    pub async fn resolve(&self, token: Option<String>) -> Result<AuthenticatedSession, WebError> {
        let token = token
            .filter(|t| valid_token(t))
            .ok_or(WebError::Unauthorized)?;
        self.run(move |db, inner| {
            let hash = token_hash(&token);
            let (user, csrf, last_seen, absolute, status) = db
                .query_row(
                    "SELECT s.user_id,s.csrf,s.last_seen,s.absolute_expiry,a.status
                 FROM sessions s JOIN accounts a ON a.user_id=s.user_id WHERE token_hash=?1",
                    [&hash],
                    |r| {
                        Ok((
                            UserId(r.get(0)?),
                            r.get::<_, String>(1)?,
                            r.get::<_, i64>(2)?,
                            r.get::<_, i64>(3)?,
                            r.get::<_, String>(4)?,
                        ))
                    },
                )
                .optional()
                .map_err(|_| WebError::Internal)?
                .ok_or(WebError::Unauthorized)?;
            let now = inner.clock.seconds();
            if now >= absolute || now >= last_seen + IDLE_SECONDS || now < last_seen {
                db.execute("DELETE FROM sessions WHERE token_hash=?1", [&hash])
                    .map_err(|_| WebError::Internal)?;
                return Err(WebError::Unauthorized);
            }
            if status != "enabled" {
                return Err(WebError::Forbidden);
            }
            db.execute(
                "UPDATE sessions SET last_seen=?1 WHERE token_hash=?2",
                params![now, hash],
            )
            .map_err(|_| WebError::Internal)?;
            Ok(AuthenticatedSession {
                authenticated: true,
                user_id: user,
                csrf_token: csrf,
                expires_at: (now + IDLE_SECONDS).min(absolute),
                absolute_expires_at: absolute,
                hash,
            })
        })
        .await
    }
    pub(crate) fn control(&self) -> SharedControl {
        self.0.control.clone()
    }
    pub async fn require_recent(&self, session: AuthenticatedSession) -> Result<(), WebError> {
        self.run(move |db, inner| {
            let now = inner.clock.seconds();
            let (_, recent) = live_session(db, &session, now)?;
            if recent.is_none_or(|at| at > now || now - at >= REAUTH_SECONDS) {
                return Err(WebError::ReauthRequired);
            }
            Ok(())
        })
        .await
    }
    pub async fn reauthenticate(
        &self,
        session: AuthenticatedSession,
        password: Zeroizing<String>,
    ) -> Result<(), WebError> {
        let permit = self
            .0
            .login_gate
            .clone()
            .try_acquire_owned()
            .map_err(|_| WebError::RateLimited)?;
        self.run(move |db, inner| {
            let _permit = permit;
            let now = inner.clock.seconds();
            let (login, _) = live_session(db, &session, now)?;
            charge_attempt(db, now)?;
            if password.is_empty() || password.len() > 256 || password.chars().any(char::is_control)
            {
                return Err(WebError::InvalidRequest);
            }
            let user = inner
                .provider
                .authenticate(db, &login, &password)
                .map_err(|error| {
                    if error == WebError::Unauthorized {
                        WebError::ReauthFailed
                    } else {
                        error
                    }
                })?;
            if user != session.user_id {
                return Err(WebError::Forbidden);
            }
            db.execute(
                "UPDATE sessions SET reauthenticated_at=?1 WHERE token_hash=?2 AND user_id=?3",
                params![now, session.hash, session.user_id.0],
            )
            .map_err(|_| WebError::Internal)?;
            Ok(())
        })
        .await
    }
    pub async fn logout(&self, session: AuthenticatedSession) -> Result<(), WebError> {
        self.run(move |db, _| {
            db.execute("DELETE FROM sessions WHERE token_hash=?1", [session.hash])
                .map_err(|_| WebError::Internal)?;
            Ok(())
        })
        .await
    }
    pub async fn close(&self) {
        let inner = self.0.clone();
        let _ = tokio::task::spawn_blocking(move || {
            inner.control.close();
        })
        .await;
    }
}

fn charge_attempt(db: &Connection, now: i64) -> Result<(), WebError> {
    db.execute("DELETE FROM login_attempts WHERE at <= ?1", [now - 60])
        .map_err(|_| WebError::Internal)?;
    let count: i64 = db
        .query_row("SELECT count(*) FROM login_attempts", [], |r| r.get(0))
        .map_err(|_| WebError::Internal)?;
    if count >= LOGIN_LIMIT {
        return Err(WebError::RateLimited);
    }
    db.execute("INSERT INTO login_attempts VALUES (?1)", [now])
        .map_err(|_| WebError::Internal)?;
    Ok(())
}
// Re-check the row at admission: logout, disable, rotation and expiry cannot
// turn an earlier session snapshot into a new reauthentication grant.
fn live_session(
    db: &Connection,
    session: &AuthenticatedSession,
    now: i64,
) -> Result<(String, Option<i64>), WebError> {
    let (login,status,last,absolute,recent): (String,String,i64,i64,Option<i64>) = db.query_row(
        "SELECT a.login,a.status,s.last_seen,s.absolute_expiry,s.reauthenticated_at FROM sessions s JOIN accounts a ON a.user_id=s.user_id WHERE s.token_hash=?1 AND s.user_id=?2",
        params![session.hash,session.user_id.0], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional().map_err(|_| WebError::Internal)?.ok_or(WebError::Unauthorized)?;
    if status != "enabled" {
        return Err(WebError::Forbidden);
    }
    if now < last || now >= absolute || now - last >= IDLE_SECONDS {
        return Err(WebError::Unauthorized);
    }
    Ok((login, recent))
}

/// HTTP loopback policy only. There is no runtime production/non-Secure switch.
/// Future HTTPS serving must use a distinct __Host- cookie + Secure, Path=/,
/// no Domain and trusted deployment configuration before any public bind exists.
pub(crate) fn cookie(token: &str) -> String {
    format!("{COOKIE_NAME}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={ABSOLUTE_SECONDS}")
}
pub(crate) fn clear_cookie() -> String {
    format!("{COOKIE_NAME}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0")
}
pub(crate) fn read_cookie(headers: &axum::http::HeaderMap) -> Result<Option<String>, WebError> {
    let mut found = None;
    for header in headers.get_all(axum::http::header::COOKIE) {
        let header = header.to_str().map_err(|_| WebError::Unauthorized)?;
        if header.len() > 4096 {
            return Err(WebError::Unauthorized);
        }
        for pair in header.split(';') {
            if let Some((name, value)) = pair.trim().split_once('=')
                && name == COOKIE_NAME
            {
                if found.is_some() || !valid_token(value) {
                    return Err(WebError::Unauthorized);
                }
                found = Some(value.to_owned());
            }
        }
    }
    Ok(found)
}

pub fn control_path(database: &PocDatabase) -> PathBuf {
    database
        .path()
        .parent()
        .expect("PoC directory")
        .join("web-control.db")
}
