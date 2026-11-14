//! SQLite persistence: accounts, devices, pairing codes.
//!
//! Live state (who is online, pending streams, tickets) is in memory in
//! `AppState`; only durable records live here.

use crate::util::{format_code, hash_secret, now, pairing_code, random_id, random_secret};
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use std::sync::Mutex;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS accounts (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    created_at  INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS devices (
    id            TEXT PRIMARY KEY,
    account_id    TEXT NOT NULL REFERENCES accounts(id),
    name          TEXT NOT NULL,
    token_hash    TEXT NOT NULL UNIQUE,
    created_at    INTEGER NOT NULL,
    last_seen_at  INTEGER,
    revoked_at    INTEGER
);
CREATE INDEX IF NOT EXISTS devices_account ON devices(account_id);
CREATE TABLE IF NOT EXISTS pairing_codes (
    code_hash   TEXT PRIMARY KEY,
    account_id  TEXT NOT NULL REFERENCES accounts(id),
    created_by  TEXT NOT NULL REFERENCES devices(id),
    expires_at  INTEGER NOT NULL,
    used_at     INTEGER
);
"#;

/// Columns added after the first release; applied when missing.
fn migrate(conn: &Connection) -> Result<()> {
    if !has_column(conn, "devices", "app")? {
        conn.execute_batch("ALTER TABLE devices ADD COLUMN app TEXT")?;
    }
    if !has_column(conn, "accounts", "external_id")? {
        conn.execute_batch("ALTER TABLE accounts ADD COLUMN external_id TEXT")?;
    }
    conn.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS accounts_external
         ON accounts(external_id) WHERE external_id IS NOT NULL",
    )?;
    Ok(())
}

fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(names.iter().any(|n| n == column))
}

const DEVICE_COLUMNS: &str = "id, account_id, name, created_at, last_seen_at, app";

#[derive(Debug, Clone)]
pub struct DeviceRow {
    pub id: String,
    pub account_id: String,
    pub name: String,
    pub created_at: i64,
    pub last_seen_at: Option<i64>,
    pub app: Option<String>,
}

pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    /// `":memory:"` opens a private in-memory database (tests, demos).
    pub fn open(path: &str) -> Result<Self> {
        let conn = if path == ":memory:" {
            Connection::open_in_memory()?
        } else {
            let conn = Connection::open(path)?;
            conn.pragma_update(None, "journal_mode", "WAL")?;
            conn
        };
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Creates an account and its first device. Returns (account_id, device_id, token).
    pub fn bootstrap(
        &self,
        account_name: &str,
        device_name: &str,
        app: Option<&str>,
    ) -> Result<(String, String, String)> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let account_id = random_id("acc");
        tx.execute(
            "INSERT INTO accounts (id, name, created_at) VALUES (?1, ?2, ?3)",
            params![account_id, account_name, now()],
        )?;
        let (device_id, token) = insert_device(&tx, &account_id, device_name, app)?;
        tx.commit()?;
        Ok((account_id, device_id, token))
    }

    /// Registers a device for the account with `external_id`, creating the
    /// account on first use. Returns (account_id, device_id, token).
    pub fn issue_device(
        &self,
        external_id: &str,
        account_name: &str,
        device_name: &str,
        app: Option<&str>,
    ) -> Result<(String, String, String)> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT id FROM accounts WHERE external_id = ?1",
                params![external_id],
                |r| r.get(0),
            )
            .optional()?;
        let account_id = match existing {
            Some(id) => id,
            None => {
                let id = random_id("acc");
                tx.execute(
                    "INSERT INTO accounts (id, name, created_at, external_id) VALUES (?1, ?2, ?3, ?4)",
                    params![id, account_name, now(), external_id],
                )?;
                id
            }
        };
        let (device_id, token) = insert_device(&tx, &account_id, device_name, app)?;
        tx.commit()?;
        Ok((account_id, device_id, token))
    }

    pub fn account_by_external_id(&self, external_id: &str) -> Result<Option<String>> {
        let id = self
            .conn()
            .query_row(
                "SELECT id FROM accounts WHERE external_id = ?1",
                params![external_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(id)
    }

    /// Returns false if the device does not exist in this account (or is revoked).
    pub fn rename_device(&self, account_id: &str, device_id: &str, name: &str) -> Result<bool> {
        let changed = self.conn().execute(
            "UPDATE devices SET name = ?1
             WHERE id = ?2 AND account_id = ?3 AND revoked_at IS NULL",
            params![name, device_id, account_id],
        )?;
        Ok(changed == 1)
    }

    /// Resolves a bearer token to an active (non-revoked) device.
    pub fn authenticate(&self, token: &str) -> Result<Option<DeviceRow>> {
        let conn = self.conn();
        let row = conn
            .query_row(
                &format!(
                    "SELECT {DEVICE_COLUMNS} FROM devices
                     WHERE token_hash = ?1 AND revoked_at IS NULL"
                ),
                params![hash_secret(token)],
                map_device,
            )
            .optional()?;
        Ok(row)
    }

    pub fn device_active(&self, device_id: &str) -> Result<bool> {
        let conn = self.conn();
        let found: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM devices WHERE id = ?1 AND revoked_at IS NULL",
                params![device_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    pub fn touch(&self, device_id: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE devices SET last_seen_at = ?1 WHERE id = ?2",
            params![now(), device_id],
        )?;
        Ok(())
    }

    /// Active devices of an account, oldest first.
    pub fn list_devices(&self, account_id: &str) -> Result<Vec<DeviceRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {DEVICE_COLUMNS} FROM devices
             WHERE account_id = ?1 AND revoked_at IS NULL ORDER BY created_at, id"
        ))?;
        let rows = stmt
            .query_map(params![account_id], map_device)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Returns (formatted code, expires_at).
    pub fn create_pairing_code(
        &self,
        account_id: &str,
        created_by: &str,
        ttl_secs: i64,
    ) -> Result<(String, i64)> {
        let conn = self.conn();
        let t = now();
        conn.execute(
            "DELETE FROM pairing_codes WHERE expires_at <= ?1 OR used_at IS NOT NULL",
            params![t],
        )?;
        let code = pairing_code();
        let expires_at = t + ttl_secs;
        conn.execute(
            "INSERT INTO pairing_codes (code_hash, account_id, created_by, expires_at) VALUES (?1, ?2, ?3, ?4)",
            params![hash_secret(&code), account_id, created_by, expires_at],
        )?;
        Ok((format_code(&code), expires_at))
    }

    /// Consumes a pairing code and registers a new device.
    /// Returns None if the code is unknown, used, or expired.
    pub fn claim_pairing_code(
        &self,
        normalized_code: &str,
        device_name: &str,
        app: Option<&str>,
    ) -> Result<Option<(String, String, String)>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let account_id: Option<String> = tx
            .query_row(
                "SELECT account_id FROM pairing_codes
                 WHERE code_hash = ?1 AND used_at IS NULL AND expires_at > ?2",
                params![hash_secret(normalized_code), now()],
                |r| r.get(0),
            )
            .optional()?;
        let Some(account_id) = account_id else {
            return Ok(None);
        };
        tx.execute(
            "UPDATE pairing_codes SET used_at = ?1 WHERE code_hash = ?2",
            params![now(), hash_secret(normalized_code)],
        )?;
        let (device_id, token) = insert_device(&tx, &account_id, device_name, app)?;
        tx.commit()?;
        Ok(Some((account_id, device_id, token)))
    }

    /// Returns false if the device does not exist in this account (or is already revoked).
    pub fn revoke_device(&self, account_id: &str, device_id: &str) -> Result<bool> {
        let changed = self.conn().execute(
            "UPDATE devices SET revoked_at = ?1
             WHERE id = ?2 AND account_id = ?3 AND revoked_at IS NULL",
            params![now(), device_id, account_id],
        )?;
        Ok(changed == 1)
    }
}

