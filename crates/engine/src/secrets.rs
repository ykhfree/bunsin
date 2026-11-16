//! Encrypts the tokens that profile owners enter, with a key that only the server holds.
//!
//! This protects a copied database or backup. It does not protect against someone who
//! can read the server's key file.

use bunsin_store::SealedSecret;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use std::collections::BTreeMap;
use std::io::Write;
#[cfg(test)]
use std::path::Path;

const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 24;

/// A token and the address it was entered for.
#[derive(Clone, PartialEq, Eq)]
pub struct StoredSecret {
    pub value: String,
    /// Scheme, host and port. The token must be sent to no other origin.
    pub origin: String,
}

impl std::fmt::Debug for StoredSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredSecret")
            .field("value", &"[REDACTED]")
            .field("origin", &self.origin)
            .finish()
    }
}

/// The stored secrets of one profile by key. An entry is an error if it cannot be decrypted.
pub type ProfileSecrets = BTreeMap<String, Result<StoredSecret, String>>;

/// The key that encrypts stored tokens. `Debug` never shows it.
#[derive(Clone, PartialEq, Eq)]
pub struct MasterKey([u8; KEY_LEN]);

impl MasterKey {
    /// A new random key.
    pub fn generate() -> Result<Self, String> {
        let mut key = [0u8; KEY_LEN];
        getrandom::fill(&mut key).map_err(|e| format!("no random key can be made: {e}"))?;
        Ok(Self(key))
    }

    /// The key in `bytes`. The error says only how long the bytes are.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        let key: [u8; KEY_LEN] = bytes
            .try_into()
            .map_err(|_| format!("a key has {KEY_LEN} bytes, this has {}", bytes.len()))?;
        Ok(Self(key))
    }

    /// The key as text, for a store that holds text.
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// The key from the text of `to_hex`. The error does not repeat the text.
    pub fn from_hex(text: &str) -> Result<Self, String> {
        let invalid = || "the text is not a key".to_owned();
        if text.len() != KEY_LEN * 2 || !text.is_ascii() {
            return Err(invalid());
        }
        let mut key = [0u8; KEY_LEN];
        for (index, byte) in key.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).map_err(|_| invalid())?;
        }
        Ok(Self(key))
    }
}

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MasterKey([REDACTED])")
    }
}

/// Where the engine gets its master key.
pub trait KeySource {
    /// The key. `may_create` is false if the data directory holds values that an earlier key
    /// encrypted: a new key would make every one of them unreadable, so the source must fail
    /// if it has no key.
    fn key(&self, may_create: bool) -> Result<MasterKey, String>;
}

/// The master key in a file.
pub struct KeyFile(pub std::path::PathBuf);

impl KeyFile {
    /// The key in the file, or None if the file does not exist.
    pub fn read(&self) -> Result<Option<MasterKey>, String> {
        match std::fs::read(&self.0) {
            Ok(bytes) => MasterKey::from_bytes(&bytes)
                .map(Some)
                .map_err(|e| format!("the key file {} cannot be used: {e}", self.0.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("the key file {} cannot be read: {error}", self.0.display())),
        }
    }
}

impl KeySource for KeyFile {
    fn key(&self, may_create: bool) -> Result<MasterKey, String> {
        if let Some(key) = self.read()? {
            return Ok(key);
        }
        if !may_create {
            return Err(format!(
                "this data directory holds encrypted values, but the key file {} does not exist. \
                 A new key could not read them. The key may be in another file (--master-key-file) \
                 or in the Keychain (--key-store keychain)",
                self.0.display()
            ));
        }
        let key = MasterKey::generate()?;
        let mut options = std::fs::OpenOptions::new();
        // `create_new` fails if another process made the file meanwhile, so a key is never replaced.
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        options
            .open(&self.0)
            .and_then(|mut file| file.write_all(&key.0))
            .map_err(|e| format!("the key file {} cannot be written: {e}", self.0.display()))?;
        Ok(key)
    }
}

pub(crate) struct SecretBox {
    cipher: XChaCha20Poly1305,
}

impl SecretBox {
    pub(crate) fn new(key: &MasterKey) -> Self {
        Self {
            cipher: XChaCha20Poly1305::new_from_slice(&key.0).expect("a master key has the length of a cipher key"),
        }
    }

    /// Reads the key file, or creates it with a new random key if it does not exist.
    #[cfg(test)]
    pub(crate) fn load_or_create(path: &Path) -> Result<Self, String> {
        Ok(Self::new(&KeyFile(path.to_owned()).key(true)?))
    }

