//! Java-compatible admin authentication.
//!
//! The upstream service signs an HS256 JWT with a random per-process secret,
//! stores `id`/`username`/`password`/`role`/`valid_status` claims, and keeps a
//! token blacklist for logout. Passwords are compared after one MD5 pass
//! because the compiled admin UI sends the MD5 of the typed password.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use md5::{Digest, Md5};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Sha256;
use tokio::sync::RwLock;

type HmacSha256 = Hmac<Sha256>;

/// Java blocks once `loginWrongCount > 10`, so ten failures are allowed.
pub const MAX_WRONG_LOGINS: u32 = 10;

/// The upstream default account, created by the first-run seed.
pub const DEFAULT_USERNAME: &str = "jproxy";
pub const DEFAULT_PASSWORD: &str = "jproxy@2023";
pub const DEFAULT_PASSWORD_HASH: &str = "765EB35667C4323E7CCB88C94C223202";

/// Resolve the first-run admin credentials without ever logging them.
///
/// `ADMIN_USERNAME`/`ADMIN_PASSWORD` override the Java defaults only when the
/// `system_user` table is empty. Blank values fall back to the defaults.
pub fn first_run_admin_credentials(
    username: Option<String>,
    password: Option<String>,
) -> (String, String) {
    let username = username
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_USERNAME.to_owned());
    let password = password
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_PASSWORD.to_owned());
    (username, password)
}

/// Store the same value Java persists for a typed password: the MD5 of the
/// MD5 the compiled UI submits, uppercase.
pub fn stored_password_hash(password: &str) -> String {
    md5_upper(&md5_upper(password))
}

/// MD5 of `value`, uppercase. This is the single hash the Java service applies
/// to the password submitted by the UI (which is already an MD5 digest).
pub fn md5_upper(value: &str) -> String {
    let digest = Md5::digest(value.as_bytes());
    digest.iter().map(|byte| format!("{byte:02X}")).collect()
}

fn random_bytes() -> Result<[u8; 16]> {
    let mut bytes = [0_u8; 16];
    getrandom::getrandom(&mut bytes)
        .map_err(|error| anyhow::anyhow!("read random bytes for admin authentication: {error}"))?;
    Ok(bytes)
}

/// A UUID-shaped identifier without external dependencies, matching Java's
/// `Generator.generateUUID()` (dashes removed, uppercase).
pub fn random_uuid() -> Result<String> {
    let mut bytes = random_bytes()?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(bytes.iter().map(|byte| format!("{byte:02X}")).collect())
}

/// Claims extracted from a verified token.
#[derive(Debug, Clone, Default)]
pub struct AuthUser {
    pub id: Option<i64>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub role: Option<String>,
    pub valid_status: Option<i64>,
}

impl AuthUser {
    pub fn anonymous() -> Self {
        Self {
            username: Some("Anonymous".to_owned()),
            password: Some("Anonymous".to_owned()),
            ..Self::default()
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
}

pub struct AuthState {
    secret: String,
    token_expires: Duration,
    login_enabled: bool,
    wrong_logins: AtomicU32,
    blacklist: RwLock<HashSet<String>>,
}

impl AuthState {
    pub fn new(login_enabled: bool, token_expires: Duration) -> Result<Self> {
        // Java uses md5(random UUID) as the HMAC key. Sixteen random bytes in
        // uppercase hex are equivalent and equally process-scoped.
        let secret = random_bytes()?
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect();
        Ok(Self {
            secret,
            token_expires,
            login_enabled,
            wrong_logins: AtomicU32::new(0),
            blacklist: RwLock::new(HashSet::new()),
        })
    }

    pub fn login_enabled(&self) -> bool {
        self.login_enabled
    }

    pub fn wrong_login_count(&self) -> u32 {
        self.wrong_logins.load(Ordering::SeqCst)
    }

    pub fn record_wrong_login(&self) -> u32 {
        self.wrong_logins.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn login_blocked(&self) -> bool {
        self.wrong_login_count() > MAX_WRONG_LOGINS
    }

    pub fn sign(&self, user: &AuthUser) -> Result<String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before the Unix epoch")?
            .as_secs();
        let expires = now
            .saturating_add(self.token_expires.as_secs())
            .saturating_add(self.token_expires.subsec_nanos() as u64 / 1_000_000_000);
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
        let claims = json!({
            "id": user.id,
            "username": user.username,
            "password": user.password,
            "role": user.role,
            "valid_status": user.valid_status,
            "exp": expires,
            "iat": now,
        });
        let payload =
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).context("serialize JWT claims")?);
        let signing_input = format!("{header}.{payload}");
        let mut mac =
            HmacSha256::new_from_slice(self.secret.as_bytes()).context("initialize JWT HMAC")?;
        mac.update(signing_input.as_bytes());
        let signature = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
        Ok(format!("{signing_input}.{signature}"))
    }

