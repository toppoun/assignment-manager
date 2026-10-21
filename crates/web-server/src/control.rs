//! Web control persistence, independent from assignment schema v3.
use crate::{
    WebError,
    poc::{PocDatabase, reject_links},
};
use rusqlite::Connection;
use std::{
    fs::{File, OpenOptions},
    sync::{Arc, Mutex},
};

pub(crate) struct ControlDb {
    pub connection: Connection,
    _lock: File,
}
impl ControlDb {
    pub fn open(database: &PocDatabase) -> Result<Self, WebError> {
        let directory = database.path().parent().ok_or(WebError::Internal)?;
        let path = directory.join("web-control.db");
        let lock_path = directory.join("web-control.lock");
        reject_links(&path)?;
        reject_links(&lock_path)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
            .map_err(|_| WebError::Internal)?;
        lock.try_lock().map_err(|_| WebError::Unavailable)?;
        let mut connection = Connection::open(path).map_err(|_| WebError::Internal)?;
        connection
            .busy_timeout(std::time::Duration::from_secs(2))
            .map_err(|_| WebError::Internal)?;
        connection
            .execute_batch(
                "PRAGMA foreign_keys=ON; PRAGMA secure_delete=ON; PRAGMA synchronous=FULL;",
            )
            .map_err(|_| WebError::Internal)?;
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(|_| WebError::Internal)?;
        if version == 0 {
            let count: i64 = connection
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
                    [],
                    |r| r.get(0),
                )
                .map_err(|_| WebError::Internal)?;
            if count != 0 {
                return Err(WebError::Internal);
            }
            let tx = connection.transaction().map_err(|_| WebError::Internal)?;
            tx.execute_batch("CREATE TABLE accounts (singleton INTEGER PRIMARY KEY CHECK(singleton=1), user_id TEXT NOT NULL UNIQUE, login TEXT NOT NULL UNIQUE, verifier TEXT NOT NULL, status TEXT NOT NULL CHECK(status IN ('enabled','disabled')));
                CREATE TABLE sessions (token_hash BLOB PRIMARY KEY, user_id TEXT NOT NULL REFERENCES accounts(user_id), csrf TEXT NOT NULL, created INTEGER NOT NULL, last_seen INTEGER NOT NULL, absolute_expiry INTEGER NOT NULL);
                CREATE TABLE login_attempts (at INTEGER NOT NULL);
                PRAGMA user_version=1;").map_err(|_| WebError::Internal)?;
            tx.commit().map_err(|_| WebError::Internal)?;
        } else if version != 1 && version != 2 {
            return Err(WebError::Internal);
        }
        let integrity: String = connection
            .query_row("PRAGMA quick_check(1)", [], |r| r.get(0))
            .map_err(|_| WebError::Internal)?;
        if integrity != "ok"
            || connection
                .prepare("PRAGMA foreign_key_check")
                .map_err(|_| WebError::Internal)?
                .exists([])
                .map_err(|_| WebError::Internal)?
        {
            return Err(WebError::Internal);
        }
        migrate_v2(&mut connection)?;
        Ok(Self {
            connection,
            _lock: lock,
        })
    }
}

