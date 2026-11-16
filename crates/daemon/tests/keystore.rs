//! The server's master key in a keychain item, with a keychain in memory.
//! No test reads or writes the real Keychain.

use axum::http::{HeaderMap, StatusCode};
use bunsin_api::{ConnectorUpdate, EngineApi, Expected, ProfileConfig, TokenChange};
use bunsin_daemon::keystore::{KEY_FILE, KEYCHAIN_ID_FILE, KeyStore, KeychainItem, KeychainKey, key_store};
use bunsin_daemon::{RealBackends, UpstreamPolicy};
use bunsin_engine::{Engine, KeySource, MasterKey, OpenError};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// What the keychain in memory holds and what was done with it.
#[derive(Default)]
struct Keychain {
    /// By account name.
    items: std::collections::BTreeMap<String, String>,
    gets: usize,
    sets: usize,
    locked: bool,
    refuses_writes: bool,
    /// Accepts a write and keeps nothing.
    loses_writes: bool,
    /// For each write: whether the key file existed at that moment.
    file_at_write: Vec<bool>,
}

struct Item {
    keychain: Arc<Mutex<Keychain>>,
    account: String,
    key_file: PathBuf,
}

impl KeychainItem for Item {
    fn get(&self) -> Result<Option<String>, String> {
        let mut keychain = self.keychain.lock().unwrap();
        keychain.gets += 1;
        if keychain.locked {
            return Err("the Keychain is locked".to_owned());
        }
        Ok(keychain.items.get(&self.account).cloned())
    }

    fn set(&self, value: &str) -> Result<(), String> {
        let mut keychain = self.keychain.lock().unwrap();
        keychain.sets += 1;
        keychain.file_at_write.push(self.key_file.exists());
        if keychain.refuses_writes {
            return Err("the Keychain refused the write".to_owned());
        }
        if !keychain.loses_writes {
            keychain.items.insert(self.account.clone(), value.to_owned());
        }
        Ok(())
    }
}

/// Makes the item of an account.
type MakeItem = Box<dyn Fn(&str) -> Box<dyn KeychainItem>>;
type Source = KeychainKey<MakeItem>;

/// The key source of `dir` on the keychain in memory.
fn source(dir: &Path, keychain: &Arc<Mutex<Keychain>>) -> Source {
    let (keychain, key_file) = (keychain.clone(), dir.join(KEY_FILE));
    let item: MakeItem = Box::new(move |account| {
        Box::new(Item {
            keychain: keychain.clone(),
            account: account.to_owned(),
            key_file: key_file.clone(),
        })
    });
    KeychainKey::new(dir, item)
}

fn key_file(dir: &Path, byte: u8) -> MasterKey {
    std::fs::write(dir.join(KEY_FILE), [byte; 32]).unwrap();
    MasterKey::from_bytes(&[byte; 32]).unwrap()
}

/// The one item of the keychain, as a key.
fn stored(keychain: &Arc<Mutex<Keychain>>) -> Option<MasterKey> {
    let keychain = keychain.lock().unwrap();
    assert!(keychain.items.len() <= 1, "more than one item");
    keychain
        .items
        .values()
        .next()
        .map(|text| MasterKey::from_hex(text).unwrap())
}

#[test]
fn the_store_follows_the_system_and_the_options() {
    let dir = Path::new("/var/lib/bunsin");
    let file = |name: &str| KeyStore::File(dir.join(name));
    let choose = |os, requested, key_file: Option<&str>| key_store(os, requested, key_file.map(PathBuf::from), dir);
    // The default: the Keychain on a Mac, a file elsewhere.
    assert_eq!(choose("macos", None, None), Ok(KeyStore::Keychain));
    assert_eq!(choose("linux", None, None), Ok(file("master.key")));
    // A Mac that starts without a login session can use the file.
    assert_eq!(choose("macos", Some("file"), None), Ok(file("master.key")));
    assert_eq!(choose("macos", Some("keychain"), None), Ok(KeyStore::Keychain));
    // A named key file means the file store on every system.
    assert_eq!(
        choose("macos", None, Some("/keys/b.key")),
        Ok(KeyStore::File("/keys/b.key".into()))
    );
    assert_eq!(
        choose("linux", Some("file"), Some("/keys/b.key")),
        Ok(KeyStore::File("/keys/b.key".into()))
    );
    // What cannot work is refused, never replaced by another store.
    assert!(
        choose("linux", Some("keychain"), None)
            .unwrap_err()
            .contains("macOS only")
    );
    assert!(
        choose("macos", Some("keychain"), Some("/keys/b.key"))
            .unwrap_err()
            .contains("--master-key-file")
    );
    assert!(
        choose("macos", Some("vault"), None)
            .unwrap_err()
            .contains("not a key store")
    );
}

