//! Where the server keeps the key that encrypts stored tokens: a file, or the Keychain of macOS.
//!
//! The stored tokens stay encrypted in the database in both cases. With the Keychain, a copied
//! data directory holds no key, and macOS guards the key with the login of the user.

use bunsin_engine::{KeyFile, KeySource, MasterKey};
use std::path::{Path, PathBuf};

/// The name of the Keychain items of Bunsin servers. Keychain Access shows it as "Where".
pub const KEYCHAIN_SERVICE: &str = "Bunsin server";
/// The file in the data directory that holds the account name of the server's Keychain item.
pub const KEYCHAIN_ID_FILE: &str = "keychain-id";
/// The key file of a data directory that an earlier version or the file store made.
pub const KEY_FILE: &str = "master.key";

/// Where the key is kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyStore {
    File(PathBuf),
    Keychain,
}

/// The store for a server on the operating system `os` (as in `std::env::consts::OS`).
/// `requested` is the `--key-store` option and `key_file` is the `--master-key-file` option.
pub fn key_store(
    os: &str,
    requested: Option<&str>,
    key_file: Option<PathBuf>,
    data_dir: &Path,
) -> Result<KeyStore, String> {
    let default_file = || data_dir.join(KEY_FILE);
    match (requested, key_file) {
        (Some("keychain"), Some(_)) => {
            Err("--master-key-file names a key file, so it cannot go with --key-store keychain".to_owned())
        }
        // A named key file means the file store, on every system.
        (Some("file") | None, Some(file)) => Ok(KeyStore::File(file)),
        (Some("file"), None) => Ok(KeyStore::File(default_file())),
        (Some("keychain"), None) if os == "macos" => Ok(KeyStore::Keychain),
        (Some("keychain"), None) => Err("--key-store keychain works on macOS only".to_owned()),
        (None, None) if os == "macos" => Ok(KeyStore::Keychain),
        (None, None) => Ok(KeyStore::File(default_file())),
        (Some(other), _) => Err(format!("`{other}` is not a key store: use `keychain` or `file`")),
    }
}

/// One item of a keychain that holds a text. An error is a short text without any part of the value.
pub trait KeychainItem {
    fn get(&self) -> Result<Option<String>, String>;
    fn set(&self, value: &str) -> Result<(), String>;
}

/// The master key in a keychain item. A key file that an earlier start made is moved into the item.
pub struct KeychainKey<F> {
    data_dir: PathBuf,
    /// Makes the item for an account name. The real server gives the Keychain of macOS.
    item: F,
}

impl<F: Fn(&str) -> Box<dyn KeychainItem>> KeychainKey<F> {
    pub fn new(data_dir: &Path, item: F) -> Self {
        Self {
            data_dir: data_dir.to_owned(),
            item,
        }
    }

