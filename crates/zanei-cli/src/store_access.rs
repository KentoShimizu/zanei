//! Opening the store with its encryption key.
//!
//! Every command that touches the store goes through here. The key comes from
//! the login Keychain, or — for development builds and CI, whose ad-hoc code
//! signature would trigger a Keychain dialog on every rebuild — from the file
//! named by `ZANEI_STORE_KEY_FILE`. The file's format, not configuration,
//! decides whether a key is needed at all (see `StoreFormat`).

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use zanei_core::store::{
    LockedReason, StoreError, StoreFormat, StoreKey, StoreReader, StoreWriter,
};
use zanei_macos::store_key::{KeychainError, KeychainInteraction, KeychainStoreKey};

/// Development override: read (and, for the recorder, create) the key in this
/// file instead of the Keychain. Not for everyday use — the key sits on disk.
pub(crate) const STORE_KEY_FILE_ENV: &str = "ZANEI_STORE_KEY_FILE";

/// Whether a missing key may be generated. Only the recorder creates keys.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KeyAccess {
    Existing,
    CreateIfMissing,
}

/// Whether a Keychain dialog may appear. Background processes say `Suppressed`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KeyPrompt {
    Allowed,
    Suppressed,
}

/// Where the key was found, for diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KeySource {
    Keychain,
    File,
}

#[must_use]
pub(crate) fn key_source() -> KeySource {
    if key_file_path().is_some() {
        KeySource::File
    } else {
        KeySource::Keychain
    }
}

fn key_file_path() -> Option<PathBuf> {
    std::env::var_os(STORE_KEY_FILE_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Loads the user's store key regardless of any particular store file.
pub(crate) fn load_store_key(
    access: KeyAccess,
    prompt: KeyPrompt,
) -> Result<Option<StoreKey>, StoreError> {
    match key_file_path() {
        Some(path) => key_from_file(&path, access),
        None => key_from_keychain(access, prompt),
    }
}

/// The key needed to open the store at `store`: `None` for plaintext or
/// missing stores, the key for encrypted ones, or `Locked` when it is gone.
pub(crate) fn store_key_for(
    store: &Path,
    prompt: KeyPrompt,
) -> Result<Option<StoreKey>, StoreError> {
    match StoreFormat::probe(store)? {
        StoreFormat::Encrypted => load_store_key(KeyAccess::Existing, prompt)?
            .map(Some)
            .ok_or(StoreError::Locked(LockedReason::KeyMissing)),
        StoreFormat::Plaintext | StoreFormat::Missing | StoreFormat::Unrecognized => Ok(None),
    }
}

pub(crate) fn open_reader(store: &Path, prompt: KeyPrompt) -> Result<StoreReader, StoreError> {
    let key = store_key_for(store, prompt)?;
    StoreReader::open_with_key(store, key.as_ref())
}

/// Opens the store for writing. A store that does not exist yet is created
/// encrypted, which needs a key: with [`KeyAccess::Existing`] and no key on
/// hand this fails rather than creating a plaintext store.
pub(crate) fn open_writer(
    store: &Path,
    access: KeyAccess,
    prompt: KeyPrompt,
) -> Result<StoreWriter, StoreError> {
    let key = match StoreFormat::probe(store)? {
        StoreFormat::Plaintext | StoreFormat::Unrecognized => None,
        StoreFormat::Encrypted | StoreFormat::Missing => Some(
            load_store_key(access, prompt)?.ok_or(StoreError::Locked(LockedReason::KeyMissing))?,
        ),
    };
    StoreWriter::open_with_key(store, key.as_ref())
}

/// `KeyProvider` for the MCP server: never prompts, never creates.
pub(crate) fn mcp_store_key(store: &Path) -> Result<Option<StoreKey>, String> {
    store_key_for(store, KeyPrompt::Suppressed).map_err(|error| error.to_string())
}

fn key_from_file(path: &Path, access: KeyAccess) -> Result<Option<StoreKey>, StoreError> {
    match fs::read_to_string(path) {
        Ok(text) => StoreKey::from_hex(&text).map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound => match access {
            KeyAccess::Existing => Ok(None),
            KeyAccess::CreateIfMissing => {
                let key = StoreKey::generate()?;
                write_key_file(path, &key)?;
                Ok(Some(key))
            }
        },
        Err(error) => Err(StoreError::io("read the store key file", error)),
    }
}

fn write_key_file(path: &Path, key: &StoreKey) -> Result<(), StoreError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| StoreError::io("create the store key file", error))?;
    writeln!(file, "{}", key.to_hex().as_str())
        .and_then(|()| file.sync_all())
        .map_err(|error| StoreError::io("write the store key file", error))
}

fn key_from_keychain(access: KeyAccess, prompt: KeyPrompt) -> Result<Option<StoreKey>, StoreError> {
    let keychain = KeychainStoreKey::default();
    let interaction = match prompt {
        KeyPrompt::Allowed => KeychainInteraction::Prompt,
        KeyPrompt::Suppressed => KeychainInteraction::NoPrompt,
    };
    if let Some(key) = keychain.load(interaction).map_err(locked)? {
        return Ok(Some(key));
    }
    match access {
        KeyAccess::Existing => Ok(None),
        KeyAccess::CreateIfMissing => {
            let key = StoreKey::generate()?;
            match keychain.store(&key) {
                Ok(()) => Ok(Some(key)),
                // Another recorder start won the race; its key is the one to use.
                Err(error) if KeychainStoreKey::is_duplicate(&error) => {
                    keychain.load(interaction).map_err(locked)
                }
                Err(error) => Err(locked(error)),
            }
        }
    }
}

fn locked(error: KeychainError) -> StoreError {
    StoreError::Locked(match error {
        KeychainError::Locked => LockedReason::KeychainLocked,
        KeychainError::Denied => LockedReason::KeychainDenied,
        other => LockedReason::KeyUnavailable(other.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use zanei_core::store::StoreKey;

    use super::{KeyAccess, key_from_file};

    #[test]
    fn key_file_is_created_once_and_read_back() {
        let directory = TempDir::new().expect("key directory");
        let path = directory.path().join("store.key");

        assert!(
            key_from_file(&path, KeyAccess::Existing)
                .expect("missing key file is not an error")
                .is_none()
        );
        let created = key_from_file(&path, KeyAccess::CreateIfMissing)
            .expect("create key file")
            .expect("key");
        let reloaded = key_from_file(&path, KeyAccess::Existing)
            .expect("read key file")
            .expect("key");
        assert_eq!(created.to_hex().as_str(), reloaded.to_hex().as_str());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .expect("key file metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }

        std::fs::write(&path, "not hex").expect("overwrite key file");
        assert!(key_from_file(&path, KeyAccess::Existing).is_err());
        let fixed = StoreKey::generate().expect("key");
        std::fs::write(&path, format!("{}\n", fixed.to_hex().as_str())).expect("write key");
        assert_eq!(
            key_from_file(&path, KeyAccess::Existing)
                .expect("read")
                .expect("key")
                .to_hex()
                .as_str(),
            fixed.to_hex().as_str()
        );
    }
}
