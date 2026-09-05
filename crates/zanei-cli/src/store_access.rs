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

use std::ffi::OsStr;
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
pub(crate) const KEYCHAIN_SERVICE_ENV: &str = "ZANEI_KEYCHAIN_SERVICE";
pub(crate) const KEYCHAIN_LABEL_ENV: &str = "ZANEI_KEYCHAIN_LABEL";
pub(crate) const KEYCHAIN_NO_PROMPT_ENV: &str = "ZANEI_KEYCHAIN_NO_PROMPT";

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

#[derive(Clone, Debug, Eq, PartialEq)]
enum KeychainIdentity {
    Default,
    Custom { service: String, label: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct KeyEnvironment {
    keychain_identity: KeychainIdentity,
    suppress_prompts: bool,
}

impl KeyEnvironment {
    pub(crate) fn uses_custom_keychain_identity(&self) -> bool {
        matches!(self.keychain_identity, KeychainIdentity::Custom { .. })
    }
}

/// Resolves the process-local key boundary. The service and label form one
/// identity and therefore cannot be supplied independently.
pub(crate) fn initialize_key_environment() -> Result<KeyEnvironment, String> {
    key_environment_from(
        std::env::var_os(KEYCHAIN_SERVICE_ENV).as_deref(),
        std::env::var_os(KEYCHAIN_LABEL_ENV).as_deref(),
        std::env::var_os(KEYCHAIN_NO_PROMPT_ENV).as_deref(),
    )
}

fn key_environment_from(
    service: Option<&OsStr>,
    label: Option<&OsStr>,
    no_prompt: Option<&OsStr>,
) -> Result<KeyEnvironment, String> {
    let keychain_identity = match (service, label) {
        (None, None) => KeychainIdentity::Default,
        (Some(service), Some(label)) => KeychainIdentity::Custom {
            service: identity_value(KEYCHAIN_SERVICE_ENV, service)?,
            label: identity_value(KEYCHAIN_LABEL_ENV, label)?,
        },
        _ => {
            return Err(format!(
                "{KEYCHAIN_SERVICE_ENV} and {KEYCHAIN_LABEL_ENV} must be set together"
            ));
        }
    };
    let suppress_prompts = match no_prompt {
        None => false,
        Some(value) if value == "1" => true,
        Some(_) => {
            return Err(format!(
                "{KEYCHAIN_NO_PROMPT_ENV} must be exactly 1 when set"
            ));
        }
    };
    Ok(KeyEnvironment {
        keychain_identity,
        suppress_prompts,
    })
}

fn identity_value(name: &str, value: &OsStr) -> Result<String, String> {
    let value = value
        .to_str()
        .ok_or_else(|| format!("{name} must be valid UTF-8"))?;
    if value.trim().is_empty() {
        return Err(format!("{name} must not be empty"));
    }
    if value.chars().any(char::is_control) {
        return Err(format!("{name} must not contain control characters"));
    }
    Ok(value.to_owned())
}

/// The key file path from the environment, when the override is active. It is
/// made absolute here, once: the recorder receives it through the launch agent
/// and runs with a different working directory than the shell that set it.
pub(crate) fn key_file_override() -> Option<PathBuf> {
    std::env::var_os(STORE_KEY_FILE_ENV)
        .filter(|value| !value.is_empty())
        .map(|value| absolute_key_file(PathBuf::from(value)))
}

fn absolute_key_file(path: PathBuf) -> PathBuf {
    std::path::absolute(&path).unwrap_or(path)
}

/// The key store this process uses: the override file when set, otherwise the
/// platform's credential store.
pub(crate) fn key_store() -> Result<Box<dyn KeyStore>, StoreError> {
    let environment = initialize_key_environment().map_err(invalid_key_environment)?;
    Ok(key_store_for(&environment))
}

fn key_store_for(environment: &KeyEnvironment) -> Box<dyn KeyStore> {
    match key_file_override() {
        Some(path) => Box::new(FileKeyStore { path }),
        None => platform_key_store(&environment.keychain_identity),
    }
}

#[cfg(target_os = "macos")]
fn platform_key_store(identity: &KeychainIdentity) -> Box<dyn KeyStore> {
    match identity {
        KeychainIdentity::Default => Box::new(zanei_macos::store_key::KeychainStoreKey::default()),
        KeychainIdentity::Custom { service, label } => {
            Box::new(zanei_macos::store_key::KeychainStoreKey::with_service(
                service.as_str(),
                label.as_str(),
            ))
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn platform_key_store(_: &KeychainIdentity) -> Box<dyn KeyStore> {
    Box::new(UnsupportedKeyStore)
}

fn invalid_key_environment(message: String) -> StoreError {
    StoreError::Locked(LockedReason::KeyUnavailable(message))
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
    let environment = initialize_key_environment().map_err(invalid_key_environment)?;
    let store = key_store_for(&environment);
    load_store_key_from(&*store, access, prompt, &environment)
}

fn load_store_key_from(
    store: &dyn KeyStore,
    access: KeyAccess,
    prompt: KeyPrompt,
    environment: &KeyEnvironment,
) -> Result<Option<StoreKey>, StoreError> {
    load_or_create(
        store,
        access == KeyAccess::CreateIfMissing,
        interaction_for(prompt, environment.suppress_prompts),
    )
}

fn interaction_for(prompt: KeyPrompt, suppress_prompts: bool) -> KeyStoreInteraction {
    match (prompt, suppress_prompts) {
        (KeyPrompt::Suppressed, _) | (_, true) => KeyStoreInteraction::NoPrompt,
        (KeyPrompt::Allowed, false) => KeyStoreInteraction::Prompt,
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

/// Creates the key file's directory when it is missing, owner-only like the
/// store's directory. An existing directory keeps its permissions.
fn ensure_parent_directory(path: &Path) -> io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    if parent.as_os_str().is_empty() || parent.is_dir() {
        return Ok(());
    }
    fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
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

    /// Writes the key to a private temporary file in the same directory
    /// (created owner-only when it is missing), syncs
    /// it, and links it into place. The final name therefore never holds a
    /// half-written key, and two creators racing each other cannot both win:
    /// the link fails with `AlreadyExists` for the loser, who then adopts the
    /// winner's key.
    fn store(&self, key: &StoreKey) -> Result<(), KeyStoreError> {
        let unavailable = |operation: &str, error: io::Error| {
            KeyStoreError::Unavailable(format!(
                "failed to {operation} the store key file {}: {error}",
                self.path.display()
            ))
        };
        ensure_parent_directory(&self.path)
            .map_err(|error| unavailable("create the directory for", error))?;
        let mut staging = self.path.as_os_str().to_os_string();
        staging.push(format!(".tmp-{}", std::process::id()));
        let staging = PathBuf::from(staging);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let written = options
            .open(&staging)
            .map_err(|error| unavailable("create", error))
            .and_then(|mut file| {
                writeln!(file, "{}", key.to_hex().as_str())
                    .and_then(|()| file.sync_all())
                    .map_err(|error| unavailable("write", error))
            })
            .and_then(|()| match fs::hard_link(&staging, &self.path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    Err(KeyStoreError::AlreadyExists)
                }
                Err(error) => Err(unavailable("publish", error)),
            });
        let _ = fs::remove_file(&staging);
        written
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
    use std::cell::Cell;
    use std::ffi::OsStr;

    use tempfile::TempDir;
    use zanei_core::store::{KeyStore, KeyStoreError, KeyStoreInteraction, StoreKey};

    use super::{
        FileKeyStore, KeyAccess, KeyPrompt, absolute_key_file, key_environment_from,
        load_store_key_from,
    };

    struct InteractionStore {
        interaction: Cell<Option<KeyStoreInteraction>>,
    }

    impl KeyStore for InteractionStore {
        fn location(&self) -> String {
            "test store".to_owned()
        }

        fn load(
            &self,
            interaction: KeyStoreInteraction,
        ) -> Result<Option<StoreKey>, KeyStoreError> {
            self.interaction.set(Some(interaction));
            Ok(None)
        }

        fn store(&self, _: &StoreKey) -> Result<(), KeyStoreError> {
            panic!("existing-only read must not create a key")
        }

        fn delete(&self) -> Result<bool, KeyStoreError> {
            panic!("read must not delete a key")
        }
    }

    #[test]
    fn key_file_override_is_resolved_against_the_current_directory() {
        let resolved = absolute_key_file(std::path::PathBuf::from("dev.key"));
        assert!(resolved.is_absolute());
        assert_eq!(
            resolved,
            std::env::current_dir().expect("cwd").join("dev.key")
        );
        assert_eq!(
            absolute_key_file(std::path::PathBuf::from("/tmp/dev.key")),
            std::path::PathBuf::from("/tmp/dev.key")
        );
    }

    #[test]
    fn custom_keychain_identity_requires_complete_valid_values() {
        let default = key_environment_from(None, None, None).expect("default identity");
        assert!(!default.uses_custom_keychain_identity());

        let custom = key_environment_from(
            Some(OsStr::new("dev.example.subject.store")),
            Some(OsStr::new("Example subject store key")),
            None,
        )
        .expect("custom identity");
        assert!(custom.uses_custom_keychain_identity());

        for (service, label) in [
            (Some(OsStr::new("dev.example.subject.store")), None),
            (None, Some(OsStr::new("Example subject store key"))),
            (Some(OsStr::new("")), Some(OsStr::new("label"))),
            (Some(OsStr::new("service")), Some(OsStr::new(" \t"))),
            (Some(OsStr::new("service\n")), Some(OsStr::new("label"))),
        ] {
            assert!(key_environment_from(service, label, None).is_err());
        }
        assert!(
            key_environment_from(None, None, Some(OsStr::new("true"))).is_err(),
            "an invalid prompt policy must not fall back to allowing prompts"
        );
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;

            assert!(
                key_environment_from(
                    Some(OsStr::from_bytes(b"\xff")),
                    Some(OsStr::new("label")),
                    None,
                )
                .is_err()
            );
        }
    }

    #[test]
    fn no_prompt_environment_and_explicit_suppression_reach_the_key_store() {
        let store = InteractionStore {
            interaction: Cell::new(None),
        };
        let no_prompt =
            key_environment_from(None, None, Some(OsStr::new("1"))).expect("no-prompt environment");
        load_store_key_from(&store, KeyAccess::Existing, KeyPrompt::Allowed, &no_prompt)
            .expect("read key");
        assert_eq!(store.interaction.get(), Some(KeyStoreInteraction::NoPrompt));

        let default = key_environment_from(None, None, None).expect("default environment");
        load_store_key_from(&store, KeyAccess::Existing, KeyPrompt::Suppressed, &default)
            .expect("read key");
        assert_eq!(store.interaction.get(), Some(KeyStoreInteraction::NoPrompt));

        load_store_key_from(&store, KeyAccess::Existing, KeyPrompt::Allowed, &default)
            .expect("read key");
        assert_eq!(store.interaction.get(), Some(KeyStoreInteraction::Prompt));
    }

    #[test]
    fn key_file_directory_is_created_owner_only_when_missing() {
        let directory = TempDir::new().expect("key directory");
        let parent = directory.path().join("config").join("zanei");
        let store = FileKeyStore {
            path: parent.join("dev.key"),
        };

        store
            .store(&StoreKey::generate().expect("key"))
            .expect("create the key file and its directory");

        assert!(
            store
                .load(KeyStoreInteraction::NoPrompt)
                .expect("read key file")
                .is_some()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&parent)
                .expect("directory metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700);
        }
    }

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
