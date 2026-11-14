//! The saved server connection and the access token store.
//!
//! The server address and the CA path go to a JSON file. The token goes to a
//! `TokenStore` only, never to the file.

use bunsin_api::{ApiError, ApiResult};
use bunsin_client::{RemoteConfig, RemoteEngine};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const FILE_NAME: &str = "connection.json";

/// Where the server is. This type has no token field, so the file cannot hold one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Connection {
    pub server: String,
    /// A PEM file to trust instead of the system roots.
    pub ca_cert: Option<PathBuf>,
}

/// Reads the saved connection. A missing or unreadable file means "no saved connection".
pub fn load_connection(dir: &Path) -> Option<Connection> {
    let text = std::fs::read_to_string(dir.join(FILE_NAME)).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn save_connection(dir: &Path, connection: &Connection) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let text = serde_json::to_string_pretty(connection).expect("a connection always serializes");
    std::fs::write(dir.join(FILE_NAME), text)
}

/// Holds the one access token of the app. Errors are short texts for the error line.
pub trait TokenStore {
    fn get(&self) -> Result<Option<String>, String>;
    fn set(&self, token: &str) -> Result<(), String>;
    /// Succeeds when no token is stored.
    fn delete(&self) -> Result<(), String>;
}

/// The macOS Keychain, through the `keyring` crate.
pub struct KeychainTokenStore;

impl KeychainTokenStore {
    const SERVICE: &str = "Bunsin";
    const ACCOUNT: &str = "access-token";

    fn entry() -> Result<keyring::Entry, String> {
        keyring::Entry::new(Self::SERVICE, Self::ACCOUNT)
            .map_err(|e| format!("the Keychain cannot be used: {e}"))
    }
}

impl TokenStore for KeychainTokenStore {
    fn get(&self) -> Result<Option<String>, String> {
        match Self::entry()?.get_password() {
            Ok(token) => Ok(Some(token)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(format!("the token cannot be read from the Keychain: {e}")),
        }
    }

    fn set(&self, token: &str) -> Result<(), String> {
        Self::entry()?
            .set_password(token)
            .map_err(|e| format!("the token cannot be saved in the Keychain: {e}"))
    }

    fn delete(&self) -> Result<(), String> {
        match Self::entry()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(format!(
                "the token cannot be deleted from the Keychain: {e}"
            )),
        }
    }
}

/// A token store in memory, for tests.
#[derive(Default)]
pub struct MemoryTokenStore(Mutex<Option<String>>);

impl TokenStore for MemoryTokenStore {
    fn get(&self) -> Result<Option<String>, String> {
        Ok(self.0.lock().expect("token lock").clone())
    }

    fn set(&self, token: &str) -> Result<(), String> {
        *self.0.lock().expect("token lock") = Some(token.to_owned());
        Ok(())
    }

    fn delete(&self) -> Result<(), String> {
        *self.0.lock().expect("token lock") = None;
        Ok(())
    }
}

/// Builds the client. It checks the address and the token form, and sends no request.
pub fn client(connection: &Connection, token: &str) -> ApiResult<RemoteEngine> {
    let ca_cert_pem = match &connection.ca_cert {
        Some(path) => Some(std::fs::read(path).map_err(|e| {
            ApiError::Invalid(format!(
                "the CA certificate {} cannot be read: {e}",
                path.display()
            ))
        })?),
        None => None,
    };
    let config = RemoteConfig {
        url: connection.server.clone(),
        ca_cert_pem,
    };
    RemoteEngine::new(&config, token)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Connection {
        Connection {
            server: "https://bunsin.example.com".into(),
            ca_cert: Some(PathBuf::from("/etc/bunsin/ca.pem")),
        }
    }

    #[test]
    fn a_saved_connection_reads_back_the_same() {
        let dir = tempfile::tempdir().unwrap();
        // The directory does not exist yet. Saving creates it.
        let dir = dir.path().join("Bunsin");
        save_connection(&dir, &sample()).unwrap();
        assert_eq!(load_connection(&dir), Some(sample()));

        let without_ca = Connection {
            ca_cert: None,
            ..sample()
        };
        save_connection(&dir, &without_ca).unwrap();
        assert_eq!(load_connection(&dir), Some(without_ca));
    }

    #[test]
    fn a_missing_file_is_no_saved_connection() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_connection(dir.path()), None);
        assert_eq!(load_connection(&dir.path().join("absent")), None);
    }

    #[test]
    fn a_corrupt_file_is_no_saved_connection() {
        let dir = tempfile::tempdir().unwrap();
        for text in ["", "{", "[]", r#"{"ca_cert": null}"#, "\u{0}\u{1}"] {
            std::fs::write(dir.path().join(FILE_NAME), text).unwrap();
            assert_eq!(load_connection(dir.path()), None, "{text:?}");
        }
    }

    #[test]
    fn the_memory_store_keeps_one_token_until_it_is_deleted() {
        let store = MemoryTokenStore::default();
        assert_eq!(store.get(), Ok(None));
        store.set("bsn_one").unwrap();
        store.set("bsn_two").unwrap();
        assert_eq!(store.get(), Ok(Some("bsn_two".into())));
        store.delete().unwrap();
        assert_eq!(store.get(), Ok(None));
        // Deleting again is not an error.
        assert_eq!(store.delete(), Ok(()));
    }

    #[test]
    fn an_unreadable_ca_file_is_an_invalid_address_error() {
        let connection = Connection {
            server: "https://bunsin.example.com".into(),
            ca_cert: Some(PathBuf::from("/nonexistent/ca.pem")),
        };
        let error = client(&connection, "bsn_x").unwrap_err();
        assert!(matches!(error, ApiError::Invalid(_)), "{error:?}");
        assert!(error.to_string().contains("/nonexistent/ca.pem"), "{error}");
    }
}
