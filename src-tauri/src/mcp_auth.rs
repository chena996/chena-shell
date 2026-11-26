//! Local MCP access credentials. Only a salted SHA-256 digest is persisted.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::RngCore;
use sha2::{Digest, Sha256};

use crate::state::AppState;

const CREDENTIAL_VERSION: &str = "v1";

pub fn generate_credential(state: &AppState) -> Result<String, String> {
    let mut token_bytes = [0u8; 32];
    let mut salt = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut token_bytes);
    rand::thread_rng().fill_bytes(&mut salt);
    let token = format!("csh_{}", URL_SAFE_NO_PAD.encode(token_bytes));
    let encoded_salt = URL_SAFE_NO_PAD.encode(salt);
    let digest = digest_token(&encoded_salt, &token);
    let stored = format!(
        "{CREDENTIAL_VERSION}${}${}",
        encoded_salt,
        URL_SAFE_NO_PAD.encode(digest)
    );
    crate::state::save_mcp_credential_hash(Some(stored.clone()))?;
    state.inner.write().mcp_credential_hash = Some(stored);
    Ok(token)
}

pub fn configured(state: &AppState) -> bool {
    state.inner.read().mcp_credential_hash.is_some()
}

pub fn verify(state: &AppState, token: &str) -> bool {
    let Some(stored) = state.inner.read().mcp_credential_hash.clone() else {
        return false;
    };
    verify_stored(&stored, token)
}

fn digest_token(encoded_salt: &str, token: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(encoded_salt.as_bytes());
    hasher.update(b"\0");
    hasher.update(token.as_bytes());
    hasher.finalize().into()
}

fn verify_stored(stored: &str, token: &str) -> bool {
    let mut parts = stored.split('$');
    let (Some(version), Some(salt), Some(expected), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    if version != CREDENTIAL_VERSION || token.len() < 16 {
        return false;
    }
    let actual = URL_SAFE_NO_PAD.encode(digest_token(salt, token));
    constant_time_eq(actual.as_bytes(), expected.as_bytes())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}

#[cfg(any(test, feature = "mcp-harness"))]
pub(crate) fn install_test_credential(state: &AppState, token: &str) {
    let salt = URL_SAFE_NO_PAD.encode([11u8; 16]);
    let stored = format!(
        "v1${}${}",
        salt,
        URL_SAFE_NO_PAD.encode(digest_token(&salt, token))
    );
    state.inner.write().mcp_credential_hash = Some(stored);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_verifies_without_storing_the_token() {
        let salt = URL_SAFE_NO_PAD.encode([7u8; 16]);
        let token = "csh_a-very-long-local-token";
        let stored = format!(
            "v1${}${}",
            salt,
            URL_SAFE_NO_PAD.encode(digest_token(&salt, token))
        );
        assert!(verify_stored(&stored, token));
        assert!(!verify_stored(&stored, "csh_wrong-token"));
        assert!(!stored.contains(token));
    }

    #[test]
    fn malformed_hashes_fail_closed() {
        assert!(!verify_stored("", "csh_a-very-long-local-token"));
        assert!(!verify_stored("v1$bad", "csh_a-very-long-local-token"));
        assert!(!verify_stored("v2$a$b", "csh_a-very-long-local-token"));
    }
}
