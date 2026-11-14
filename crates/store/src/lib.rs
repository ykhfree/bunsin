//! On-disk state. Each profile has its own database file and artifact folder,
//! so one profile cannot read another profile's data through a missed filter.

mod profile;

pub use profile::ProfileStore;

use bunsin_core::records::ProfileInfo;
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

/// The data directory of one Bunsin installation.
pub struct AppStore {
    root: PathBuf,
    db: Mutex<Connection>,
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
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS profiles (
                id TEXT PRIMARY KEY,
                display_name TEXT NOT NULL,
                created_at INTEGER NOT NULL
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
        let mut statement =
            db.prepare("SELECT id, display_name FROM profiles ORDER BY created_at, id")?;
        let rows = statement.query_map([], |row| {
            Ok(ProfileInfo {
                id: row.get(0)?,
                display_name: row.get(1)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Opens the private store of an existing profile.
    pub fn open_profile(&self, id: &str) -> Result<ProfileStore> {
        if !valid_profile_id(id) {
            return Err(StoreError::BadProfileId(id.to_owned()));
        }
        let exists: bool = self.db.lock().expect("store lock").query_row(
            "SELECT EXISTS (SELECT 1 FROM profiles WHERE id = ?1)",
            [id],
            |row| row.get(0),
        )?;
        if !exists {
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
        let names: Vec<String> = app
            .list_profiles()
            .unwrap()
            .into_iter()
            .map(|p| p.id)
            .collect();
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
                matches!(
                    app.create_profile(id, "x", 1),
                    Err(StoreError::BadProfileId(_))
                ),
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
        assert!(matches!(
            app.open_profile("lee"),
            Err(StoreError::NoSuchProfile(_))
        ));
        // No directory was created for a rejected id.
        assert_eq!(
            std::fs::read_dir(dir.path().join("profiles"))
                .unwrap()
                .count(),
            0
        );
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
        let artifact = kim
            .put_artifact("draft", b"kim draft", 5, None, None)
            .unwrap();
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
}