    /// The data that binds a ciphertext to its place. A secret moved to another profile
    /// or key does not decrypt.
    fn place(profile: &str, key: &str, origin: &str) -> Vec<u8> {
        format!("{}:{profile}\0{}:{key}\0{origin}", profile.len(), key.len()).into_bytes()
    }

    pub(crate) fn seal(&self, profile: &str, key: &str, secret: &StoredSecret) -> Result<SealedSecret, String> {
        let mut nonce = [0u8; NONCE_LEN];
        getrandom::fill(&mut nonce).map_err(|e| e.to_string())?;
        let aad = Self::place(profile, key, &secret.origin);
        let ciphertext = self
            .cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: secret.value.as_bytes(),
                    aad: &aad,
                },
            )
            .map_err(|_| "the token cannot be encrypted".to_owned())?;
        Ok(SealedSecret {
            nonce: nonce.to_vec(),
            ciphertext,
            origin: secret.origin.clone(),
        })
    }

    pub(crate) fn open(&self, profile: &str, key: &str, sealed: &SealedSecret) -> Result<StoredSecret, String> {
        let unreadable = || format!("the stored token `{key}` cannot be read with this server's key; enter it again");
        if sealed.nonce.len() != NONCE_LEN {
            return Err(unreadable());
        }
        let aad = Self::place(profile, key, &sealed.origin);
        let plain = self
            .cipher
            .decrypt(
                XNonce::from_slice(&sealed.nonce),
                Payload {
                    msg: &sealed.ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|_| unreadable())?;
        let value = String::from_utf8(plain).map_err(|_| unreadable())?;
        Ok(StoredSecret {
            value,
            origin: sealed.origin.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret(value: &str) -> StoredSecret {
        StoredSecret {
            value: value.into(),
            origin: "https://mcp.example.com:443".into(),
        }
    }

    fn new_box(dir: &Path) -> SecretBox {
        SecretBox::load_or_create(&dir.join("master.key")).unwrap()
    }

    #[test]
    fn a_sealed_secret_opens_only_in_its_own_place() {
        let dir = tempfile::tempdir().unwrap();
        let b = new_box(dir.path());
        let sealed = b.seal("kim", "MCP_SLACK", &secret("xoxb-token")).unwrap();
        assert_eq!(b.open("kim", "MCP_SLACK", &sealed).unwrap(), secret("xoxb-token"));
        // The same bytes under another profile, another key or another address do not open.
        assert!(b.open("lee", "MCP_SLACK", &sealed).is_err());
        assert!(b.open("kim", "MCP_JIRA", &sealed).is_err());
        let moved = SealedSecret {
            origin: "https://evil.example:443".into(),
            ..sealed.clone()
        };
        assert!(b.open("kim", "MCP_SLACK", &moved).is_err());
        let mut changed = sealed.clone();
        changed.ciphertext[0] ^= 1;
        assert!(b.open("kim", "MCP_SLACK", &changed).is_err());
    }

    #[test]
    fn the_ciphertext_does_not_hold_the_token_and_differs_each_time() {
        let dir = tempfile::tempdir().unwrap();
        let b = new_box(dir.path());
        let first = b.seal("kim", "LLM", &secret("sk-very-secret-token")).unwrap();
        let second = b.seal("kim", "LLM", &secret("sk-very-secret-token")).unwrap();
        assert_ne!(first.nonce, second.nonce);
        assert_ne!(first.ciphertext, second.ciphertext);
        let needle = b"sk-very-secret-token";
        assert!(!first.ciphertext.windows(needle.len()).any(|window| window == needle));
    }

    #[test]
    fn the_key_file_is_made_once_and_then_reused() {
        let dir = tempfile::tempdir().unwrap();
        let sealed = new_box(dir.path()).seal("kim", "LLM", &secret("t")).unwrap();
        // A second server start reads the same key.
        assert_eq!(new_box(dir.path()).open("kim", "LLM", &sealed).unwrap().value, "t");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("master.key"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // Another key cannot read the secret, and the error does not show it.
        let other = tempfile::tempdir().unwrap();
        let error = new_box(other.path()).open("kim", "LLM", &sealed).unwrap_err();
        assert!(error.contains("enter it again"), "{error}");
    }

    #[test]
    fn a_damaged_key_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("master.key"), b"too short").unwrap();
        assert!(SecretBox::load_or_create(&dir.path().join("master.key")).is_err());
    }

    #[test]
    fn debug_output_hides_the_value() {
        let shown = format!("{:?}", secret("xoxb-token"));
        assert!(
            !shown.contains("xoxb-token") && shown.contains("mcp.example.com"),
            "{shown}"
        );
    }
}
