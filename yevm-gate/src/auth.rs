use chrono::{DateTime, Utc};
use eyre::{Result, eyre};
use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
use rand::Rng;
use sqlx::SqlitePool;
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use yevm_base::Acc;
use yevm_misc::{hex::parse_vec, keccak256};

const NONCE_TTL_SECS: i64 = 60;
const SESSION_TTL_SECS: i64 = 8 * 3600;
const CLOCK_DRIFT_SECS: i64 = 60;

pub struct AuthStore {
    pool: SqlitePool,
}

impl AuthStore {
    pub fn new(pool: SqlitePool) -> Arc<Self> {
        Arc::new(Self { pool })
    }

    pub async fn new_challenge(&self) -> Result<String> {
        let nonce = random_hex(32);
        sqlx::query("INSERT INTO auth_challenges (nonce, created_at) VALUES (?, ?)")
            .bind(&nonce)
            .bind(unix_now())
            .execute(&self.pool)
            .await?;
        Ok(nonce)
    }

    pub async fn verify(
        &self,
        message: &str,
        signature: &str,
        host: &str,
    ) -> Result<(Acc, String)> {
        let domain = siwe_domain(message)?;
        if !domain.eq_ignore_ascii_case(host) {
            return Err(eyre!("SIWE domain {domain:?} does not match host {host:?}"));
        }
        let uri = find_and_strip_prefix(message, "URI: ")?;
        if !uri_host(&uri).eq_ignore_ascii_case(host) {
            return Err(eyre!("SIWE URI {uri:?} does not match host {host:?}"));
        }

        // Reject messages outside their own validity window (EIP-4361).
        let now = unix_now();
        let issued = parse_iso8601_utc(&find_and_strip_prefix(message, "Issued At: ")?)?;
        if issued > now + CLOCK_DRIFT_SECS {
            return Err(eyre!("SIWE issued in the future"));
        }
        let expires = parse_iso8601_utc(&find_and_strip_prefix(message, "Expiration Time: ")?)?;
        if now > expires {
            return Err(eyre!("SIWE message expired"));
        }

        let nonce = find_and_strip_prefix(message, "Nonce: ")?;

        sqlx::query("DELETE FROM auth_challenges WHERE created_at < ?")
            .bind(now - NONCE_TTL_SECS)
            .execute(&self.pool)
            .await?;
        let consumed = sqlx::query("DELETE FROM auth_challenges WHERE nonce = ?")
            .bind(&nonce)
            .execute(&self.pool)
            .await?;
        if consumed.rows_affected() == 0 {
            return Err(eyre!("unknown or expired nonce"));
        }

        let address = recover_signer(message.as_bytes(), signature)?;

        let msg_addr = siwe_address(message)?;
        if format!("{address}").to_lowercase() != msg_addr.to_lowercase() {
            return Err(eyre!("address mismatch"));
        }

        let token = random_hex(32);
        sqlx::query("INSERT INTO auth_sessions (token, signer, created_at) VALUES (?, ?, ?)")
            .bind(&token)
            .bind(format!("{address}"))
            .bind(now)
            .execute(&self.pool)
            .await?;
        Ok((address, token))
    }

    pub async fn authenticate(&self, token: &str) -> Option<Acc> {
        let now = unix_now();
        sqlx::query("DELETE FROM auth_sessions WHERE created_at < ?")
            .bind(now - SESSION_TTL_SECS)
            .execute(&self.pool)
            .await
            .ok()?;
        let (signer,): (String,) =
            sqlx::query_as("SELECT signer FROM auth_sessions WHERE token = ?")
                .bind(token)
                .fetch_optional(&self.pool)
                .await
                .ok()??;
        signer.as_str().try_into().ok()
    }
}

// Extract a named field value from a SIWE message, e.g. "Nonce: abc123" -> "abc123".
fn find_and_strip_prefix(message: &str, prefix: &str) -> Result<String> {
    message
        .lines()
        .filter_map(|line| line.strip_prefix(prefix))
        .next()
        .map(|line| line.to_string())
        .ok_or_else(|| eyre!("SIWE message missing field: {prefix}"))
}

fn siwe_domain(message: &str) -> Result<String> {
    const SUFFIX: &str = " wants you to sign in with your Ethereum account:";
    message
        .lines()
        .next()
        .and_then(|l| l.strip_suffix(SUFFIX))
        .map(|d| d.trim().to_string())
        .ok_or_else(|| eyre!("malformed SIWE first line"))
}

// Reduce "https://host:8000/path" to "host[:port]" 
// to be later comparable to the Host header value.
fn uri_host(uri: &str) -> String {
    uri.split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(uri)
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .trim_end_matches('/')
        .to_string()
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn parse_iso8601_utc(s: &str) -> Result<i64> {
    s.trim()
        .parse::<DateTime<Utc>>()
        .map(|dt| dt.timestamp())
        .map_err(|e| eyre!("bad timestamp: {s}: {e}"))
}

fn siwe_address(message: &str) -> Result<String> {
    message
        .lines()
        .nth(1)
        .map(|l| l.trim().to_string())
        .ok_or_else(|| eyre!("SIWE message too short"))
}

fn recover_signer(message: &[u8], sig_hex: &str) -> Result<Acc> {
    let sig_bytes = parse_vec(sig_hex).map_err(|e| eyre!("{e}"))?;
    if sig_bytes.len() != 65 {
        return Err(eyre!("signature must be 65 bytes, got {}", sig_bytes.len()));
    }

    let prefix = format!("\x19Ethereum Signed Message:\n{}", message.len());
    let mut payload = prefix.into_bytes();
    payload.extend_from_slice(message);
    let hash = keccak256(&payload);

    let v = sig_bytes[64];
    let recovery_id = if v >= 27 { v - 27 } else { v };
    let rid = RecoveryId::try_from(recovery_id).map_err(|e| eyre!("invalid v: {e}"))?;

    let r: [u8; 32] = sig_bytes[0..32].try_into().unwrap();
    let s: [u8; 32] = sig_bytes[32..64].try_into().unwrap();
    let sig = Signature::from_scalars(r, s).map_err(|e| eyre!("invalid sig: {e}"))?;
    let (sig, rid) = if let Some(norm) = sig.normalize_s() {
        (norm, RecoveryId::new(!rid.is_y_odd(), rid.is_x_reduced()))
    } else {
        (sig, rid)
    };

    let key = VerifyingKey::recover_from_prehash(hash.as_ref(), &sig, rid)
        .map_err(|e| eyre!("ecrecover: {e}"))?;
    let point = key.to_encoded_point(false);
    let h = keccak256(&point.as_bytes()[1..]);
    Ok(Acc::from(&h.as_ref()[12..]))
}

fn random_hex(bytes: usize) -> String {
    let mut rng = rand::thread_rng();
    let bytes: Vec<u8> = (0..bytes).map(|_| rng.r#gen()).collect();
    hex::encode(bytes)
}
