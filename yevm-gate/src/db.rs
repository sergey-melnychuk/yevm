use eyre::Result;
use sqlx::{SqlitePool, sqlite::SqliteConnectOptions};
use std::str::FromStr;
use yevm_base::Acc;

pub struct SignedTx {
    pub hash: String,
    pub from: Acc,
    pub raw: String,
    pub chain_id: i64,
}

pub async fn open(path: &str) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::from_str(&format!("sqlite:{path}"))?.create_if_missing(true);
    let pool = SqlitePool::connect_with(opts).await?;
    sqlx::migrate!("./db").run(&pool).await?;
    Ok(pool)
}

fn row_to_tx(hash: String, signer: String, raw: String, chain_id: i64) -> Option<SignedTx> {
    match signer.as_str().try_into().ok() {
        Some(from) => Some(SignedTx {
            hash,
            from,
            raw,
            chain_id,
        }),
        None => {
            tracing::warn!("skipping tx {hash}: invalid signer in DB: {signer:?}");
            None
        }
    }
}

pub async fn list(pool: &SqlitePool, signer: Option<&Acc>) -> Result<Vec<SignedTx>> {
    let rows = match signer {
        Some(acc) => {
            sqlx::query_as::<_, (String, String, String, i64)>(
                "SELECT hash, signer, raw, chain_id FROM txs WHERE signer = ?",
            )
            .bind(format!("{acc}"))
            .fetch_all(pool)
            .await?
        }
        None => {
            sqlx::query_as::<_, (String, String, String, i64)>(
                "SELECT hash, signer, raw, chain_id FROM txs",
            )
            .fetch_all(pool)
            .await?
        }
    };
    Ok(rows
        .into_iter()
        .filter_map(|(hash, signer, raw, chain_id)| row_to_tx(hash, signer, raw, chain_id))
        .collect())
}

pub async fn count_total(pool: &SqlitePool) -> Result<i64> {
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM txs")
        .fetch_one(pool)
        .await?;
    Ok(n)
}

pub async fn count_for_signer(pool: &SqlitePool, signer: &Acc) -> Result<i64> {
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM txs WHERE signer = ?")
        .bind(format!("{signer}"))
        .fetch_one(pool)
        .await?;
    Ok(n)
}

pub async fn get(pool: &SqlitePool, hash: &str) -> Result<Option<SignedTx>> {
    let row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT hash, signer, raw, chain_id FROM txs WHERE hash = ?",
    )
    .bind(hash)
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(|(hash, signer, raw, chain_id)| row_to_tx(hash, signer, raw, chain_id)))
}

pub async fn insert(
    pool: &SqlitePool,
    hash: &str,
    from: &Acc,
    raw: &str,
    chain_id: i64,
) -> Result<()> {
    let signer = format!("{from}");
    sqlx::query("INSERT OR IGNORE INTO txs (hash, signer, raw, chain_id) VALUES (?, ?, ?, ?)")
        .bind(hash)
        .bind(&signer)
        .bind(raw)
        .bind(chain_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete(pool: &SqlitePool, hash: &str) -> Result<()> {
    sqlx::query("DELETE FROM txs WHERE hash = ?")
        .bind(hash)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn list_chains(pool: &SqlitePool) -> Result<Vec<(i64, String)>> {
    let rows = sqlx::query_as::<_, (i64, String)>("SELECT id, url FROM chains ORDER BY id")
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

pub async fn update_chain(pool: &SqlitePool, id: i64, url: &str) -> Result<()> {
    sqlx::query("INSERT INTO chains (id, url) VALUES (?, ?) ON CONFLICT(id) DO UPDATE SET url = excluded.url")
        .bind(id)
        .bind(url)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete_chain(pool: &SqlitePool, id: i64) -> Result<()> {
    sqlx::query("DELETE FROM chains WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}
