//! The store encryption key, kept as a generic-password item in the login Keychain.
//!
//! This is the macOS implementation of [`KeyStore`]. One item per macOS user
//! (`service` = `dev.zanei.store`, `account` = `key`) holds the 256-bit key as
//! 64 hexadecimal characters, so `security find-generic-password -s
//! dev.zanei.store -w` prints it verbatim. The item's access list is bound to
//! the creating app's code signature: the CLI, the launchd recorder, and the
//! MCP server are one signed executable, so none of them triggers a dialog.

use zanei_core::store::{KeyStore, KeyStoreError, KeyStoreInteraction, StoreKey};

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

const LOCKED_ADVICE: &str = "your login Keychain is locked; unlock it (for example by opening \
                             Keychain Access) and try again";
const DENIED_ADVICE: &str = "macOS denied this process access to the store key in your login \
                             Keychain; allow Zanei in the Keychain dialog or use the signed \
                             Zanei.app build";

/// The store key item in the login Keychain. `service` is configurable so tests
/// can use a throwaway item without touching the real one.
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
}

impl KeyStore for KeychainStoreKey {
    fn location(&self) -> String {
        format!("the login Keychain (item \"{STORE_KEY_LABEL}\")")
    }

    fn load(&self, interaction: KeyStoreInteraction) -> Result<Option<StoreKey>, KeyStoreError> {
        let _guard = InteractionGuard::new(interaction);
        let data = keychain::find_generic_password(&self.service, STORE_KEY_ACCOUNT)
            .map_err(|failure| classify(failure, "read"))?;
        data.map(|bytes| {
            let text = String::from_utf8(bytes)
                .map_err(|_| KeyStoreError::InvalidItem("not UTF-8 text".to_owned()))?;
            StoreKey::from_hex(&text).map_err(|error| KeyStoreError::InvalidItem(error.to_string()))
        })
        .transpose()
    }

    fn store(&self, key: &StoreKey) -> Result<(), KeyStoreError> {
        let _guard = InteractionGuard::new(KeyStoreInteraction::NoPrompt);
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

    fn delete(&self) -> Result<bool, KeyStoreError> {
        let _guard = InteractionGuard::new(KeyStoreInteraction::NoPrompt);
        keychain::delete_generic_password(&self.service, STORE_KEY_ACCOUNT)
            .map_err(|failure| classify(failure, "delete"))
    }
}

fn classify(failure: KeychainFailure, operation: &'static str) -> KeyStoreError {
    match failure {
        KeychainFailure::Allocation => {
            KeyStoreError::Unavailable("failed to build a Keychain request".to_owned())
        }
        KeychainFailure::Status(ERR_SEC_DUPLICATE_ITEM) => KeyStoreError::AlreadyExists,
        KeychainFailure::Status(ERR_SEC_INTERACTION_NOT_ALLOWED) => {
            // The same status covers a locked keychain and an access-control
            // prompt that could not be shown; the lock state tells them apart.
            if keychain::default_keychain_is_unlocked() == Some(false) {
                KeyStoreError::Locked {
                    advice: LOCKED_ADVICE.to_owned(),
                }
            } else {
                KeyStoreError::Denied {
                    advice: DENIED_ADVICE.to_owned(),
                }
            }
        }
        KeychainFailure::Status(ERR_SEC_AUTH_FAILED | ERR_SEC_USER_CANCELED) => {
            KeyStoreError::Denied {
                advice: DENIED_ADVICE.to_owned(),
            }
        }
        KeychainFailure::Status(status) => KeyStoreError::Unavailable(format!(
            "Keychain {operation} failed with OSStatus {status}"
        )),
    }
}

/// Suppresses keychain dialogs for the duration of a call and restores them after.
struct InteractionGuard {
    suppressed: bool,
}

impl InteractionGuard {
    fn new(interaction: KeyStoreInteraction) -> Self {
        let suppressed = interaction == KeyStoreInteraction::NoPrompt;
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
