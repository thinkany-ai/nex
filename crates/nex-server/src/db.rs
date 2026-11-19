//! Postgres persistence.
//!
//! Tenancy: console users own apps; an app (one agent product) owns
//! accounts (its end users); an account owns devices and pairing codes.
//! Every query a device or an app key can reach is scoped through that
//! chain, so apps never see each other's accounts or devices.
//!
//! Relay coordination queries live in the cluster module.

use crate::util::{format_code, hash_secret, now, pairing_code, random_id, random_secret};
use anyhow::{Context, Result};
use sqlx::{
    postgres::{PgConnectOptions, PgPoolOptions},
    Executor, FromRow, PgPool, Postgres, Transaction,
};
use std::str::FromStr;

/// The built-in app that `NEX_ADMIN_KEY` and open bootstrap (development)
/// use. It has no owner; console administrators see it.
pub const DEFAULT_APP_ID: &str = "app_default";

/// Active keys an app may hold at once: enough to rotate without downtime.
pub const MAX_ACTIVE_KEYS: i64 = 2;

pub const ROLE_ADMIN: &str = "admin";
pub const ROLE_MEMBER: &str = "member";

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

const DEVICE_COLUMNS: &str =
    "d.id, d.account_id, a.app_id, d.name, d.created_at, d.last_seen_at, d.app";

#[derive(Debug, Clone, FromRow)]
pub struct DeviceRow {
    pub id: String,
    pub account_id: String,
    pub app_id: String,
    pub name: String,
    pub created_at: i64,
    pub last_seen_at: Option<i64>,
    /// The device's free-form client label (e.g. "termany").
    pub app: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
pub struct UserRow {
    pub id: String,
    pub email: String,
    pub password_hash: String,
    pub role: String,
    pub created_at: i64,
}

impl UserRow {
    pub fn is_admin(&self) -> bool {
        self.role == ROLE_ADMIN
    }
}

#[derive(Debug, Clone, FromRow)]
pub struct UserSummary {
    pub id: String,
    pub email: String,
    pub role: String,
    pub created_at: i64,
    pub apps: i64,
}

#[derive(Debug, Clone, FromRow)]
pub struct AppRow {
    pub id: String,
    pub owner_id: Option<String>,
    pub name: String,
    pub disabled: bool,
    pub created_at: i64,
}

#[derive(Debug, Clone, FromRow)]
pub struct AppSummary {
    pub id: String,
    pub owner_id: Option<String>,
    pub owner_email: Option<String>,
    pub name: String,
    pub disabled: bool,
    pub created_at: i64,
    pub accounts: i64,
    pub devices: i64,
}

#[derive(Debug, Clone, FromRow)]
pub struct KeyRow {
    pub id: String,
    pub app_id: String,
    pub name: String,
    pub key_prefix: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub revoked_at: Option<i64>,
}

#[derive(Debug, Clone, FromRow)]
pub struct AccountRow {
    pub id: String,
    pub name: String,
    pub external_id: Option<String>,
    pub created_at: i64,
}

/// Why a key could not be created.
#[derive(Debug, PartialEq, Eq)]
pub enum KeyError {
    NoSuchApp,
    TooMany,
}

#[derive(Clone)]
pub struct Db {
    pub(crate) pool: PgPool,
}

impl Db {
    /// Connects and applies pending migrations. With `schema`, everything
    /// lives in that schema (created if missing) instead of `public`; tests
    /// use it to get a private database each.
    pub async fn connect(url: &str, schema: Option<&str>) -> Result<Self> {
        let opts = PgConnectOptions::from_str(url).context("invalid database URL")?;
        let mut pool = PgPoolOptions::new().max_connections(10);
        if let Some(schema) = schema {
            if !schema
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
                || schema.is_empty()
            {
                anyhow::bail!("schema must be letters, digits and _");
            }
            // CREATE SCHEMA IF NOT EXISTS still races on pg_namespace if
            // multiple replicas start against the same fresh schema.
            use sqlx::Connection;
            let mut conn = sqlx::PgConnection::connect_with(&opts).await?;
            let mut tx = conn.begin().await?;
            sqlx::query("SELECT pg_advisory_xact_lock(hashtext('nex schema'), hashtext($1))")
                .bind(schema)
                .execute(&mut *tx)
                .await?;
            sqlx::query(&format!("CREATE SCHEMA IF NOT EXISTS {schema}"))
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            let set = format!("SET search_path TO {schema}");
            pool = pool.after_connect(move |conn, _| {
                let set = set.clone();
                Box::pin(async move {
                    conn.execute(set.as_str()).await?;
                    Ok(())
                })
            });
        }
        let pool = pool
            .connect_with(opts)
            .await
            .context("connecting to Postgres")?;
        MIGRATOR.run(&pool).await.context("applying migrations")?;
        Ok(Self { pool })
    }