    /// Verify an `Authorization` header. The raw header is checked against the
    /// blacklist first, matching Java's `cache.get(token)` before stripping
    /// the `Bearer ` prefix.
    pub async fn verify(&self, authorization: Option<&str>) -> bool {
        self.user_from_authorization(authorization).await.is_some()
    }

    pub async fn user_from_authorization(&self, authorization: Option<&str>) -> Option<AuthUser> {
        let raw = authorization?;
        if self.blacklist.read().await.contains(raw) {
            return None;
        }
        let token = raw.replace("Bearer ", "");
        self.verify_token(&token)
    }

    fn verify_token(&self, token: &str) -> Option<AuthUser> {
        let mut parts = token.split('.');
        let header = parts.next()?;
        let payload = parts.next()?;
        let signature = parts.next()?;
        if parts.next().is_some() {
            return None;
        }

        let decoded_header = URL_SAFE_NO_PAD.decode(header).ok()?;
        let header_json: Value = serde_json::from_slice(&decoded_header).ok()?;
        if header_json.get("alg").and_then(Value::as_str) != Some("HS256") {
            return None;
        }

        let mut mac = HmacSha256::new_from_slice(self.secret.as_bytes()).ok()?;
        mac.update(format!("{header}.{payload}").as_bytes());
        let signature = URL_SAFE_NO_PAD.decode(signature).ok()?;
        mac.verify_slice(&signature).ok()?;

        let decoded_payload = URL_SAFE_NO_PAD.decode(payload).ok()?;
        let claims: Value = serde_json::from_slice(&decoded_payload).ok()?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
        let expires = claims.get("exp").and_then(Value::as_u64)?;
        if expires <= now {
            return None;
        }

        Some(AuthUser {
            id: claims.get("id").and_then(Value::as_i64),
            username: claims
                .get("username")
                .and_then(Value::as_str)
                .map(str::to_owned),
            password: claims
                .get("password")
                .and_then(Value::as_str)
                .map(str::to_owned),
            role: claims
                .get("role")
                .and_then(Value::as_str)
                .map(str::to_owned),
            valid_status: claims.get("valid_status").and_then(Value::as_i64),
        })
    }

    /// Java stores the raw `Authorization` header in `token_black_list`.
    pub async fn logout(&self, authorization: Option<&str>) {
        if let Some(raw) = authorization {
            self.blacklist.write().await.insert(raw.to_owned());
        }
    }

    pub async fn clear_blacklist(&self) {
        self.blacklist.write().await.clear();
    }
}

/// Marker inserted by the UDS control listener after the peer UID and control
/// token have been verified. TCP requests never carry it, so the control token
/// cannot be used as an authentication bypass on the Web UI listener.
#[derive(Debug, Clone, Copy, Default)]
pub struct ControlPrincipal {
    pub uid: Option<u32>,
}

/// Reject unprotected admin API access with `403`, as Java's
/// `LoginInterceptor` does.
pub async fn require_admin(
    State(auth): State<Arc<AuthState>>,
    mut request: Request,
    next: Next,
) -> Response {
    if request.extensions().get::<ControlPrincipal>().is_some() {
        request.extensions_mut().insert(AuthUser {
            username: Some("control".to_owned()),
            role: Some("admin".to_owned()),
            valid_status: Some(1),
            ..AuthUser::default()
        });
        return next.run(request).await;
    }
    let authorization = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    let Some(user) = auth.user_from_authorization(authorization).await else {
        return StatusCode::FORBIDDEN.into_response();
    };
    request.extensions_mut().insert(user);
    next.run(request).await
}

