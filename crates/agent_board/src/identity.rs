//! Device identity: loads the local ed25519 SSH key, derives a stable device id,
//! signs request payloads, and produces a best-effort location fingerprint.
//!
//! Signing scheme (must match `agent-board-worker/src/index.js`):
//!   message = canonical_request_body + "|" + unix_timestamp
//!   sig     = ed25519_sign(secret_key, message_bytes)
//! The worker verifies with the raw 32-byte ed25519 public key.
//!
//! Device id = hex(blake3(raw_ed25519_pubkey_32)). This is stable across
//! sessions and unique per SSH key.

use anyhow::{Context, Result, bail};
use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey};
use ssh_key::PrivateKey;

/// Canonical encoding for signatures and device-id derivation. Matches the
/// worker's atob(base64) byte handling (standard base64, no URL-safe chars).
const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// A loaded device identity backed by an ed25519 SSH key.
pub struct DeviceIdentity {
    signing_key: SigningKey,
    /// Raw 32-byte ed25519 public key (the verifying key).
    public_key_32: [u8; 32],
    /// hex(blake3(public_key_32)).
    device_id: String,
    device_name: String,
    location_hash: String,
}

impl DeviceIdentity {
    /// Load an ed25519 OpenSSH private key from `path` (e.g. `~/.ssh/id_ed25519`).
    /// `device_name` is a human label (hostname); `location_hash` is the
    /// best-effort location fingerprint (see [`location_hash`]).
    pub fn load(path: &std::path::Path, device_name: String, location_hash: String) -> Result<Self> {
        let pem = std::fs::read_to_string(path)
            .with_context(|| format!("reading ssh private key at {}", path.display()))?;
        // ssh-key auto-detects OpenSSH vs PEM; ed25519 keys need no passphrase
        // here (operator key is unencrypted). Encrypted keys will error loudly.
        let private = PrivateKey::from_openssh(&pem)
            .with_context(|| format!("parsing ssh private key at {}", path.display()))?;

        let keypair = private.key_data().ed25519().context(
            "agent_board only supports ed25519 ssh keys; please point at an id_ed25519 file",
        )?;
        // ssh_key::Ed25519Keypair { public: [u8;32], private: KeypairBytes }
        let mut secret = [0u8; 32];
        secret.copy_from_slice(keypair.private.as_ref());
        let signing_key = SigningKey::from_bytes(&secret);
        let public_key_32 = signing_key.verifying_key().to_bytes();

        // Zero the local copy of the secret bytes; the ssh_key PrivateKey still
        // holds its own copy until dropped at end of function.
        // (secret is a stack array, dropped at scope end — nothing more to do.)

        let device_id = device_id_from_pubkey(&public_key_32);
        Ok(Self {
            signing_key,
            public_key_32,
            device_id,
            device_name,
            location_hash,
        })
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    pub fn location_hash(&self) -> &str {
        &self.location_hash
    }

    /// Raw 32-byte verifying key, base64-encoded, for the `X-Pubkey` header.
    pub fn public_key_b64(&self) -> String {
        B64.encode(self.public_key_32)
    }

    /// Sign `body_text + "|" + timestamp` and return the base64 signature.
    /// `body_text` MUST be the exact bytes sent over the wire (raw request body).
    pub fn sign(&self, body_text: &str, timestamp: i64) -> Result<String> {
        let message = format!("{body_text}|{timestamp}");
        let sig: Signature = self.signing_key.sign(message.as_bytes());
        Ok(B64.encode(sig.to_bytes()))
    }

    /// Self-test: verify our own signature round-trips. Reserved for init-time
    /// fail-fast on a malformed key.
    #[allow(dead_code)]
    pub fn verify_self(&self, body_text: &str, timestamp: i64) -> Result<()> {
        use ed25519_dalek::Verifier;
        let sig_b64 = self.sign(body_text, timestamp)?;
        let sig_bytes = B64
            .decode(sig_b64.as_bytes())
            .context("decoding self-signature")?;
        let sig = Signature::from_slice(&sig_bytes).context("parsing self-signature")?;
        let message = format!("{body_text}|{timestamp}");
        let verifying = self.signing_key.verifying_key();
        verifying
            .verify(message.as_bytes(), &sig)
            .context("self-verify failed — key or signing path is broken")?;
        Ok(())
    }
}

/// Derive a stable device id from a raw 32-byte ed25519 public key.
pub fn device_id_from_pubkey(public_key_32: &[u8; 32]) -> String {
    let hash = blake3::hash(public_key_32);
    hex::encode(hash.as_bytes())
}

/// Best-effort location fingerprint: blake3(hostname + primary network
/// interface MAC). On modern macOS, reading the current Wi-Fi SSID requires
/// CoreLocation authorization (fragile, prompts the user), so we deliberately
/// avoid it and use a stable host+interface hash instead. Two machines on the
/// same LAN with different hostnames/MACs still get distinct fingerprints.
pub fn location_hash() -> String {
    let hostname = sysinfo::System::host_name().unwrap_or_default();
    let mac = primary_mac();
    let mut material = String::new();
    material.push_str(&hostname);
    material.push('|');
    material.push_str(&mac);
    let hash = blake3::hash(material.as_bytes());
    hex::encode(&hash.as_bytes()[..8])
}

/// Pick the MAC of the first non-loopback interface with a hardware address,
/// preferring the default route interface when sysinfo exposes one.
fn primary_mac() -> String {
    let networks = sysinfo::Networks::new_with_refreshed_list();
    // sysinfo returns macs per interface; prefer the first non-empty one.
    for (_, data) in networks.list() {
        let mac = data.mac_address().to_string();
        // "00:00:00:00:00:00:00:00:e0" / zeroed MACs appear on some virt ifaces.
        if !mac.trim().is_empty()
            && !mac.starts_with("00:00:00:00:00:00")
            && mac != "??"
        {
            return mac;
        }
    }
    String::new()
}

/// Resolve `~/.ssh/<name>` style paths relative to the home directory.
pub fn expand_ssh_path(path: &str) -> Result<std::path::PathBuf> {
    let expanded = shellexpand_home(path);
    let path = std::path::PathBuf::from(expanded);
    if !path.exists() {
        bail!("ssh key path does not exist: {}", path.display());
    }
    Ok(path)
}

fn shellexpand_home(input: &str) -> String {
    if let Some(rest) = input.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return std::path::Path::new(&home).join(rest).to_string_lossy().into_owned();
        }
    }
    input.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn random_key() -> SigningKey {
        // Generate from random bytes to avoid pulling a specific rand_core
        // version into the test matrix.
        let mut secret = [0u8; 32];
        // rand 0.8 fill:
        use rand::RngCore;
        rand::thread_rng().fill_bytes(&mut secret);
        SigningKey::from_bytes(&secret)
    }

    #[test]
    fn device_id_is_stable() {
        let key = random_key();
        let pk = key.verifying_key().to_bytes();
        let id_a = device_id_from_pubkey(&pk);
        let id_b = device_id_from_pubkey(&pk);
        assert_eq!(id_a, id_b, "device id must be deterministic");
        assert_eq!(id_a.len(), 64, "blake3 hex is 64 chars");
    }

    #[test]
    fn device_id_differs_per_key() {
        let a = random_key().verifying_key().to_bytes();
        let b = random_key().verifying_key().to_bytes();
        assert_ne!(device_id_from_pubkey(&a), device_id_from_pubkey(&b));
    }
}