fn insert_device(
    conn: &Connection,
    account_id: &str,
    name: &str,
    app: Option<&str>,
) -> Result<(String, String)> {
    let device_id = random_id("dev");
    let token = random_secret("nexd");
    conn.execute(
        "INSERT INTO devices (id, account_id, name, token_hash, created_at, app)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![device_id, account_id, name, hash_secret(&token), now(), app],
    )?;
    Ok((device_id, token))
}

fn map_device(r: &rusqlite::Row<'_>) -> rusqlite::Result<DeviceRow> {
    Ok(DeviceRow {
        id: r.get(0)?,
        account_id: r.get(1)?,
        name: r.get(2)?,
        created_at: r.get(3)?,
        last_seen_at: r.get(4)?,
        app: r.get(5)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upgrades_a_first_release_database() {
        let dir = std::env::temp_dir().join(format!("nex-migrate-{}", random_id("t")));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE accounts (id TEXT PRIMARY KEY, name TEXT NOT NULL, created_at INTEGER NOT NULL);
                 CREATE TABLE devices (id TEXT PRIMARY KEY, account_id TEXT NOT NULL REFERENCES accounts(id),
                   name TEXT NOT NULL, token_hash TEXT NOT NULL UNIQUE, created_at INTEGER NOT NULL,
                   last_seen_at INTEGER, revoked_at INTEGER);
                 INSERT INTO accounts VALUES ('acc_old', 'old', 1);
                 INSERT INTO devices (id, account_id, name, token_hash, created_at)
                   VALUES ('dev_old', 'acc_old', 'laptop', 'h', 1);",
            )
            .unwrap();
        }
        let db = Db::open(path.to_str().unwrap()).unwrap();
        let devices = db.list_devices("acc_old").unwrap();
        assert_eq!(
            (devices[0].name.as_str(), devices[0].app.as_deref()),
            ("laptop", None)
        );
        let (acc, _, _) = db.issue_device("ext-1", "n", "d", Some("termany")).unwrap();
        let (again, _, _) = db.issue_device("ext-1", "n", "d2", None).unwrap();
        assert_eq!(acc, again);
        drop(db);
        Db::open(path.to_str().unwrap()).unwrap(); // re-running migrations is a no-op
        std::fs::remove_dir_all(dir).unwrap();
    }
}
