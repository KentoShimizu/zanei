//! Store encryption key and on-disk format detection.

use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use zeroize::Zeroizing;

use super::StoreError;

/// Length of a raw SQLCipher key in bytes (AES-256).
pub const STORE_KEY_BYTES: usize = 32;
const STORE_KEY_HEX_CHARS: usize = STORE_KEY_BYTES * 2;
const SQLITE_HEADER: &[u8; 16] = b"SQLite format 3\0";
/// SQLCipher writes whole pages of this size, so a ciphertext store is always a
/// non-empty multiple of it; anything else with a foreign header is damage.
const SQLCIPHER_PAGE_SIZE: u64 = 4096;

/// A 256-bit raw SQLCipher key.
///
/// The bytes are zeroed when the key is dropped and never appear in `Debug` output.
#[derive(Clone)]
pub struct StoreKey(Zeroizing<[u8; STORE_KEY_BYTES]>);

impl StoreKey {
    /// Generates a fresh random key from the operating system's CSPRNG.
    pub fn generate() -> Result<Self, StoreError> {
        let mut bytes = Zeroizing::new([0_u8; STORE_KEY_BYTES]);
        getrandom::fill(bytes.as_mut_slice())
            .map_err(|error| StoreError::KeyGeneration(error.to_string()))?;
        Ok(Self(bytes))
    }

    #[must_use]
    pub fn from_bytes(bytes: [u8; STORE_KEY_BYTES]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// Parses the 64-character hexadecimal form used in the Keychain and in key files.
    pub fn from_hex(text: &str) -> Result<Self, StoreError> {
        let text = text.trim();
        if text.len() != STORE_KEY_HEX_CHARS {
            return Err(StoreError::InvalidKey("expected 64 hexadecimal characters"));
        }
        let mut bytes = Zeroizing::new([0_u8; STORE_KEY_BYTES]);
        for (index, pair) in text.as_bytes().chunks_exact(2).enumerate() {
            let high = hex_value(pair[0]);
            let low = hex_value(pair[1]);
            let (Some(high), Some(low)) = (high, low) else {
                return Err(StoreError::InvalidKey("expected 64 hexadecimal characters"));
            };
            bytes[index] = (high << 4) | low;
        }
        Ok(Self(bytes))
    }

    /// Lowercase hexadecimal form. The returned string is zeroed on drop.
    #[must_use]
    pub fn to_hex(&self) -> Zeroizing<String> {
        let mut text = String::with_capacity(STORE_KEY_HEX_CHARS);
        for byte in self.0.iter() {
            text.push(HEX_DIGITS[usize::from(byte >> 4)] as char);
            text.push(HEX_DIGITS[usize::from(byte & 0x0f)] as char);
        }
        Zeroizing::new(text)
    }

    /// The `x'…'` literal SQLCipher accepts as a raw key (no key derivation).
    pub(super) fn sqlcipher_literal(&self) -> Zeroizing<String> {
        Zeroizing::new(format!("x'{}'", self.to_hex().as_str()))
    }
}

impl fmt::Debug for StoreKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("StoreKey(<redacted>)")
    }
}

const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// On-disk format of a store file, decided by its first 16 bytes and its size.
///
/// Readers never consult configuration: a plaintext header is a store written before
/// encryption existed, a page-aligned file with any other header is a SQLCipher
/// database (which randomizes the header), and everything else is damaged.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreFormat {
    /// The file does not exist or is empty.
    Missing,
    /// A plain SQLite database.
    Plaintext,
    /// A SQLCipher database; a key is required to read it.
    Encrypted,
    /// Neither a SQLite header nor a whole number of SQLCipher pages: the file
    /// is truncated or overwritten. Opening it reports corruption.
    Unrecognized,
}

impl StoreFormat {
    /// Reads the header of the file at `path`.
    ///
    /// This opens and closes the file outside SQLite. POSIX advisory locks are
    /// per process, not per descriptor: closing this descriptor releases every
    /// lock the process holds on the file, including the one a SQLite
    /// connection in WAL mode keeps for its whole lifetime. Probe before
    /// opening a connection, never while one is open in the same process; a
    /// caller that already holds one passes its known format to
    /// `StoreReader::open_known` / `StoreWriter::open_known` instead.
    pub fn probe(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref();
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::Missing),
            Err(error) => return Err(StoreError::io("open the store", error)),
        };
        let length = file
            .metadata()
            .map_err(|error| StoreError::io("read the store metadata", error))?
            .len();
        let mut header = Vec::with_capacity(SQLITE_HEADER.len());
        file.by_ref()
            .take(SQLITE_HEADER.len() as u64)
            .read_to_end(&mut header)
            .map_err(|error| StoreError::io("read the store header", error))?;
        Ok(if header.is_empty() {
            Self::Missing
        } else if header.as_slice() == SQLITE_HEADER {
            Self::Plaintext
        } else if length % SQLCIPHER_PAGE_SIZE == 0 {
            Self::Encrypted
        } else {
            Self::Unrecognized
        })
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Plaintext => "plaintext",
            Self::Encrypted => "sqlcipher",
            Self::Unrecognized => "unrecognized",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{STORE_KEY_BYTES, StoreKey};

    #[test]
    fn hex_round_trips_and_rejects_malformed_input() {
        let key = StoreKey::generate().expect("generate key");
        let hex = key.to_hex();
        assert_eq!(hex.len(), STORE_KEY_BYTES * 2);
        let parsed = StoreKey::from_hex(&format!("  {}\n", hex.to_uppercase())).expect("parse hex");
        assert_eq!(parsed.to_hex().as_str(), hex.as_str());
        assert_eq!(format!("{key:?}"), "StoreKey(<redacted>)");
        assert_eq!(
            key.sqlcipher_literal().as_str(),
            format!("x'{}'", hex.as_str())
        );

        assert!(StoreKey::from_hex("").is_err());
        assert!(StoreKey::from_hex(&hex[..62]).is_err());
        assert!(StoreKey::from_hex(&format!("{}zz", &hex[..62])).is_err());
        assert!(StoreKey::from_hex(&format!("{}é", &hex[..62])).is_err());
    }
}