    /// A private database for one test: a fresh schema in the database at
    /// `NEX_TEST_DATABASE_URL` (default postgres://localhost:5432/nex_test).
    pub async fn connect_for_test() -> Result<Self> {
        let url = std::env::var("NEX_TEST_DATABASE_URL")
            .unwrap_or_else(|_| "postgres://localhost:5432/nex_test".into());
        let schema = random_id("t");
        Self::connect(&url, Some(&schema)).await
    }

    // ---------- devices (relay API) ----------

    /// Resolves a bearer token to an active device of an enabled app.
    pub async fn authenticate(&self, token: &str) -> Result<Option<DeviceRow>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {DEVICE_COLUMNS} FROM devices d
             JOIN accounts a ON a.id = d.account_id
             JOIN apps p ON p.id = a.app_id
             WHERE d.token_hash = $1 AND d.revoked_at IS NULL AND NOT p.disabled"
        ))
        .bind(hash_secret(token))
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn device_active(&self, device_id: &str) -> Result<bool> {
        let found: Option<i32> = sqlx::query_scalar(
            "SELECT 1 FROM devices d
             JOIN accounts a ON a.id = d.account_id
             JOIN apps p ON p.id = a.app_id
             WHERE d.id = $1 AND d.revoked_at IS NULL AND NOT p.disabled",
        )
        .bind(device_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(found.is_some())
    }

    pub async fn touch(&self, device_id: &str) -> Result<()> {
        sqlx::query("UPDATE devices SET last_seen_at = $1 WHERE id = $2")
            .bind(now())
            .bind(device_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Active devices of an account, oldest first.
    pub async fn list_devices(&self, account_id: &str) -> Result<Vec<DeviceRow>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {DEVICE_COLUMNS} FROM devices d JOIN accounts a ON a.id = d.account_id
             WHERE d.account_id = $1 AND d.revoked_at IS NULL ORDER BY d.created_at, d.id"
        ))
        .bind(account_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Returns false if the device does not exist in this account (or is revoked).
    pub async fn rename_device(&self, account_id: &str, device_id: &str, name: &str) -> Result<bool> {
        let r = sqlx::query(
            "UPDATE devices SET name = $1
             WHERE id = $2 AND account_id = $3 AND revoked_at IS NULL",
        )
        .bind(name)
        .bind(device_id)
        .bind(account_id)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() == 1)
    }

    /// Returns false if the device does not exist in this account (or is already revoked).
    pub async fn revoke_device(&self, account_id: &str, device_id: &str) -> Result<bool> {
        let r = sqlx::query(
            "UPDATE devices SET revoked_at = $1
             WHERE id = $2 AND account_id = $3 AND revoked_at IS NULL",
        )
        .bind(now())
        .bind(device_id)
        .bind(account_id)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() == 1)
    }

    /// Creates an account in `app_id` and its first device.
    /// Returns (account_id, device_id, token).
    pub async fn bootstrap(
        &self,
        app_id: &str,
        account_name: &str,
        device_name: &str,
        label: Option<&str>,
    ) -> Result<(String, String, String)> {
        let mut tx = self.pool.begin().await?;
        let account_id = random_id("acc");
        sqlx::query("INSERT INTO accounts (id, app_id, name, created_at) VALUES ($1, $2, $3, $4)")
            .bind(&account_id)
            .bind(app_id)
            .bind(account_name)
            .bind(now())
            .execute(&mut *tx)
            .await?;
        let (device_id, token) = insert_device(&mut tx, &account_id, device_name, label).await?;
        tx.commit().await?;
        Ok((account_id, device_id, token))
    }

    /// Registers a device for the app's account with `external_id`, creating
    /// the account on first use. Returns (account_id, device_id, token).
    pub async fn issue_device(
        &self,
        app_id: &str,
        external_id: &str,
        account_name: &str,
        device_name: &str,
        label: Option<&str>,
    ) -> Result<(String, String, String)> {
        let mut tx = self.pool.begin().await?;
        // Upsert so two concurrent first calls for one user share an account.
        let account_id: String = sqlx::query_scalar(
            "INSERT INTO accounts (id, app_id, name, external_id, created_at)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (app_id, external_id) WHERE external_id IS NOT NULL
             DO UPDATE SET external_id = EXCLUDED.external_id
             RETURNING id",
        )
        .bind(random_id("acc"))
        .bind(app_id)
        .bind(account_name)
        .bind(external_id)
        .bind(now())
        .fetch_one(&mut *tx)
        .await?;
        let (device_id, token) = insert_device(&mut tx, &account_id, device_name, label).await?;
        tx.commit().await?;
        Ok((account_id, device_id, token))
    }

    pub async fn account_by_external_id(&self, app_id: &str, external_id: &str) -> Result<Option<String>> {
        Ok(
            sqlx::query_scalar("SELECT id FROM accounts WHERE app_id = $1 AND external_id = $2")
                .bind(app_id)
                .bind(external_id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// Returns (formatted code, expires_at).
    pub async fn create_pairing_code(
        &self,
        account_id: &str,
        created_by: &str,
        ttl_secs: i64,
    ) -> Result<(String, i64)> {
        let t = now();
        sqlx::query("DELETE FROM pairing_codes WHERE expires_at <= $1 OR used_at IS NOT NULL")
            .bind(t)
            .execute(&self.pool)
            .await?;
        let code = pairing_code();
        let expires_at = t + ttl_secs;
        sqlx::query(
            "INSERT INTO pairing_codes (code_hash, account_id, created_by, expires_at)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(hash_secret(&code))
        .bind(account_id)
        .bind(created_by)
        .bind(expires_at)
        .execute(&self.pool)
        .await?;
        Ok((format_code(&code), expires_at))
    }

    /// Consumes a pairing code and registers a new device in its account.
    /// Returns None if the code is unknown, used, expired, or its app is disabled.
    pub async fn claim_pairing_code(
        &self,
        normalized_code: &str,
        device_name: &str,
        label: Option<&str>,
    ) -> Result<Option<(String, String, String)>> {
        let mut tx = self.pool.begin().await?;
        // The UPDATE claims the code atomically: of two racing claims, one wins.
        let account_id: Option<String> = sqlx::query_scalar(
            "UPDATE pairing_codes c SET used_at = $2
             FROM accounts a JOIN apps p ON p.id = a.app_id
             WHERE c.code_hash = $1 AND c.used_at IS NULL AND c.expires_at > $2
               AND a.id = c.account_id AND NOT p.disabled
             RETURNING c.account_id",
        )
        .bind(hash_secret(normalized_code))
        .bind(now())
        .fetch_optional(&mut *tx)
        .await?;
        let Some(account_id) = account_id else {
            return Ok(None);
        };
        let (device_id, token) = insert_device(&mut tx, &account_id, device_name, label).await?;
        tx.commit().await?;
        Ok(Some((account_id, device_id, token)))
    }

    // ---------- app keys ----------

    /// Resolves an app key to its enabled app.
    pub async fn app_by_key(&self, key: &str) -> Result<Option<AppRow>> {
        let t = now();
        let app: Option<AppRow> = sqlx::query_as(
            "SELECT p.id, p.owner_id, p.name, p.disabled, p.created_at
             FROM api_keys k JOIN apps p ON p.id = k.app_id
             WHERE k.key_hash = $1 AND k.revoked_at IS NULL AND NOT p.disabled",
        )
        .bind(hash_secret(key))
        .fetch_optional(&self.pool)
        .await?;
        if app.is_some() {
            // At most one write a minute per key.
            sqlx::query(
                "UPDATE api_keys SET last_used_at = $2
                 WHERE key_hash = $1 AND (last_used_at IS NULL OR last_used_at < $2 - 60)",
            )
            .bind(hash_secret(key))
            .bind(t)
            .execute(&self.pool)
            .await?;
        }
        Ok(app)
    }

    /// Creates the built-in default app if missing.
    pub async fn ensure_default_app(&self) -> Result<()> {
        sqlx::query(
            "INSERT INTO apps (id, owner_id, name, created_at) VALUES ($1, NULL, 'default', $2)
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(DEFAULT_APP_ID)
        .bind(now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn create_key(&self, app_id: &str, name: &str) -> Result<Result<(KeyRow, String), KeyError>> {
        let mut tx = self.pool.begin().await?;
        // Lock the app row so two concurrent creates can't both pass the cap.
        let exists: Option<String> = sqlx::query_scalar("SELECT id FROM apps WHERE id = $1 FOR UPDATE")
            .bind(app_id)
            .fetch_optional(&mut *tx)
            .await?;
        if exists.is_none() {
            return Ok(Err(KeyError::NoSuchApp));
        }
        let active: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM api_keys WHERE app_id = $1 AND revoked_at IS NULL",
        )
        .bind(app_id)
        .fetch_one(&mut *tx)
        .await?;
        if active >= MAX_ACTIVE_KEYS {
            return Ok(Err(KeyError::TooMany));
        }
        let key = random_secret("nexk");
        let row = KeyRow {
            id: random_id("key"),
            app_id: app_id.to_string(),
            name: name.to_string(),
            key_prefix: key.chars().take(12).collect(),
            created_at: now(),
            last_used_at: None,
            revoked_at: None,
        };
        sqlx::query(
            "INSERT INTO api_keys (id, app_id, name, key_hash, key_prefix, created_at)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(&row.id)
        .bind(app_id)
        .bind(name)
        .bind(hash_secret(&key))
        .bind(&row.key_prefix)
        .bind(row.created_at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Ok((row, key)))
    }

    /// Active keys first, then the 20 most recently revoked.
    pub async fn list_keys(&self, app_id: &str) -> Result<Vec<KeyRow>> {
        Ok(sqlx::query_as(
            "(SELECT id, app_id, name, key_prefix, created_at, last_used_at, revoked_at
              FROM api_keys WHERE app_id = $1 AND revoked_at IS NULL)
             UNION ALL
             (SELECT id, app_id, name, key_prefix, created_at, last_used_at, revoked_at
              FROM api_keys WHERE app_id = $1 AND revoked_at IS NOT NULL
              ORDER BY revoked_at DESC LIMIT 20)
             ORDER BY revoked_at DESC NULLS FIRST, created_at DESC",
        )
        .bind(app_id)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn revoke_key(&self, app_id: &str, key_id: &str) -> Result<bool> {
        let r = sqlx::query(
            "UPDATE api_keys SET revoked_at = $3 WHERE id = $1 AND app_id = $2 AND revoked_at IS NULL",
        )
        .bind(key_id)
        .bind(app_id)
        .bind(now())
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() == 1)
    }

    // ---------- apps (console) ----------

    pub async fn create_app(&self, owner_id: &str, name: &str) -> Result<AppRow> {
        let app = AppRow {
            id: random_id("app"),
            owner_id: Some(owner_id.to_string()),
            name: name.to_string(),
            disabled: false,
            created_at: now(),
        };
        sqlx::query("INSERT INTO apps (id, owner_id, name, created_at) VALUES ($1, $2, $3, $4)")
            .bind(&app.id)
            .bind(owner_id)
            .bind(name)
            .bind(app.created_at)
            .execute(&self.pool)
            .await?;
        Ok(app)
    }

    pub async fn get_app(&self, app_id: &str) -> Result<Option<AppRow>> {
        Ok(
            sqlx::query_as("SELECT id, owner_id, name, disabled, created_at FROM apps WHERE id = $1")
                .bind(app_id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// Apps of `owner_id`, or every app when None.
    pub async fn list_apps(&self, owner_id: Option<&str>) -> Result<Vec<AppSummary>> {
        Ok(sqlx::query_as(
            "SELECT p.id, p.owner_id, u.email AS owner_email, p.name, p.disabled, p.created_at,
               (SELECT count(*) FROM accounts a WHERE a.app_id = p.id) AS accounts,
               (SELECT count(*) FROM devices d JOIN accounts a ON a.id = d.account_id
                 WHERE a.app_id = p.id AND d.revoked_at IS NULL) AS devices
             FROM apps p LEFT JOIN users u ON u.id = p.owner_id
             WHERE $1::text IS NULL OR p.owner_id = $1
             ORDER BY p.created_at DESC, p.id",
        )
        .bind(owner_id)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn update_app(&self, app_id: &str, name: Option<&str>, disabled: Option<bool>) -> Result<()> {
        sqlx::query(
            "UPDATE apps SET name = COALESCE($2, name), disabled = COALESCE($3, disabled) WHERE id = $1",
        )
        .bind(app_id)
        .bind(name)
        .bind(disabled)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Deletes the app with everything in it; returns its device ids so the
    /// caller can drop their live connections.
    pub async fn delete_app(&self, app_id: &str) -> Result<Vec<String>> {
        let ids = self.app_device_ids(app_id).await?;
        sqlx::query("DELETE FROM apps WHERE id = $1")
            .bind(app_id)
            .execute(&self.pool)
            .await?;
        Ok(ids)
    }

    pub async fn app_device_ids(&self, app_id: &str) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar(
            "SELECT d.id FROM devices d JOIN accounts a ON a.id = d.account_id
             WHERE a.app_id = $1 AND d.revoked_at IS NULL",
        )
        .bind(app_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// The app's most recently created accounts.
    pub async fn list_accounts(&self, app_id: &str, limit: i64) -> Result<Vec<AccountRow>> {
        Ok(sqlx::query_as(
            "SELECT id, name, external_id, created_at FROM accounts
             WHERE app_id = $1 ORDER BY created_at DESC, id LIMIT $2",
        )
        .bind(app_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Active devices of the given accounts of an app.
    pub async fn devices_of_accounts(&self, app_id: &str, account_ids: &[String]) -> Result<Vec<DeviceRow>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {DEVICE_COLUMNS} FROM devices d JOIN accounts a ON a.id = d.account_id
             WHERE a.app_id = $1 AND d.account_id = ANY($2) AND d.revoked_at IS NULL
             ORDER BY d.created_at, d.id"
        ))
        .bind(app_id)
        .bind(account_ids)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Revokes a device of an app (console). Returns false if not found.
    pub async fn revoke_app_device(&self, app_id: &str, device_id: &str) -> Result<bool> {
        let r = sqlx::query(
            "UPDATE devices d SET revoked_at = $3 FROM accounts a
             WHERE d.id = $1 AND a.id = d.account_id AND a.app_id = $2 AND d.revoked_at IS NULL",
        )
        .bind(device_id)
        .bind(app_id)
        .bind(now())
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() == 1)
    }

    // ---------- console users ----------

    pub async fn user_count(&self) -> Result<i64> {
        Ok(sqlx::query_scalar("SELECT count(*) FROM users").fetch_one(&self.pool).await?)
    }

    /// Returns None if the email is taken.
    pub async fn create_user(&self, email: &str, password_hash: &str, role: &str) -> Result<Option<UserRow>> {
        let user = UserRow {
            id: random_id("usr"),
            email: email.to_string(),
            password_hash: password_hash.to_string(),
            role: role.to_string(),
            created_at: now(),
        };
        let r = sqlx::query(
            "INSERT INTO users (id, email, password_hash, role, created_at) VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (email) DO NOTHING",
        )
        .bind(&user.id)
        .bind(email)
        .bind(password_hash)
        .bind(role)
        .bind(user.created_at)
        .execute(&self.pool)
        .await?;
        Ok((r.rows_affected() == 1).then_some(user))
    }

    pub async fn user_by_email(&self, email: &str) -> Result<Option<UserRow>> {
        Ok(sqlx::query_as(
            "SELECT id, email, password_hash, role, created_at FROM users WHERE email = $1",
        )
        .bind(email)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn user_by_id(&self, id: &str) -> Result<Option<UserRow>> {
        Ok(sqlx::query_as(
            "SELECT id, email, password_hash, role, created_at FROM users WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn list_users(&self) -> Result<Vec<UserSummary>> {
        Ok(sqlx::query_as(
            "SELECT u.id, u.email, u.role, u.created_at,
               (SELECT count(*) FROM apps p WHERE p.owner_id = u.id) AS apps
             FROM users u ORDER BY u.created_at, u.id",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// Changes a role, refusing to demote the last administrator.
    /// Returns false if that would happen or the user is gone.
    pub async fn set_role(&self, user_id: &str, role: &str) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        lock_admins(&mut tx).await?;
        let r = sqlx::query(
            "UPDATE users SET role = $2 WHERE id = $1
             AND ($2 = 'admin' OR role <> 'admin'
                  OR (SELECT count(*) FROM users WHERE role = 'admin') > 1)",
        )
        .bind(user_id)
        .bind(role)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(r.rows_affected() == 1)
    }

    /// Deletes a user with their apps, refusing to delete the last
    /// administrator. Returns the device ids of the deleted apps, or None.
    pub async fn delete_user(&self, user_id: &str) -> Result<Option<Vec<String>>> {
        let mut tx = self.pool.begin().await?;
        lock_admins(&mut tx).await?;
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT d.id FROM devices d JOIN accounts a ON a.id = d.account_id
             JOIN apps p ON p.id = a.app_id WHERE p.owner_id = $1 AND d.revoked_at IS NULL",
        )
        .bind(user_id)
        .fetch_all(&mut *tx)
        .await?;
        let r = sqlx::query(
            "DELETE FROM users WHERE id = $1
             AND (role <> 'admin' OR (SELECT count(*) FROM users WHERE role = 'admin') > 1)",
        )
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok((r.rows_affected() == 1).then_some(ids))
    }

    pub async fn set_password(&self, user_id: &str, password_hash: &str) -> Result<()> {
        sqlx::query("UPDATE users SET password_hash = $2 WHERE id = $1")
            .bind(user_id)
            .bind(password_hash)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ---------- sessions, rate limits, settings ----------

    pub async fn create_session(&self, token_hash: &str, user_id: &str, ttl_secs: i64) -> Result<()> {
        let t = now();
        sqlx::query("DELETE FROM sessions WHERE expires_at <= $1")
            .bind(t)
            .execute(&self.pool)
            .await?;
        sqlx::query("INSERT INTO sessions (token_hash, user_id, created_at, expires_at) VALUES ($1, $2, $3, $4)")
            .bind(token_hash)
            .bind(user_id)
            .bind(t)
            .bind(t + ttl_secs)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn session_user(&self, token_hash: &str) -> Result<Option<UserRow>> {
        Ok(sqlx::query_as(
            "SELECT u.id, u.email, u.password_hash, u.role, u.created_at
             FROM sessions s JOIN users u ON u.id = s.user_id
             WHERE s.token_hash = $1 AND s.expires_at > $2",
        )
        .bind(token_hash)
        .bind(now())
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn delete_session(&self, token_hash: &str) -> Result<()> {
        sqlx::query("DELETE FROM sessions WHERE token_hash = $1")
            .bind(token_hash)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Ends every session of a user except `keep` (a token hash).
    pub async fn delete_other_sessions(&self, user_id: &str, keep: &str) -> Result<()> {
        sqlx::query("DELETE FROM sessions WHERE user_id = $1 AND token_hash <> $2")
            .bind(user_id)
            .bind(keep)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Counts an attempt in `bucket`'s current window; returns the count.
    pub async fn attempt(&self, bucket: &str, window_secs: i64) -> Result<i64> {
        let t = now();
        let count: i32 = sqlx::query_scalar(
            "INSERT INTO login_limits (bucket, window_start, count) VALUES ($1, $2, 1)
             ON CONFLICT (bucket) DO UPDATE SET
               window_start = CASE WHEN login_limits.window_start <= $2 - $3 THEN $2 ELSE login_limits.window_start END,
               count = CASE WHEN login_limits.window_start <= $2 - $3 THEN 1 ELSE login_limits.count + 1 END
             RETURNING count",
        )
        .bind(bucket)
        .bind(t)
        .bind(window_secs)
        .fetch_one(&self.pool)
        .await?;
        Ok(count as i64)
    }

    pub async fn clear_attempts(&self, bucket: &str) -> Result<()> {
        sqlx::query("DELETE FROM login_limits WHERE bucket = $1")
            .bind(bucket)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(sqlx::query_scalar("SELECT value FROM settings WHERE key = $1")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?)
    }

    pub async fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO settings (key, value) VALUES ($1, $2)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

/// Serializes role changes and deletions so "keep one administrator" holds.
async fn lock_admins(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(7340110)")
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn insert_device(
    tx: &mut Transaction<'_, Postgres>,
    account_id: &str,
    name: &str,
    label: Option<&str>,
) -> Result<(String, String)> {
    let device_id = random_id("dev");
    let token = random_secret("nexd");
    sqlx::query(
        "INSERT INTO devices (id, account_id, name, token_hash, created_at, app)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(&device_id)
    .bind(account_id)
    .bind(name)
    .bind(hash_secret(&token))
    .bind(now())
    .bind(label)
    .execute(&mut **tx)
    .await?;
    Ok((device_id, token))
}