#[test]
fn a_new_folder_gets_a_key_in_the_keychain_and_no_key_file() {
    let dir = tempfile::tempdir().unwrap();
    let keychain = Arc::new(Mutex::new(Keychain::default()));
    let key = source(dir.path(), &keychain).key(true).unwrap();
    assert_eq!(stored(&keychain), Some(key.clone()));
    assert!(!dir.path().join(KEY_FILE).exists());
    // The next start reads the same key and writes nothing.
    let again = source(dir.path(), &keychain).key(false).unwrap();
    assert_eq!(again, key);
    assert_eq!(keychain.lock().unwrap().sets, 1);
    // Another spelling of the folder finds the same item: the account is an ID in the folder.
    let other_spelling = dir.path().join(".").join("..").join(dir.path().file_name().unwrap());
    assert_eq!(source(&other_spelling, &keychain).key(false).unwrap(), key);
    let account = std::fs::read_to_string(dir.path().join(KEYCHAIN_ID_FILE)).unwrap();
    assert_eq!(keychain.lock().unwrap().items.keys().collect::<Vec<_>>(), [&account]);
    // Another folder has its own item.
    let second = tempfile::tempdir().unwrap();
    let other = source(second.path(), &keychain).key(true).unwrap();
    assert_ne!(other, key);
    assert_eq!(keychain.lock().unwrap().items.len(), 2);
}

#[test]
fn a_key_file_moves_into_the_keychain_and_goes_only_after_the_read_back() {
    let dir = tempfile::tempdir().unwrap();
    let old = key_file(dir.path(), 7);
    let keychain = Arc::new(Mutex::new(Keychain::default()));
    // The folder holds encrypted values, so no new key may be made.
    let key = source(dir.path(), &keychain).key(false).unwrap();
    assert_eq!(key, old);
    assert_eq!(stored(&keychain), Some(old));
    assert!(!dir.path().join(KEY_FILE).exists());
    // The file was still there when the item was written.
    assert_eq!(keychain.lock().unwrap().file_at_write, [true]);
}

#[test]
fn a_move_that_stopped_halfway_is_finished() {
    let dir = tempfile::tempdir().unwrap();
    let old = key_file(dir.path(), 7);
    let keychain = Arc::new(Mutex::new(Keychain::default()));
    source(dir.path(), &keychain).key(false).unwrap();
    // The item was written, and the file was not removed: the earlier start stopped there.
    key_file(dir.path(), 7);
    assert_eq!(source(dir.path(), &keychain).key(false).unwrap(), old);
    assert!(!dir.path().join(KEY_FILE).exists());
    assert_eq!(keychain.lock().unwrap().sets, 1);
}

#[test]
fn nothing_is_changed_when_the_keychain_cannot_be_trusted_with_the_key() {
    // (what is wrong with the keychain, a part of the error)
    type Setup = fn(&mut Keychain, &str);
    let cases: [(&str, Setup, &str); 5] = [
        ("locked", |k, _| k.locked = true, "locked"),
        ("refuses the write", |k, _| k.refuses_writes = true, "refused the write"),
        (
            "loses the write",
            |k, _| k.loses_writes = true,
            "did not give back the key",
        ),
        (
            "holds another key",
            |k, account| drop(k.items.insert(account.to_owned(), "ab".repeat(32))),
            "hold different keys",
        ),
        (
            "holds something that is no key",
            |k, account| drop(k.items.insert(account.to_owned(), "a note of the user".to_owned())),
            "does not hold a key",
        ),
    ];
    for (what, setup, reason) in cases {
        let dir = tempfile::tempdir().unwrap();
        key_file(dir.path(), 7);
        let keychain = Arc::new(Mutex::new(Keychain::default()));
        // The account of this folder exists before the keychain is set up.
        let account = "0123456789abcdef0123456789abcdef";
        std::fs::write(dir.path().join(KEYCHAIN_ID_FILE), account).unwrap();
        setup(&mut keychain.lock().unwrap(), account);
        let before = keychain.lock().unwrap().items.clone();

        let error = source(dir.path(), &keychain).key(false).unwrap_err();
        assert!(error.contains(reason), "{what}: {error}");
        // The key file is as it was, and so is the keychain.
        assert_eq!(std::fs::read(dir.path().join(KEY_FILE)).unwrap(), [7u8; 32], "{what}");
        assert_eq!(keychain.lock().unwrap().items, before, "{what}");
        // No error holds a key.
        assert!(
            !error.contains(&"07".repeat(32)) && !error.contains(&"ab".repeat(32)),
            "{what}: {error}"
        );
    }
}

