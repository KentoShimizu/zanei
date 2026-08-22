//! The store encryption key, kept as a generic-password item in the login Keychain.
//!
//! One item per macOS user (`service` = `dev.zanei.store`, `account` = `key`)
//! holds the 256-bit key as 64 hexadecimal characters, so `security
//! find-generic-password -s dev.zanei.store -w` prints it verbatim. The item's
//! access list is bound to the creating app's code signature: the CLI, the
//! launchd recorder, and the MCP server are one signed executable, so none of
//! them triggers a dialog.

use thiserror::Error;
use zanei_core::store::StoreKey;

use crate::ffi::keychain::{
    self, ERR_SEC_AUTH_FAILED, ERR_SEC_DUPLICATE_ITEM, ERR_SEC_INTERACTION_NOT_ALLOWED,
    ERR_SEC_USER_CANCELED, KeychainFailure,
};

/// `kSecAttrService` of the store key item.
pub const STORE_KEY_SERVICE: &str = "dev.zanei.store";
/// `kSecAttrAccount` of the store key item.
pub const STORE_KEY_ACCOUNT: &str = "key";
const STORE_KEY_LABEL: &str = "Zanei store key";
const STORE_KEY_DESCRIPTION: &str = "Encryption key for the local Zanei store";

/// Whether a Keychain call may put a dialog on screen.
///
/// Background processes (the recorder, the MCP server) must not: a dialog
/// nobody sees would stall them forever.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeychainInteraction {
    Prompt,
    NoPrompt,
}

#[derive(Debug, Error)]
pub enum KeychainError {
    #[error("the login Keychain is locked")]
    Locked,
    #[error("access to the store key in the login Keychain was denied")]
    Denied,
    #[error("the Keychain item \"{STORE_KEY_LABEL}\" does not hold a valid store key: {0}")]
    InvalidItem(String),
    #[error("failed to build a Keychain request")]
    Request,
    #[error("Keychain {operation} failed with OSStatus {status}")]
    Failed {
        operation: &'static str,
        status: i32,
    },
}

/// Handle to the store key item. `service` is configurable so tests can use a
/// throwaway item without touching the real one.
#[derive(Clone, Debug)]
pub struct KeychainStoreKey {
    service: String,
}

impl Default for KeychainStoreKey {
    fn default() -> Self {
        Self::with_service(STORE_KEY_SERVICE)
    }
}

impl KeychainStoreKey {
    #[must_use]
    pub fn with_service(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    /// Reads the key. `Ok(None)` when no item exists.
    pub fn load(
        &self,
        interaction: KeychainInteraction,
    ) -> Result<Option<StoreKey>, KeychainError> {
        let _guard = InteractionGuard::new(interaction);
        let data = keychain::find_generic_password(&self.service, STORE_KEY_ACCOUNT)
            .map_err(|failure| classify(failure, "read"))?;
        data.map(|bytes| {
            let text = String::from_utf8(bytes)
                .map_err(|_| KeychainError::InvalidItem("not UTF-8 text".to_owned()))?;
            StoreKey::from_hex(&text).map_err(|error| KeychainError::InvalidItem(error.to_string()))
        })
        .transpose()
    }

    /// Stores `key` as a new item. Fails when an item already exists; callers
    /// that raced another creator should reload instead of overwriting.
    pub fn store(&self, key: &StoreKey) -> Result<(), KeychainError> {
        let _guard = InteractionGuard::new(KeychainInteraction::NoPrompt);
        let hex = key.to_hex();
        keychain::add_generic_password(
            &self.service,
            STORE_KEY_ACCOUNT,
            STORE_KEY_LABEL,
            STORE_KEY_DESCRIPTION,
            hex.as_bytes(),
        )
        .map_err(|failure| classify(failure, "write"))
    }

    /// Whether a `store` failure was a duplicate-item race.
    #[must_use]
    pub fn is_duplicate(error: &KeychainError) -> bool {
        matches!(
            error,
            KeychainError::Failed {
                status: ERR_SEC_DUPLICATE_ITEM,
                ..
            }
        )
    }

    /// Deletes the item. `Ok(false)` when there was none.
    pub fn delete(&self) -> Result<bool, KeychainError> {
        let _guard = InteractionGuard::new(KeychainInteraction::NoPrompt);
        keychain::delete_generic_password(&self.service, STORE_KEY_ACCOUNT)
            .map_err(|failure| classify(failure, "delete"))
    }
}

fn classify(failure: KeychainFailure, operation: &'static str) -> KeychainError {
    match failure {
        KeychainFailure::Allocation => KeychainError::Request,
        KeychainFailure::Status(ERR_SEC_INTERACTION_NOT_ALLOWED) => {
            // The same status covers a locked keychain and an access-control
            // prompt that could not be shown; the lock state tells them apart.
            if keychain::default_keychain_is_unlocked() == Some(false) {
                KeychainError::Locked
            } else {
                KeychainError::Denied
            }
        }
        KeychainFailure::Status(ERR_SEC_AUTH_FAILED | ERR_SEC_USER_CANCELED) => {
            KeychainError::Denied
        }
        KeychainFailure::Status(status) => KeychainError::Failed { operation, status },
    }
}

/// Suppresses keychain dialogs for the duration of a call and restores them after.
struct InteractionGuard {
    suppressed: bool,
}

impl InteractionGuard {
    fn new(interaction: KeychainInteraction) -> Self {
        let suppressed = interaction == KeychainInteraction::NoPrompt;
        if suppressed {
            keychain::set_user_interaction_allowed(false);
        }
        Self { suppressed }
    }
}

impl Drop for InteractionGuard {
    fn drop(&mut self) {
        if self.suppressed {
            keychain::set_user_interaction_allowed(true);
        }
    }
}
