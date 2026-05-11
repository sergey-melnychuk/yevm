use eyre::{eyre, Result};
use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
use rand::Rng;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::RwLock;
use yaevmi_base::Acc;
use yaevmi_misc::keccak256;

const NONCE_TTL: Duration = Duration::from_secs(60);
const SESSION_TTL: Duration = Duration::from_secs(8 * 3600);

pub struct AuthStore {
    challenges: RwLock<HashMap<String, Instant>>,
    sessions: RwLock<HashMap<String, (Acc, Instant)>>, // token -> (address, expiry)
}

impl AuthStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            challenges: RwLock::new(HashMap::new()),
            sessions: RwLock::new(HashMap::new()),
        })
    }

    pub async fn new_challenge(&self) -> String {
        let nonce = random_hex(32);
        self.challenges
            .write()
            .await
            .insert(nonce.clone(), Instant::now());
        nonce
    }

    pub async fn verify(&self, nonce: &str, signature: &str) -> Result<(Acc, String)> {
        // Check nonce exists and hasn't expired.
        {
            let mut challenges = self.challenges.write().await;
            let created = challenges
                .remove(nonce)
                .ok_or_else(|| eyre!("unknown or expired nonce"))?;
            if created.elapsed() > NONCE_TTL {
                return Err(eyre!("nonce expired"));
            }
        }

        let nonce_bytes = hex::decode(nonce).map_err(|e| eyre!("invalid nonce hex: {e}"))?;
        let address = recover_personal_sign(&nonce_bytes, signature)?;

        // Issue session token.
        let token = random_hex(32);
        self.sessions
            .write()
            .await
            .insert(token.clone(), (address, Instant::now()));
        Ok((address, token))
    }

    pub async fn authenticate(&self, token: &str) -> Option<Acc> {
        let sessions = self.sessions.read().await;
        sessions.get(token).and_then(|(addr, created)| {
            if created.elapsed() < SESSION_TTL {
                Some(*addr)
            } else {
                None
            }
        })
    }
}

// Recover address from a MetaMask personal_sign signature.
// personal_sign prepends "\x19Ethereum Signed Message:\n{len}" before hashing.
fn recover_personal_sign(message: &[u8], sig_hex: &str) -> Result<Acc> {
    let sig_bytes = hex_decode(sig_hex)?;
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
    let bytes: Vec<u8> = (0..bytes).map(|_| rng.gen()).collect();
    hex::encode(bytes)
}

fn hex_decode(s: &str) -> Result<Vec<u8>> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    hex::decode(s).map_err(|e| eyre!("hex: {e}"))
}
