CREATE TABLE IF NOT EXISTS txs (
    hash     TEXT PRIMARY KEY,
    signer   TEXT NOT NULL,
    raw      TEXT NOT NULL,
    chain_id INTEGER NOT NULL DEFAULT 1
);
