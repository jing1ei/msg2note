//! Bot-token storage backed by the macOS Keychain.
//!
//! Tokens are kept out of `bots.json` entirely: the config file stores only the
//! non-secret bot metadata, while each token lives in the login Keychain under
//! the fixed service `com.notekeeper.app`, keyed by the bot's UUID. This keeps secrets
//! out of plaintext on disk (and out of any backup of the config file).

use crate::config::Config;
use keyring::Entry;

/// Keychain service name. Kept fixed at the original `com.notekeeper.app` on
/// purpose — it is intentionally *not* derived from the Tauri bundle identifier,
/// so renaming the bundle id (e.g. away from the `.app` suffix macOS dislikes)
/// never strands previously stored tokens under an old service name.
const SERVICE: &str = "com.notekeeper.app";

/// Fixed Keychain key for the global local-server `api_hash`. The leading
/// underscores keep it from ever colliding with a bot's UUID.
const SERVER_HASH_KEY: &str = "__server_api_hash__";

/// Read the local-server `api_hash` from the Keychain.
pub fn get_server_api_hash() -> Option<String> {
    get_token(SERVER_HASH_KEY)
}

/// Store (or replace) the local-server `api_hash` in the Keychain.
pub fn set_server_api_hash(hash: &str) -> Result<(), String> {
    set_token(SERVER_HASH_KEY, hash)
}

/// Remove the local-server `api_hash` from the Keychain. Used to roll back to
/// "no hash stored" — writing an empty string instead would leave a Keychain
/// entry behind that reads back as `Some("")`.
pub fn clear_server_api_hash() {
    delete_token(SERVER_HASH_KEY);
}

/// Read a bot's token from the Keychain. Returns `None` if there is no entry
/// (or the Keychain is unavailable).
pub fn get_token(id: &str) -> Option<String> {
    Entry::new(SERVICE, id).ok()?.get_password().ok()
}

/// Store (or replace) a bot's token in the Keychain.
pub fn set_token(id: &str, token: &str) -> Result<(), String> {
    Entry::new(SERVICE, id)
        .and_then(|e| e.set_password(token))
        .map_err(|e| e.to_string())
}

/// Delete a bot's token from the Keychain. Missing entries are ignored.
pub fn delete_token(id: &str) {
    if let Ok(e) = Entry::new(SERVICE, id) {
        let _ = e.delete_credential();
    }
}

/// Fill each bot's in-memory token from the Keychain.
///
/// If a bot has no Keychain entry but still carries a token in memory (i.e. it
/// was just loaded from an older plaintext `bots.json`), the token is migrated
/// into the Keychain. Returns `true` if any token was migrated, signalling the
/// caller to re-save the config so the cleartext token is removed from disk.
pub fn hydrate_tokens(config: &mut Config) -> bool {
    let mut migrated = false;
    for b in &mut config.bots {
        match get_token(&b.id) {
            Some(tok) => b.token = tok,
            None => {
                if !b.token.is_empty() && set_token(&b.id, &b.token).is_ok() {
                    migrated = true;
                }
            }
        }
    }
    migrated
}
