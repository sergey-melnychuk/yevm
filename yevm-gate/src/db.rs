use eyre::Result;
use sqlx::{SqlitePool, sqlite::SqliteConnectOptions};
use std::{collections::HashMap, str::FromStr};
use yevm_base::Acc;

use crate::{PendingTx, SimResult};

pub async fn open(path: &str) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::from_str(&format!("sqlite:{path}"))?.create_if_missing(true);
    let pool = SqlitePool::connect_with(opts).await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS pending_txs (
            hash     TEXT PRIMARY KEY,
            from_addr TEXT NOT NULL,
            raw      TEXT NOT NULL,
            sim      TEXT NOT NULL
        )",
    )
    .execute(&pool)
    .await?;
    Ok(pool)
}

pub async fn load_all(pool: &SqlitePool) -> Result<HashMap<String, PendingTx>> {
    let rows = sqlx::query_as::<_, (String, String, String, String)>(
        "SELECT hash, from_addr, raw, sim FROM pending_txs",
    )
    .fetch_all(pool)
    .await?;

    let mut map = HashMap::new();
    for (hash, from_addr, raw, sim_json) in rows {
        let from_bytes = hex::decode(from_addr.trim_start_matches("0x")).unwrap_or_default();
        let from = Acc::from(from_bytes.as_slice());
        let sim: SimResult = match serde_json::from_str(&sim_json) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("skipping malformed sim for {hash}: {e}");
                continue;
            }
        };
        map.insert(hash, PendingTx { from, raw, sim });
    }
    Ok(map)
}

pub async fn insert(
    pool: &SqlitePool,
    hash: &str,
    from: &Acc,
    raw: &str,
    sim: &SimResult,
) -> Result<()> {
    let from_str = format!("{from}");
    let sim_json = serde_json::to_string(sim)?;
    sqlx::query(
        "INSERT OR IGNORE INTO pending_txs (hash, from_addr, raw, sim) VALUES (?, ?, ?, ?)",
    )
    .bind(hash)
    .bind(&from_str)
    .bind(raw)
    .bind(&sim_json)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn update_sim(pool: &SqlitePool, hash: &str, sim: &SimResult) -> Result<()> {
    let sim_json = serde_json::to_string(sim)?;
    sqlx::query("UPDATE pending_txs SET sim = ? WHERE hash = ?")
        .bind(&sim_json)
        .bind(hash)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete(pool: &SqlitePool, hash: &str) -> Result<()> {
    sqlx::query("DELETE FROM pending_txs WHERE hash = ?")
        .bind(hash)
        .execute(pool)
        .await?;
    Ok(())
}