#[test]
fn no_new_key_is_made_over_encrypted_values() {
    let dir = tempfile::tempdir().unwrap();
    let keychain = Arc::new(Mutex::new(Keychain::default()));
    let error = source(dir.path(), &keychain).key(false).unwrap_err();
    assert!(
        error.contains("holds encrypted values") && error.contains("--key-store file"),
        "{error}"
    );
    assert_eq!(keychain.lock().unwrap().sets, 0);
    assert!(!dir.path().join(KEY_FILE).exists());
}

const TOKEN: &str = "xoxb-sealed-before-the-move-4e1a";

/// A server that records the `Authorization` header of every request and answers 401.
async fn recorder() -> (String, Arc<Mutex<Vec<Option<String>>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = {
        let seen = seen.clone();
        move |headers: HeaderMap| {
            let seen = seen.clone();
            async move {
                let auth = headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                seen.lock().unwrap().push(auth);
                StatusCode::UNAUTHORIZED
            }
        }
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, axum::Router::new().fallback(record)).await });
    (url, seen)
}

fn backends() -> Arc<RealBackends> {
    let allowed = UpstreamPolicy {
        allowed_hosts: vec!["127.0.0.1".into()],
    };
    Arc::new(RealBackends::new(Arc::new(|_| None), false, allowed))
}

#[tokio::test]
async fn a_token_that_was_sealed_before_the_move_is_still_used_after_it() {
    let dir = tempfile::tempdir().unwrap();
    let (url, seen) = recorder().await;
    let sent_token = |seen: &Arc<Mutex<Vec<Option<String>>>>| {
        let expected = format!("Bearer {TOKEN}");
        seen.lock()
            .unwrap()
            .iter()
            .filter(|auth| auth.as_deref() == Some(expected.as_str()))
            .count()
    };
    {
        // An earlier version: the key is in a file.
        let engine = Engine::open(dir.path(), backends(), Arc::new(|| 1_000)).unwrap();
        engine.create_profile("kim", ProfileConfig::new("Kim")).await.unwrap();
        let update = ConnectorUpdate {
            url: url.clone(),
            token: TokenChange::Set(TOKEN.into()),
            token_header: None,
            expected: Expected::Unchecked,
        };
        engine.set_connector("kim", "wiki", update).await.unwrap();
        let _ = engine.connector_tools("kim", "wiki").await;
        assert!(sent_token(&seen) > 0);
    }
    assert!(dir.path().join(KEY_FILE).exists());
    let before = sent_token(&seen);

    // The same folder starts with the keychain store.
    let keychain = Arc::new(Mutex::new(Keychain::default()));
    let engine = Engine::open_with_key(
        dir.path(),
        &source(dir.path(), &keychain),
        backends(),
        Arc::new(|| 1_000),
    )
    .unwrap();
    assert!(!dir.path().join(KEY_FILE).exists());
    assert!(stored(&keychain).is_some());
    // The token decrypts with the moved key and goes to its server again.
    let _ = engine.connector_tools("kim", "wiki").await;
    assert!(sent_token(&seen) > before, "{:?}", seen.lock().unwrap());
    drop(engine);

    // Without its key the folder does not start, with either store. No new key is made.
    keychain.lock().unwrap().items.clear();
    for store in ["keychain", "file"] {
        let opened = match store {
            "keychain" => Engine::open_with_key(
                dir.path(),
                &source(dir.path(), &keychain),
                backends(),
                Arc::new(|| 1_000),
            ),
            _ => Engine::open(dir.path(), backends(), Arc::new(|| 1_000)),
        };
        let error = opened.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(error.contains("holds encrypted values"), "{store}: {error}");
    }
    assert!(keychain.lock().unwrap().items.is_empty());
    assert!(!dir.path().join(KEY_FILE).exists());
}

#[tokio::test]
async fn a_second_instance_does_not_touch_the_key() {
    let dir = tempfile::tempdir().unwrap();
    let keychain = Arc::new(Mutex::new(Keychain::default()));
    let first = Engine::open_with_key(
        dir.path(),
        &source(dir.path(), &keychain),
        backends(),
        Arc::new(|| 1_000),
    )
    .unwrap();
    let (gets, sets) = {
        let keychain = keychain.lock().unwrap();
        (keychain.gets, keychain.sets)
    };
    // A key file appears, as if someone put an old one back. The second start must not move it.
    key_file(dir.path(), 9);
    let second = Engine::open_with_key(
        dir.path(),
        &source(dir.path(), &keychain),
        backends(),
        Arc::new(|| 1_000),
    );
    assert!(matches!(second, Err(OpenError::AlreadyRunning)));
    let keychain = keychain.lock().unwrap();
    assert_eq!((keychain.gets, keychain.sets), (gets, sets));
    assert!(dir.path().join(KEY_FILE).exists());
    drop(first);
}
