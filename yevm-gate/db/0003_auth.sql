CREATE TABLE IF NOT EXISTS auth_challenges (
    nonce      TEXT PRIMARY KEY,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS auth_sessions (
    token      TEXT PRIMARY KEY,
    signer     TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
