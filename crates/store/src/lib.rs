//! On-disk state. Each profile has its own database file and artifact folder,
//! so one profile cannot read another profile's data through a missed filter.

mod profile;

pub use profile::{EventStart, IntakeMessage, IntakeRow, LlmCall, ProfileStore, SealedSecret, WikiWrite};

use bunsin_core::records::{Caller, CallerToken, ProfileInfo};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("file error: {0}")]
    Io(#[from] std::io::Error),
    #[error("stored JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("profile id `{0}` is invalid: use 1 to 40 of a-z, 0-9 and `-`")]
    BadProfileId(String),
    #[error("profile `{0}` already exists")]
    ProfileExists(String),
    #[error("profile `{0}` does not exist")]
    NoSuchProfile(String),
    #[error("{0} {1} does not exist")]
    NotFound(&'static str, i64),
    #[error("artifact {id} is damaged: its content does not match its recorded hash")]
    ArtifactDamaged { id: i64 },
    #[error("the stored value `{0}` is not a known state")]
    BadState(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

pub(crate) const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// An access token as the operator lists it. The token value is not stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenInfo {
    /// The first characters of the token hash. Enough to name a token for removal.
    pub id: String,
    pub profile: String,
    pub label: String,
    pub created_at: i64,
}

const TOKEN_PREFIX: &str = "bsn_";
/// Another prefix than access tokens, so a person can tell the two kinds apart.
const CALLER_TOKEN_PREFIX: &str = "bsc_";
const TOKEN_ID_LEN: usize = 12;

/// The data directory of one Bunsin installation.
pub struct AppStore {
    root: PathBuf,
    db: Mutex<Connection>,
}

/// A new token: the prefix and 32 random bytes from the OS as hex.
fn new_token(prefix: &str) -> Result<String> {
    let mut random = [0u8; 32];
    getrandom::fill(&mut random).map_err(|e| StoreError::Io(std::io::Error::other(e.to_string())))?;
    let secret: String = random.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("{prefix}{secret}"))
}

fn valid_profile_id(id: &str) -> bool {
    (1..=40).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

impl AppStore {
    pub fn open(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root.join("profiles"))?;
        let db = Connection::open(root.join("app.db"))?;
        // The server and operator commands are separate processes on the same files.
        db.busy_timeout(BUSY_TIMEOUT)?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS profiles (
                id TEXT PRIMARY KEY,
                display_name TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS tokens (
                hash TEXT PRIMARY KEY,
                profile TEXT NOT NULL,
                label TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS caller_tokens (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                hash TEXT NOT NULL UNIQUE,
                profile TEXT NOT NULL,
                label TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                revoked_at INTEGER
            );",
        )?;
        Ok(Self {
            root: root.to_owned(),
            db: Mutex::new(db),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn create_profile(&self, id: &str, display_name: &str, now: i64) -> Result<ProfileInfo> {
        // The id becomes a directory name, so it is checked before any path is built.
        if !valid_profile_id(id) {
            return Err(StoreError::BadProfileId(id.to_owned()));
        }
        let db = self.db.lock().expect("store lock");
        let inserted = db.execute(
            "INSERT OR IGNORE INTO profiles (id, display_name, created_at) VALUES (?1, ?2, ?3)",
            (id, display_name, now),
        )?;
        if inserted == 0 {
            return Err(StoreError::ProfileExists(id.to_owned()));
        }
        Ok(ProfileInfo {
            id: id.to_owned(),
            display_name: display_name.to_owned(),
        })
    }

    pub fn list_profiles(&self) -> Result<Vec<ProfileInfo>> {
        let db = self.db.lock().expect("store lock");
        let mut statement = db.prepare("SELECT id, display_name FROM profiles ORDER BY created_at, id")?;
        let rows = statement.query_map([], |row| {
            Ok(ProfileInfo {
                id: row.get(0)?,
                display_name: row.get(1)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn profile_exists(&self, id: &str) -> Result<bool> {
        Ok(self.db.lock().expect("store lock").query_row(
            "SELECT EXISTS (SELECT 1 FROM profiles WHERE id = ?1)",
            [id],
            |row| row.get(0),
        )?)
    }

    /// Creates an access token for one profile and returns it. Only its hash is stored,
    /// so this is the only time the token can be read.
    pub fn add_token(&self, profile: &str, label: &str, now: i64) -> Result<String> {
        if !self.profile_exists(profile)? {
            return Err(StoreError::NoSuchProfile(profile.to_owned()));
        }
        let token = new_token(TOKEN_PREFIX)?;
        self.db.lock().expect("store lock").execute(
            "INSERT INTO tokens (hash, profile, label, created_at) VALUES (?1, ?2, ?3, ?4)",
            (bunsin_core::sha256_hex(token.as_bytes()), profile, label, now),
        )?;
        Ok(token)
    }

    /// The profile a token gives access to. `None` for an unknown or revoked token.
    pub fn token_profile(&self, token: &str) -> Result<Option<String>> {
        use rusqlite::OptionalExtension;
        Ok(self
            .db
            .lock()
            .expect("store lock")
            .query_row(
                "SELECT profile FROM tokens WHERE hash = ?1",
                [bunsin_core::sha256_hex(token.as_bytes())],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn list_tokens(&self) -> Result<Vec<TokenInfo>> {
        let db = self.db.lock().expect("store lock");
        let mut statement =
            db.prepare("SELECT hash, profile, label, created_at FROM tokens ORDER BY created_at, hash")?;
        let rows = statement.query_map([], |row| {
            let hash: String = row.get(0)?;
            Ok(TokenInfo {
                id: hash[..TOKEN_ID_LEN].to_owned(),
                profile: row.get(1)?,
                label: row.get(2)?,
                created_at: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Removes the tokens whose ID starts with `id`. Returns how many were removed.
    pub fn revoke_token(&self, id: &str) -> Result<usize> {
        // A short prefix could match many tokens, so the full listed ID is required.
        if id.len() < TOKEN_ID_LEN || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(0);
        }
        Ok(self
            .db
            .lock()
            .expect("store lock")
            .execute("DELETE FROM tokens WHERE hash LIKE ?1 || '%'", [id])?)
    }

    /// Creates a caller token for one profile and returns its ID and the token. Only the hash of
    /// the token is stored, so this is the only time the token can be read.
    pub fn add_caller_token(&self, profile: &str, label: &str, now: i64) -> Result<(i64, String)> {
        if !self.profile_exists(profile)? {
            return Err(StoreError::NoSuchProfile(profile.to_owned()));
        }
        let token = new_token(CALLER_TOKEN_PREFIX)?;
        let db = self.db.lock().expect("store lock");
        db.execute(
            "INSERT INTO caller_tokens (hash, profile, label, created_at) VALUES (?1, ?2, ?3, ?4)",
            (bunsin_core::sha256_hex(token.as_bytes()), profile, label, now),
        )?;
        Ok((db.last_insert_rowid(), token))
    }

    /// The caller behind a token. `None` for an unknown or revoked token. The table is read
    /// on every request, so a revocation works from the next request on.
    pub fn caller(&self, token: &str) -> Result<Option<Caller>> {
        use rusqlite::OptionalExtension;
        Ok(self
            .db
            .lock()
            .expect("store lock")
            .query_row(
                "SELECT id, profile, label FROM caller_tokens WHERE hash = ?1 AND revoked_at IS NULL",
                [bunsin_core::sha256_hex(token.as_bytes())],
                |row| {
                    Ok(Caller {
                        id: row.get(0)?,
                        profile: row.get(1)?,
                        label: row.get(2)?,
                    })
                },
            )
            .optional()?)
    }

    /// The caller tokens of one profile, revoked ones too, oldest first.
    pub fn caller_tokens(&self, profile: &str) -> Result<Vec<CallerToken>> {
        let db = self.db.lock().expect("store lock");
        let mut statement =
            db.prepare("SELECT id, label, created_at, revoked_at FROM caller_tokens WHERE profile = ?1 ORDER BY id")?;
        let rows = statement.query_map([profile], |row| {
            Ok(CallerToken {
                id: row.get(0)?,
                label: row.get(1)?,
                created_at: row.get(2)?,
                revoked_at: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Revokes a caller token of `profile`. Returns false if the profile has no such token
    /// that is still valid, so a token of another profile is never touched.
    pub fn revoke_caller_token(&self, profile: &str, id: i64, now: i64) -> Result<bool> {
        let changed = self.db.lock().expect("store lock").execute(
            "UPDATE caller_tokens SET revoked_at = ?1 WHERE id = ?2 AND profile = ?3 AND revoked_at IS NULL",
            (now, id, profile),
        )?;
        Ok(changed == 1)
    }

    /// Opens the private store of an existing profile.
    pub fn open_profile(&self, id: &str) -> Result<ProfileStore> {
        if !valid_profile_id(id) {
            return Err(StoreError::BadProfileId(id.to_owned()));
        }
        if !self.profile_exists(id)? {
            return Err(StoreError::NoSuchProfile(id.to_owned()));
        }
        ProfileStore::open(&self.root.join("profiles").join(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_are_listed_in_creation_order() {
        let dir = tempfile::tempdir().unwrap();
        let app = AppStore::open(dir.path()).unwrap();
        app.create_profile("kim", "Kim", 10).unwrap();
        app.create_profile("lee", "Lee", 20).unwrap();
        let names: Vec<String> = app.list_profiles().unwrap().into_iter().map(|p| p.id).collect();
        assert_eq!(names, ["kim", "lee"]);
    }

    #[test]
    fn profiles_survive_reopening() {
        let dir = tempfile::tempdir().unwrap();
        AppStore::open(dir.path())
            .unwrap()
            .create_profile("kim", "Kim", 10)
            .unwrap();
        let reopened = AppStore::open(dir.path()).unwrap();
        assert_eq!(
            reopened.list_profiles().unwrap(),
            [ProfileInfo {
                id: "kim".into(),
                display_name: "Kim".into()
            }]
        );
    }

    #[test]
    fn bad_and_duplicate_ids_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let app = AppStore::open(dir.path()).unwrap();
        let long = "a".repeat(41);
        for id in ["", "../kim", "Kim", "a b", "a/b", long.as_str()] {
            assert!(
                matches!(app.create_profile(id, "x", 1), Err(StoreError::BadProfileId(_))),
                "{id:?}"
            );
            assert!(
                matches!(app.open_profile(id), Err(StoreError::BadProfileId(_))),
                "{id:?}"
            );
        }
        app.create_profile("kim", "Kim", 1).unwrap();
        assert!(matches!(
            app.create_profile("kim", "Other", 2),
            Err(StoreError::ProfileExists(_))
        ));
        assert!(matches!(app.open_profile("lee"), Err(StoreError::NoSuchProfile(_))));
        // No directory was created for a rejected id.
        assert_eq!(std::fs::read_dir(dir.path().join("profiles")).unwrap().count(), 0);
    }

    #[test]
    fn one_profile_cannot_see_another_profiles_rows_or_files() {
        let dir = tempfile::tempdir().unwrap();
        let app = AppStore::open(dir.path()).unwrap();
        app.create_profile("kim", "Kim", 1).unwrap();
        app.create_profile("lee", "Lee", 1).unwrap();
        let kim = app.open_profile("kim").unwrap();
        let lee = app.open_profile("lee").unwrap();

        kim.append_audit(5, "note", None, &serde_json::json!({"secret": "kim only"}))
            .unwrap();
        let artifact = kim.put_artifact("draft", b"kim draft", 5, None, None).unwrap();
        kim.set_kill_switch(true).unwrap();

        assert!(lee.list_audit(10).unwrap().is_empty());
        assert!(matches!(
            lee.read_artifact(artifact.id),
            Err(StoreError::NotFound("artifact", _))
        ));
        assert!(!lee.kill_switch().unwrap());
        assert!(
            !dir.path()
                .join("profiles/lee/artifacts/sha256")
                .join(&artifact.sha256)
                .exists()
        );
    }

    #[test]
    fn a_token_gives_access_to_its_profile_only_until_it_is_revoked() {
        let dir = tempfile::tempdir().unwrap();
        let app = AppStore::open(dir.path()).unwrap();
        app.create_profile("kim", "Kim", 1).unwrap();
        app.create_profile("lee", "Lee", 1).unwrap();
        let kim = app.add_token("kim", "laptop", 10).unwrap();
        let lee = app.add_token("lee", "laptop", 11).unwrap();
        assert_ne!(kim, lee);
        assert!(kim.starts_with("bsn_") && kim.len() == 4 + 64, "{}", kim.len());
        assert_eq!(app.token_profile(&kim).unwrap().as_deref(), Some("kim"));
        assert_eq!(app.token_profile(&lee).unwrap().as_deref(), Some("lee"));
        assert_eq!(app.token_profile("bsn_guess").unwrap(), None);
        assert_eq!(app.token_profile("").unwrap(), None);

        let listed = app.list_tokens().unwrap();
        assert_eq!(
            listed.iter().map(|t| t.profile.as_str()).collect::<Vec<_>>(),
            ["kim", "lee"]
        );
        assert_eq!(app.revoke_token(&listed[0].id).unwrap(), 1);
        assert_eq!(app.token_profile(&kim).unwrap(), None);
        assert_eq!(app.token_profile(&lee).unwrap().as_deref(), Some("lee"));
    }

    #[test]
    fn the_token_value_is_not_stored() {
        let dir = tempfile::tempdir().unwrap();
        let app = AppStore::open(dir.path()).unwrap();
        app.create_profile("kim", "Kim", 1).unwrap();
        let token = app.add_token("kim", "laptop", 10).unwrap();
        drop(app);
        let bytes = std::fs::read(dir.path().join("app.db")).unwrap();
        let secret = &token.as_bytes()[4..];
        assert!(!bytes.windows(secret.len()).any(|window| window == secret));
    }

    #[test]
    fn tokens_need_an_existing_profile_and_a_full_id_to_revoke() {
        let dir = tempfile::tempdir().unwrap();
        let app = AppStore::open(dir.path()).unwrap();
        assert!(matches!(
            app.add_token("nobody", "x", 1),
            Err(StoreError::NoSuchProfile(_))
        ));
        app.create_profile("kim", "Kim", 1).unwrap();
        let a = app.add_token("kim", "a", 1).unwrap();
        let b = app.add_token("kim", "b", 2).unwrap();
        // Neither an empty ID nor a wildcard removes anything.
        for id in ["", "%", "0", "zzzzzzzzzzzz"] {
            assert_eq!(app.revoke_token(id).unwrap(), 0, "{id:?}");
        }
        assert!(app.token_profile(&a).unwrap().is_some() && app.token_profile(&b).unwrap().is_some());
    }

    #[test]
    fn a_caller_token_names_its_caller_until_it_is_revoked() {
        let dir = tempfile::tempdir().unwrap();
        let app = AppStore::open(dir.path()).unwrap();
        app.create_profile("kim", "Kim", 1).unwrap();
        app.create_profile("lee", "Lee", 1).unwrap();
        assert!(matches!(
            app.add_caller_token("nobody", "x", 1),
            Err(StoreError::NoSuchProfile(_))
        ));
        let (bot, token) = app.add_caller_token("kim", "Lee's bot", 10).unwrap();
        let (other, _) = app.add_caller_token("lee", "Kim's bot", 11).unwrap();
        assert_ne!(bot, other);
        assert!(token.starts_with("bsc_") && token.len() == 4 + 64);
        let caller = app.caller(&token).unwrap().unwrap();
        assert_eq!(
            (caller.id, caller.profile.as_str(), caller.label.as_str()),
            (bot, "kim", "Lee's bot")
        );
        // The two kinds of token never stand in for each other.
        assert_eq!(app.token_profile(&token).unwrap(), None);
        let access = app.add_token("kim", "laptop", 12).unwrap();
        assert_eq!(app.caller(&access).unwrap(), None);
        assert_eq!(app.caller("bsc_guess").unwrap(), None);

        // An owner revokes only a token of the own profile.
        assert!(!app.revoke_caller_token("lee", bot, 20).unwrap());
        assert!(app.caller(&token).unwrap().is_some());
        assert!(app.revoke_caller_token("kim", bot, 20).unwrap());
        assert_eq!(app.caller(&token).unwrap(), None);
        assert!(!app.revoke_caller_token("kim", bot, 21).unwrap());
        let listed = app.caller_tokens("kim").unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!((listed[0].id, listed[0].revoked_at), (bot, Some(20)));
        assert_eq!(app.caller_tokens("lee").unwrap()[0].revoked_at, None);
        // Only the hash is stored.
        drop(app);
        let bytes = std::fs::read(dir.path().join("app.db")).unwrap();
        let secret = &token.as_bytes()[4..];
        assert!(!bytes.windows(secret.len()).any(|window| window == secret));
    }

    #[test]
    fn a_second_process_sees_changes_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let server = AppStore::open(dir.path()).unwrap();
        let operator = AppStore::open(dir.path()).unwrap();
        operator.create_profile("kim", "Kim", 1).unwrap();
        let token = operator.add_token("kim", "laptop", 2).unwrap();
        assert_eq!(server.token_profile(&token).unwrap().as_deref(), Some("kim"));
        let id = operator.list_tokens().unwrap().remove(0).id;
        operator.revoke_token(&id).unwrap();
        assert_eq!(server.token_profile(&token).unwrap(), None);
    }
}