    /// The account name of this data directory's item. It is a random ID in a file of the
    /// directory, not the path: a path has more than one spelling, and a directory can move.
    fn account(&self) -> Result<String, String> {
        let path = self.data_dir.join(KEYCHAIN_ID_FILE);
        match std::fs::read_to_string(&path) {
            Ok(id) if valid_id(id.trim()) => Ok(id.trim().to_owned()),
            Ok(_) => Err(format!(
                "the file {} does not hold an ID of a Keychain item",
                path.display()
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut bytes = [0u8; 16];
                getrandom::fill(&mut bytes).map_err(|e| format!("no random ID can be made: {e}"))?;
                let id: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
                std::fs::write(&path, &id)
                    .map_err(|e| format!("the file {} cannot be written: {e}", path.display()))?;
                Ok(id)
            }
            Err(error) => Err(format!("the file {} cannot be read: {error}", path.display())),
        }
    }
}

fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

impl<F: Fn(&str) -> Box<dyn KeychainItem>> KeySource for KeychainKey<F> {
    fn key(&self, may_create: bool) -> Result<MasterKey, String> {
        let account = self.account()?;
        let item = (self.item)(&account);
        let file = KeyFile(self.data_dir.join(KEY_FILE));
        let in_file = file.read()?;
        let in_item = match item.get()? {
            // An item that is no key is never replaced: it may be the only copy of something.
            Some(text) => Some(MasterKey::from_hex(&text).map_err(|_| {
                format!(
                    "the Keychain item `{KEYCHAIN_SERVICE}` with the account {account} does not hold a key. \\
                     Nothing was changed"
                )
            })?),
            None => None,
        };
        match (in_item, in_file) {
            (Some(key), None) => Ok(key),
            // An earlier move stopped after it wrote the item. The file is the same key.
            (Some(key), Some(old)) if key == old => {
                remove(&file.0)?;
                Ok(key)
            }
            (Some(_), Some(_)) => Err(format!(
                "the Keychain item and the key file {} hold different keys. Nothing was changed. \\
                 Start with --key-store file to use the file, or move the file away to use the Keychain",
                file.0.display()
            )),
            (None, Some(key)) => {
                store(item.as_ref(), &key)?;
                // The file goes only after the item was read back with the same key.
                remove(&file.0)?;
                eprintln!(
                    "moved the key from {} into the Keychain item `{KEYCHAIN_SERVICE}`, account {account}",
                    file.0.display()
                );
                Ok(key)
            }
            (None, None) if may_create => {
                let key = MasterKey::generate()?;
                store(item.as_ref(), &key)?;
                Ok(key)
            }
            (None, None) => Err(format!(
                "this data directory holds encrypted values, but the Keychain has no item \\
                 `{KEYCHAIN_SERVICE}` with the account {account}. A new key could not read them. \\
                 The key may be in a key file: start with --key-store file or --master-key-file"
            )),
        }
    }
}

/// Writes the key into the item and reads it back. An item that does not give the key back is no store.
fn store(item: &dyn KeychainItem, key: &MasterKey) -> Result<(), String> {
    item.set(&key.to_hex())?;
    match item.get()?.map(|text| MasterKey::from_hex(&text)) {
        Some(Ok(back)) if back == *key => Ok(()),
        _ => Err("the Keychain did not give back the key that was written into it. The key file stays".to_owned()),
    }
}

fn remove(file: &Path) -> Result<(), String> {
    std::fs::remove_file(file).map_err(|e| {
        format!(
            "the key is in the Keychain, but the key file {} cannot be removed: {e}",
            file.display()
        )
    })
}

/// An item of the login Keychain of macOS.
#[cfg(target_os = "macos")]
pub struct MacKeychainItem {
    account: String,
}

#[cfg(target_os = "macos")]
impl MacKeychainItem {
    pub fn boxed(account: &str) -> Box<dyn KeychainItem> {
        Box::new(Self {
            account: account.to_owned(),
        })
    }

    /// Why the Keychain did not do what was asked. Some errors of the crate hold the bytes of
    /// the stored value, which is the key here, so no error is printed as it is.
    fn error(error: &keyring::Error) -> String {
        let reason = match error {
            keyring::Error::NoStorageAccess(_) => "the Keychain is locked or did not give access",
            keyring::Error::BadEncoding(_) | keyring::Error::BadDataFormat(..) => "the item does not hold text",
            keyring::Error::Ambiguous(_) => "more than one item has this name",
            _ => "the Keychain could not do it",
        };
        format!(
            "{reason}. Start the server in a login session of this Mac, or use --key-store file to keep the key in a file"
        )
    }

    fn entry(&self) -> Result<keyring::Entry, String> {
        keyring::Entry::new(KEYCHAIN_SERVICE, &self.account).map_err(|e| Self::error(&e))
    }
}

#[cfg(target_os = "macos")]
impl KeychainItem for MacKeychainItem {
    fn get(&self) -> Result<Option<String>, String> {
        match self.entry()?.get_password() {
            Ok(text) => Ok(Some(text)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(Self::error(&error)),
        }
    }

    fn set(&self, value: &str) -> Result<(), String> {
        self.entry()?.set_password(value).map_err(|e| Self::error(&e))
    }
}