pub fn authorization_header(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_credentials_match_the_upstream_seed() {
        assert_eq!(
            first_run_admin_credentials(None, None),
            (DEFAULT_USERNAME.to_owned(), DEFAULT_PASSWORD.to_owned())
        );
        assert_eq!(
            stored_password_hash(DEFAULT_PASSWORD),
            DEFAULT_PASSWORD_HASH
        );
        assert_eq!(
            md5_upper(DEFAULT_PASSWORD),
            "D7FD102352432BC8D92B6023EDF4B28A"
        );
    }

    #[test]
    fn environment_credentials_override_defaults_and_blank_values_do_not() {
        assert_eq!(
            first_run_admin_credentials(
                Some("operator".to_owned()),
                Some("synthetic-password".to_owned())
            ),
            ("operator".to_owned(), "synthetic-password".to_owned())
        );
        assert_eq!(
            first_run_admin_credentials(Some("   ".to_owned()), Some(String::new())),
            (DEFAULT_USERNAME.to_owned(), DEFAULT_PASSWORD.to_owned())
        );
    }

    #[test]
    fn ui_password_hash_matches_the_upstream_seed() {
        assert_eq!(
            md5_upper("D7FD102352432BC8D92B6023EDF4B28A"),
            DEFAULT_PASSWORD_HASH
        );
        assert_eq!(md5_upper("jproxy@2023"), "D7FD102352432BC8D92B6023EDF4B28A");
    }

    #[tokio::test]
    async fn tokens_round_trip_and_expire() {
        let auth = AuthState::new(true, Duration::from_secs(60)).unwrap();
        let user = AuthUser {
            id: Some(1),
            username: Some("synthetic".to_owned()),
            password: Some("hash".to_owned()),
            role: Some("admin".to_owned()),
            valid_status: Some(1),
        };
        let token = auth.sign(&user).unwrap();
        let decoded = auth
            .user_from_authorization(Some(&format!("Bearer {token}")))
            .await
            .unwrap();
        assert_eq!(decoded.id, Some(1));
        assert_eq!(decoded.username.as_deref(), Some("synthetic"));
        assert_eq!(decoded.valid_status, Some(1));

        let expired = AuthState::new(true, Duration::ZERO).unwrap();
        let token = expired.sign(&user).unwrap();
        assert!(
            expired
                .user_from_authorization(Some(&token))
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn logout_blacklists_the_raw_header() {
        let auth = AuthState::new(true, Duration::from_secs(60)).unwrap();
        let token = auth.sign(&AuthUser::anonymous()).unwrap();
        let header = format!("Bearer {token}");
        assert!(auth.verify(Some(&header)).await);
        auth.logout(Some(&header)).await;
        assert!(!auth.verify(Some(&header)).await);
        auth.clear_blacklist().await;
        assert!(auth.verify(Some(&header)).await);
    }

    #[tokio::test]
    async fn tampered_tokens_are_rejected() {
        let auth = AuthState::new(true, Duration::from_secs(60)).unwrap();
        let token = auth.sign(&AuthUser::anonymous()).unwrap();
        let mut parts: Vec<&str> = token.split('.').collect();
        parts[1] = "ZXZpbA";
        assert!(!auth.verify(Some(&parts.join("."))).await);
        assert!(!auth.verify(Some("not-a-token")).await);
        assert!(!auth.verify(None).await);
    }

    #[test]
    fn wrong_login_counter_blocks_after_ten() {
        let auth = AuthState::new(true, Duration::from_secs(60)).unwrap();
        for _ in 0..MAX_WRONG_LOGINS {
            auth.record_wrong_login();
        }
        assert!(!auth.login_blocked());
        auth.record_wrong_login();
        assert!(auth.login_blocked());
    }
}
