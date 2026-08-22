//! Opening the store with its encryption key.
//!
//! Every command that touches the store goes through here. The key comes from
//! the platform's credential store (the macOS login Keychain) through the
//! platform-neutral `KeyStore` trait, or — for development builds and CI,
//! whose ad-hoc code signature would trigger a Keychain dialog on every
//! rebuild — from the file named by `ZANEI_STORE_KEY_FILE`. The file's format,
//! not configuration, decides whether a key is needed at all (see
//! `StoreFormat`). Adding a platform means implementing `KeyStore` once and
//! returning it from [`platform_key_store`].

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use zanei_core::store::{
    KeyStore, KeyStoreError, KeyStoreInteraction, LockedReason, StoreError, StoreFormat, StoreKey,
    StoreReader, StoreWriter, load_or_create,
};

/// Development override: read (and, for the recorder, create) the key in this
/// file instead of the platform key store. Not for everyday use — the key sits
/// on disk.
pub(crate) const STORE_KEY_FILE_ENV: &str = "ZANEI_STORE_KEY_FILE";

/// Whether a missing key may be generated. Only the recorder creates keys.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KeyAccess {
    Existing,
    CreateIfMissing,
}

/// Whether a platform dialog may appear. Background processes say `Suppressed`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KeyPrompt {
    Allowed,
    Suppressed,
}

/// The key file path from the environment, when the override is active.
pub(crate) fn key_file_override() -> Option<PathBuf> {
    std::env::var_os(STORE_KEY_FILE_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// The key store this process uses: the override file when set, otherwise the
/// platform's credential store.
pub(crate) fn key_store() -> Box<dyn KeyStore> {
    match key_file_override() {
        Some(path) => Box::new(FileKeyStore { path }),
        None => platform_key_store(),
    }
}

#[cfg(target_os = "macos")]
fn platform_key_store() -> Box<dyn KeyStore> {
    Box::new(zanei_macos::store_key::KeychainStoreKey::default())
}

#[cfg(not(target_os = "macos"))]
fn platform_key_store() -> Box<dyn KeyStore> {
    Box::new(UnsupportedKeyStore)
}

/// Placeholder until a platform implements `KeyStore`; only the key file works there.
#[cfg(not(target_os = "macos"))]
struct UnsupportedKeyStore;

#[cfg(not(target_os = "macos"))]
impl KeyStore for UnsupportedKeyStore {
    fn location(&self) -> String {
        "no key store on this platform yet".to_owned()
    }

    fn load(&self, _: KeyStoreInteraction) -> Result<Option<StoreKey>, KeyStoreError> {
        Err(KeyStoreError::Unavailable(format!(
            "this platform has no key store yet; set {STORE_KEY_FILE_ENV}"
        )))
    }

    fn store(&self, _: &StoreKey) -> Result<(), KeyStoreError> {
        self.load(KeyStoreInteraction::NoPrompt).map(|_| ())
    }

    fn delete(&self) -> Result<bool, KeyStoreError> {
        self.load(KeyStoreInteraction::NoPrompt).map(|_| false)
    }
}

/// Loads the user's store key regardless of any particular store file.
pub(crate) fn load_store_key(
    access: KeyAccess,
    prompt: KeyPrompt,
) -> Result<Option<StoreKey>, StoreError> {
    let interaction = match prompt {
        KeyPrompt::Allowed => KeyStoreInteraction::Prompt,
        KeyPrompt::Suppressed => KeyStoreInteraction::NoPrompt,
    };
    load_or_create(
        &*key_store(),
        access == KeyAccess::CreateIfMissing,
        interaction,
    )
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
/// hand this fails rather than creating a plaintext store. An encrypted store
/// only ever gets its existing key, whatever `access` says.
pub(crate) fn open_writer(
    store: &Path,
    access: KeyAccess,
    prompt: KeyPrompt,
) -> Result<StoreWriter, StoreError> {
    let key = match StoreFormat::probe(store)? {
        StoreFormat::Plaintext | StoreFormat::Unrecognized => None,
        StoreFormat::Encrypted => Some(
            load_store_key(KeyAccess::Existing, prompt)?
                .ok_or(StoreError::Locked(LockedReason::KeyMissing))?,
        ),
        StoreFormat::Missing => Some(
            load_store_key(access, prompt)?.ok_or(StoreError::Locked(LockedReason::KeyMissing))?,
        ),
    };
    StoreWriter::open_with_key(store, key.as_ref())
}

/// `KeyProvider` for the MCP server: never prompts, never creates.
pub(crate) fn mcp_store_key(store: &Path) -> Result<Option<StoreKey>, String> {
    store_key_for(store, KeyPrompt::Suppressed).map_err(|error| error.to_string())
}

/// The development override: one hexadecimal key in a file.
struct FileKeyStore {
    path: PathBuf,
}

impl KeyStore for FileKeyStore {
    fn location(&self) -> String {
        format!(
            "the key file {} ({STORE_KEY_FILE_ENV}, development override)",
            self.path.display()
        )
    }

    fn load(&self, _: KeyStoreInteraction) -> Result<Option<StoreKey>, KeyStoreError> {
        match fs::read_to_string(&self.path) {
            Ok(text) => StoreKey::from_hex(&text)
                .map(Some)
                .map_err(|error| KeyStoreError::InvalidItem(error.to_string())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(KeyStoreError::Unavailable(format!(
                "failed to read the store key file {}: {error}",
                self.path.display()
            ))),
        }
    }

    fn store(&self, key: &StoreKey) -> Result<(), KeyStoreError> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = match options.open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(KeyStoreError::AlreadyExists);
            }
            Err(error) => {
                return Err(KeyStoreError::Unavailable(format!(
                    "failed to create the store key file {}: {error}",
                    self.path.display()
                )));
            }
        };
        writeln!(file, "{}", key.to_hex().as_str())
            .and_then(|()| file.sync_all())
            .map_err(|error| {
                KeyStoreError::Unavailable(format!(
                    "failed to write the store key file {}: {error}",
                    self.path.display()
                ))
            })
    }

    fn delete(&self) -> Result<bool, KeyStoreError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(KeyStoreError::Unavailable(format!(
                "failed to remove the store key file {}: {error}",
                self.path.display()
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use zanei_core::store::{KeyStore, KeyStoreError, KeyStoreInteraction, StoreKey};

    use super::FileKeyStore;

    #[test]
    fn key_file_is_created_once_and_read_back() {
        let directory = TempDir::new().expect("key directory");
        let store = FileKeyStore {
            path: directory.path().join("store.key"),
        };

        assert!(
            store
                .load(KeyStoreInteraction::NoPrompt)
                .expect("missing key file is not an error")
                .is_none()
        );
        let created = StoreKey::generate().expect("key");
        store.store(&created).expect("create key file");
        assert_eq!(
            store
                .store(&created)
                .expect_err("second store must not overwrite"),
            KeyStoreError::AlreadyExists
        );
        let reloaded = store
            .load(KeyStoreInteraction::NoPrompt)
            .expect("read key file")
            .expect("key");
        assert_eq!(created.to_hex().as_str(), reloaded.to_hex().as_str());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&store.path)
                .expect("key file metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }

        std::fs::write(&store.path, "not hex").expect("overwrite key file");
        assert!(matches!(
            store.load(KeyStoreInteraction::NoPrompt),
            Err(KeyStoreError::InvalidItem(_))
        ));
        assert!(store.delete().expect("delete key file"));
        assert!(!store.delete().expect("delete absent key file"));
    }
}
