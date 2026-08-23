//! Where the store key lives: a platform-neutral contract.
//!
//! The core never talks to a keychain. A platform crate implements [`KeyStore`]
//! for its credential store (the macOS login Keychain today; Windows and Linux
//! add their own), the CLI implements it for the development key file, and the
//! code that opens stores only ever sees the trait. Error variants carry the
//! platform's own wording so core can stay silent about keychains, credential
//! managers, or secret services.

use super::{LockedReason, StoreError, StoreKey};

/// Whether a call may put a platform dialog on screen.
///
/// Background processes (the recorder, the MCP server) must not: a dialog
/// nobody sees would stall them forever.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyStoreInteraction {
    Prompt,
    NoPrompt,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KeyStoreError {
    /// The key store is locked. `advice` tells the user how to unlock it.
    Locked { advice: String },
    /// The platform refused this process access. `advice` tells the user what to do.
    Denied { advice: String },
    /// [`KeyStore::store`] found an item already present (lost a creation race).
    AlreadyExists,
    /// The item exists but does not hold a valid key.
    InvalidItem(String),
    /// Anything else, already described for the user.
    Unavailable(String),
}

impl std::fmt::Display for KeyStoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Locked { advice } | Self::Denied { advice } => formatter.write_str(advice),
            Self::AlreadyExists => formatter.write_str("a store key already exists"),
            Self::InvalidItem(detail) => {
                write!(
                    formatter,
                    "the stored item is not a valid store key: {detail}"
                )
            }
            Self::Unavailable(detail) => formatter.write_str(detail),
        }
    }
}

impl std::error::Error for KeyStoreError {}

impl From<KeyStoreError> for StoreError {
    fn from(error: KeyStoreError) -> Self {
        Self::Locked(match error {
            KeyStoreError::Locked { advice } => LockedReason::KeyStoreLocked(advice),
            KeyStoreError::Denied { advice } => LockedReason::KeyStoreDenied(advice),
            other => LockedReason::KeyUnavailable(other.to_string()),
        })
    }
}

/// A place that holds the user's single store key.
pub trait KeyStore {
    /// Where the key is, for diagnostics: "the login Keychain", "the key file …".
    fn location(&self) -> String;

    /// Reads the key. `Ok(None)` when no key has been stored yet.
    fn load(&self, interaction: KeyStoreInteraction) -> Result<Option<StoreKey>, KeyStoreError>;

    /// Stores `key` as a new item. Must fail with [`KeyStoreError::AlreadyExists`]
    /// rather than overwrite when an item is already present.
    fn store(&self, key: &StoreKey) -> Result<(), KeyStoreError>;

    /// Deletes the item. `Ok(false)` when there was none.
    fn delete(&self) -> Result<bool, KeyStoreError>;
}

/// Loads the key from `store`, generating and storing one when `create` is set
/// and none exists. Two creators racing each other converge on the winner's key.
pub fn load_or_create(
    store: &dyn KeyStore,
    create: bool,
    interaction: KeyStoreInteraction,
) -> Result<Option<StoreKey>, StoreError> {
    if let Some(key) = store.load(interaction)? {
        return Ok(Some(key));
    }
    if !create {
        return Ok(None);
    }
    let key = StoreKey::generate()?;
    match store.store(&key) {
        Ok(()) => Ok(Some(key)),
        Err(KeyStoreError::AlreadyExists) => Ok(store.load(interaction)?),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::{KeyStore, KeyStoreError, KeyStoreInteraction, load_or_create};
    use crate::store::{LockedReason, StoreError, StoreKey};

    struct MemoryStore {
        key: RefCell<Option<StoreKey>>,
        fail_store_with_duplicate: bool,
    }

    impl KeyStore for MemoryStore {
        fn location(&self) -> String {
            "memory".to_owned()
        }

        fn load(&self, _: KeyStoreInteraction) -> Result<Option<StoreKey>, KeyStoreError> {
            Ok(self.key.borrow().clone())
        }

        fn store(&self, key: &StoreKey) -> Result<(), KeyStoreError> {
            if self.fail_store_with_duplicate {
                *self.key.borrow_mut() = Some(StoreKey::generate().expect("racing key"));
                return Err(KeyStoreError::AlreadyExists);
            }
            if self.key.borrow().is_some() {
                return Err(KeyStoreError::AlreadyExists);
            }
            *self.key.borrow_mut() = Some(key.clone());
            Ok(())
        }

        fn delete(&self) -> Result<bool, KeyStoreError> {
            Ok(self.key.borrow_mut().take().is_some())
        }
    }

    #[test]
    fn load_or_create_generates_once_and_reuses_the_stored_key() {
        let store = MemoryStore {
            key: RefCell::new(None),
            fail_store_with_duplicate: false,
        };
        assert!(
            load_or_create(&store, false, KeyStoreInteraction::NoPrompt)
                .expect("load without create")
                .is_none()
        );
        let created = load_or_create(&store, true, KeyStoreInteraction::NoPrompt)
            .expect("create")
            .expect("key");
        let again = load_or_create(&store, true, KeyStoreInteraction::NoPrompt)
            .expect("reload")
            .expect("key");
        assert_eq!(created.to_hex().as_str(), again.to_hex().as_str());
    }

    #[test]
    fn load_or_create_adopts_the_winner_of_a_creation_race() {
        let store = MemoryStore {
            key: RefCell::new(None),
            fail_store_with_duplicate: true,
        };
        let adopted = load_or_create(&store, true, KeyStoreInteraction::NoPrompt)
            .expect("adopt racing key")
            .expect("key");
        let stored = store.key.borrow().clone().expect("racing key stored");
        assert_eq!(adopted.to_hex().as_str(), stored.to_hex().as_str());
    }

    #[test]
    fn key_store_errors_become_locked_reasons_with_platform_advice() {
        let locked: StoreError = KeyStoreError::Locked {
            advice: "unlock it".to_owned(),
        }
        .into();
        assert!(matches!(
            locked,
            StoreError::Locked(LockedReason::KeyStoreLocked(advice)) if advice == "unlock it"
        ));
        let denied: StoreError = KeyStoreError::Denied {
            advice: "allow it".to_owned(),
        }
        .into();
        assert!(denied.to_string().ends_with("allow it"));
        let other: StoreError = KeyStoreError::InvalidItem("short".to_owned()).into();
        assert!(matches!(
            other,
            StoreError::Locked(LockedReason::KeyUnavailable(_))
        ));
    }
}