fn migrate_v2(connection: &mut Connection) -> Result<(), WebError> {
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .map_err(|_| WebError::Internal)?;
    if version == 1 {
        let tx = connection.transaction().map_err(|_| WebError::Internal)?;
        tx.execute_batch("ALTER TABLE sessions ADD COLUMN reauthenticated_at INTEGER;
            CREATE TABLE vault_binding (singleton INTEGER PRIMARY KEY CHECK(singleton=1), owner TEXT NOT NULL REFERENCES accounts(user_id), envelope BLOB NOT NULL);
            CREATE TABLE web_credentials (owner TEXT NOT NULL REFERENCES accounts(user_id), kind TEXT NOT NULL CHECK(kind IN ('moodle','webclass')), envelope BLOB NOT NULL, PRIMARY KEY(owner,kind));
            CREATE TABLE vault_nonces (key_id BLOB NOT NULL CHECK(length(key_id)=16), nonce BLOB NOT NULL CHECK(length(nonce)=12), PRIMARY KEY(key_id,nonce));
            PRAGMA user_version=2;").map_err(|_| WebError::Internal)?;
        validate_schema(&tx)?;
        tx.commit().map_err(|_| WebError::Internal)?;
    }
    validate_schema(connection)
}

fn validate_schema(connection: &Connection) -> Result<(), WebError> {
    // A claimed version with missing/partial tables is never recreated/reset.
    connection.prepare("SELECT a.singleton,a.user_id,a.login,a.verifier,a.status,
        s.token_hash,s.user_id,s.csrf,s.created,s.last_seen,s.absolute_expiry,s.reauthenticated_at,
        b.singleton,b.owner,b.envelope,c.owner,c.kind,c.envelope,n.key_id,n.nonce,l.at
        FROM accounts a LEFT JOIN sessions s ON s.user_id=a.user_id
        LEFT JOIN vault_binding b ON b.owner=a.user_id LEFT JOIN web_credentials c ON c.owner=a.user_id
        LEFT JOIN vault_nonces n ON 0 LEFT JOIN login_attempts l ON 0 LIMIT 0").map_err(|_| WebError::Internal)?;
    Ok(())
}

#[derive(Clone)]
pub(crate) struct SharedControl(Arc<Mutex<Option<ControlDb>>>);
impl SharedControl {
    pub fn new(db: ControlDb) -> Self {
        Self(Arc::new(Mutex::new(Some(db))))
    }
    pub fn with<T>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T, WebError>,
    ) -> Result<T, WebError> {
        let mut guard = self.0.lock().map_err(|_| WebError::Unavailable)?;
        f(&mut guard.as_mut().ok_or(WebError::Unavailable)?.connection)
    }
    pub fn close(&self) {
        if let Ok(mut guard) = self.0.lock() {
            guard.take();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn v1() -> (tempfile::TempDir, PocDatabase) {
        let root = tempfile::tempdir().unwrap();
        let db = PocDatabase::prepare(&root.path().join("poc")).unwrap();
        crate::auth::bootstrap_admin(
            &db,
            "owner",
            "synthetic-control-password-3b".to_owned().into(),
        )
        .unwrap();
        let conn = Connection::open(crate::auth::control_path(&db)).unwrap();
        conn.execute_batch("INSERT INTO sessions (token_hash,user_id,csrf,created,last_seen,absolute_expiry) SELECT x'0001',user_id,'synthetic-csrf',1,1,999 FROM accounts;
            INSERT INTO login_attempts VALUES (42);
            DROP TABLE web_credentials; DROP TABLE vault_binding; DROP TABLE vault_nonces;
            ALTER TABLE sessions DROP COLUMN reauthenticated_at; PRAGMA user_version=1;").unwrap();
        (root, db)
    }
    #[test]
    fn migrates_v1_without_losing_accounts_sessions_or_rate_limit_and_keeps_assignments_v3() {
        let (_root, db) = v1();
        let control = ControlDb::open(&db).unwrap();
        for (table, count) in [
            ("accounts", 1),
            ("sessions", 1),
            ("login_attempts", 1),
            ("web_credentials", 0),
            ("vault_nonces", 0),
        ] {
            assert_eq!(
                control
                    .connection
                    .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r
                        .get::<_, i64>(0))
                    .unwrap(),
                count
            );
        }
        assert!(
            control
                .connection
                .query_row("SELECT reauthenticated_at FROM sessions", [], |r| r
                    .get::<_, Option<i64>>(0))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            control
                .connection
                .query_row("SELECT at FROM login_attempts", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            42
        );
        assert_eq!(
            control
                .connection
                .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            Connection::open(db.path())
                .unwrap()
                .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            3
        );
    }
    #[test]
    fn migration_failure_rolls_back_and_future_or_partial_schema_is_never_reset() {
        let (_root, db) = v1();
        let path = crate::auth::control_path(&db);
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE web_credentials (foreign_data TEXT); INSERT INTO web_credentials VALUES ('keep')").unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(ControlDb::open(&db).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(
            conn.prepare("SELECT reauthenticated_at FROM sessions")
                .is_err()
        );
        assert_eq!(
            conn.query_row("SELECT foreign_data FROM web_credentials", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "keep"
        );
        conn.execute_batch("PRAGMA user_version=99").unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(ControlDb::open(&db).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        conn.execute_batch("DROP TABLE web_credentials; PRAGMA user_version=2")
            .unwrap();
        assert!(ControlDb::open(&db).is_err());
    }
}
