-- Console users: people who sign in at /admin and own apps.
CREATE TABLE users (
    id             TEXT PRIMARY KEY,
    email          TEXT NOT NULL UNIQUE,
    -- "scrypt:<salt hex>:<key hex>", the same format as imc and connany.
    password_hash  TEXT NOT NULL,
    role           TEXT NOT NULL DEFAULT 'member' CHECK (role IN ('admin', 'member')),
    created_at     BIGINT NOT NULL
);

CREATE TABLE sessions (
    token_hash  TEXT PRIMARY KEY,
    user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at  BIGINT NOT NULL,
    expires_at  BIGINT NOT NULL
);
CREATE INDEX sessions_user ON sessions(user_id);

-- Fixed-window attempt counters (sign-in, sign-up).
CREATE TABLE login_limits (
    bucket        TEXT PRIMARY KEY,
    window_start  BIGINT NOT NULL,
    count         INTEGER NOT NULL
);

CREATE TABLE settings (
    key    TEXT PRIMARY KEY,
    value  TEXT NOT NULL
);

-- An app is one agent product that needs networking. Every account, device
-- and pairing code belongs to exactly one app; apps never see each other.
-- owner_id is NULL only for the built-in "default" app (NEX_ADMIN_KEY).
CREATE TABLE apps (
    id          TEXT PRIMARY KEY,
    owner_id    TEXT REFERENCES users(id) ON DELETE CASCADE,
    name        TEXT NOT NULL,
    disabled    BOOLEAN NOT NULL DEFAULT FALSE,
    created_at  BIGINT NOT NULL
);
CREATE INDEX apps_owner ON apps(owner_id);

-- App keys for product backends (x-nex-admin-key or Bearer nexk_…).
-- Only the SHA-256 and a display prefix are kept.
CREATE TABLE api_keys (
    id            TEXT PRIMARY KEY,
    app_id        TEXT NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    name          TEXT NOT NULL DEFAULT '',
    key_hash      TEXT NOT NULL UNIQUE,
    key_prefix    TEXT NOT NULL,
    created_at    BIGINT NOT NULL,
    last_used_at  BIGINT,
    revoked_at    BIGINT
);
CREATE INDEX api_keys_app ON api_keys(app_id);

-- An account is one end user of an app: their devices reach each other.
CREATE TABLE accounts (
    id           TEXT PRIMARY KEY,
    app_id       TEXT NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    name         TEXT NOT NULL,
    -- The product's own user id (admin API); NULL for bootstrapped accounts.
    external_id  TEXT,
    created_at   BIGINT NOT NULL
);
CREATE INDEX accounts_app ON accounts(app_id);
CREATE UNIQUE INDEX accounts_external ON accounts(app_id, external_id) WHERE external_id IS NOT NULL;

CREATE TABLE devices (
    id            TEXT PRIMARY KEY,
    account_id    TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name          TEXT NOT NULL,
    token_hash    TEXT NOT NULL UNIQUE,
    -- Free-form client label (e.g. "termany"), chosen by the device.
    app           TEXT,
    created_at    BIGINT NOT NULL,
    last_seen_at  BIGINT,
    revoked_at    BIGINT
);
CREATE INDEX devices_account ON devices(account_id);

CREATE TABLE pairing_codes (
    code_hash   TEXT PRIMARY KEY,
    account_id  TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    created_by  TEXT NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
    expires_at  BIGINT NOT NULL,
    used_at     BIGINT
);
CREATE INDEX pairing_codes_expires ON pairing_codes(expires_at);
