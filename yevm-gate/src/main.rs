mod auth;
mod db;
mod tx;

use auth::AuthStore;
use axum::{
    Json, Router,
    extract::{Path, Request, State as AxumState},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use eyre::eyre;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use yevm_misc::hex::Hex;
use std::{collections::HashMap, net::SocketAddr, sync::Arc};
use tokio::sync::RwLock;
use tower_http::cors::CorsLayer;
use yevm_base::Acc;

const WASM_JS: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../yevm-wasm/pkg/yevm_wasm.js"
));
const WASM_BG: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../yevm-wasm/pkg/yevm_wasm_bg.wasm"
));

struct AppState {
    client: reqwest::Client,
    auth: Arc<AuthStore>,
    admin: Option<Acc>,
    pool: SqlitePool,
    chains: RwLock<HashMap<i64, String>>,
}

type Shared = Arc<AppState>;

#[derive(Clone)]
struct Caller(Acc);

// RUST_LOG=info YEVM_PROXY_BIND="127.0.0.1:8000" YEVM_DB="./target/gate.db" cargo run -p yevm-gate

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt::init();
    dotenvy::dotenv().ok();

    let bind: SocketAddr = std::env::var("YEVM_PROXY_BIND")
        .unwrap_or_else(|_| "127.0.0.1:8000".into())
        .parse()?;
    let admin: Option<Acc> = std::env::var("YEVM_ADMIN").ok().map(|s| {
        let s = s.trim().strip_prefix("0x").unwrap_or(&s);
        let bytes =
            hex::decode(s).unwrap_or_else(|_| panic!("YEVM_ADMIN is not a valid address: {s}"));
        Acc::from(bytes.as_slice())
    });
    if let Some(a) = &admin {
        tracing::info!("admin: {a}");
    }

    let db_path = std::env::var("YEVM_DB").unwrap_or_else(|_| "gate.db".into());
    let pool = db::open(&db_path).await?;   // runs migrations (seeds default networks)
    let chains: HashMap<i64, String> = db::list_chains(&pool).await?.into_iter().collect();
    tracing::info!("configured chains: {:?}", chains.keys().collect::<Vec<_>>());

    let state: Shared = Arc::new(AppState {
        client: reqwest::Client::new(),
        auth: AuthStore::new(pool.clone()),
        admin,
        pool,
        chains: RwLock::new(chains),
    });

    let api = Router::new()
        .route("/api/txs", get(api_list))
        .route("/api/txs/{hash}", get(api_get).delete(api_delete))
        .route("/api/txs/{hash}/submit", post(api_submit))
        .route("/chain", get(chains_get))
        .layer(middleware::from_fn_with_state(state.clone(), require_auth));

    let app = Router::new()
        .route("/", get(ui))
        .route("/static/{*path}", get(serve))
        .route("/rpc", post(handle_rpc))
        .route("/rpc/{chainId}", post(handle_rpc_chain))
        .route("/auth/challenge", get(auth_challenge))
        .route("/auth/verify", post(auth_verify))
        .merge(api)
        .layer(CorsLayer::permissive())
        .with_state(state);

    tracing::info!("yevm-gate listening on {bind}");
    let listener = tokio::net::TcpListener::bind(bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn require_auth(
    AxumState(state): AxumState<Shared>,
    mut req: Request,
    next: Next,
) -> Result<Response, AppError> {
    let token = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| {
            AppError::new(StatusCode::UNAUTHORIZED, eyre!("missing Authorization header"))
        })?;

    let addr = state
        .auth
        .authenticate(token)
        .await
        .ok_or_else(|| {
            AppError::new(StatusCode::UNAUTHORIZED, eyre!("invalid or expired session"))
        })?;

    req.extensions_mut().insert(Caller(addr));
    Ok(next.run(req).await)
}

async fn auth_challenge(AxumState(state): AxumState<Shared>) -> Result<Json<Value>, AppError> {
    let nonce = state.auth.new_challenge().await?;
    Ok(Json(json!({ "nonce": nonce })))
}

async fn auth_verify(
    AxumState(state): AxumState<Shared>,
    headers: header::HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, AppError> {
    let bad = |m: &str| AppError::new(StatusCode::BAD_REQUEST, eyre!("{m}"));
    let message = body
        .get("message")
        .and_then(|v| v.as_str())
        .ok_or_else(|| bad("missing message"))?;
    let signature = body
        .get("signature")
        .and_then(|v| v.as_str())
        .ok_or_else(|| bad("missing signature"))?;

    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .or(headers
            .get(header::FORWARDED)
            .and_then(|v| v.to_str().ok()))
        .ok_or_else(|| bad("missing Host header"))?;

    let (address, token) = state
        .auth
        .verify(message, signature, host)
        .await
        .map_err(|e| AppError::new(StatusCode::UNAUTHORIZED, e))?;
    Ok(Json(
        json!({ "address": format!("{address}"), "token": token }),
    ))
}

async fn resolve_url(state: &AppState, chain_id: i64) -> Option<String> {
    state.chains.read().await.get(&chain_id).cloned()
}

fn parse_chain_id(s: &str) -> Option<i64> {
    s.try_into().ok().map(|hex: Hex<8>| hex.as_u64() as i64)
}

async fn handle_rpc(
    AxumState(state): AxumState<Shared>,
    Json(body): Json<Value>,
) -> Result<Response, AppError> {
    let chain_id = 1; // Ethereum Mainnet
    proxy(state, chain_id, body).await
}

async fn handle_rpc_chain(
    AxumState(state): AxumState<Shared>,
    Path(chain): Path<String>,
    Json(body): Json<Value>,
) -> Result<Response, AppError> {
    let id = body.get("id").cloned().unwrap_or(Value::Null);
    match parse_chain_id(&chain) {
        Some(chain_id) => proxy(state, chain_id, body).await,
        None => Ok(rpc_error(id, -32602, format!("invalid chain id: {chain}")).into_response()),
    }
}

async fn proxy(state: Shared, chain_id: i64, body: Value) -> Result<Response, AppError> {
    let url = match resolve_url(&state, chain_id).await {
        Some(url) => url,
        None => {
            let id: Value = body.get("id").cloned().unwrap_or(Value::Null);
            return Ok(rpc_error(id, -32602, format!("chain {chain_id} not configured"))
                .into_response());
        }
    };

    if let Some(arr) = body.as_array() {
        let has_send_raw_tx = arr
            .iter()
            .any(|e| e.get("method").and_then(|m| m.as_str()) == Some("eth_sendRawTransaction"));
        tracing::info!("proxy chain={chain_id} batch[{}]{}", arr.len(), if has_send_raw_tx { " +send" } else { "" });
        if has_send_raw_tx {
            return handle_batch(state, chain_id, &url, arr).await;
        }
        return relay(&state, &url, &body).await;
    }

    let method = body.get("method").and_then(|m| m.as_str()).unwrap_or("");
    tracing::info!("proxy chain={chain_id} {method}");
    if method == "eth_sendRawTransaction" {
        return Ok(intercept_send_raw(state, body, chain_id).await?.into_response());
    }
    relay(&state, &url, &body).await
}

async fn relay(state: &AppState, url: &str, body: &Value) -> Result<Response, AppError> {
    let resp = state.client.post(url).json(body).send().await?;
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    let bytes = resp.bytes().await?;
    Ok((status, [(header::CONTENT_TYPE, ct)], bytes).into_response())
}

async fn handle_batch(
    state: Shared,
    chain_id: i64,
    url: &str,
    arr: &[Value],
) -> Result<Response, AppError> {
    let mut slots: Vec<Value> = vec![Value::Null; arr.len()];
    let mut forward: Vec<Value> = Vec::new();
    let mut fwd_pos: Vec<usize> = Vec::new();
    for (i, entry) in arr.iter().enumerate() {
        if entry.get("method").and_then(|m| m.as_str()) == Some("eth_sendRawTransaction") {
            slots[i] = store_raw_tx(&state, entry, chain_id).await;
        } else {
            fwd_pos.push(i);
            forward.push(entry.clone());
        }
    }
    if !forward.is_empty() {
        let resp = state.client.post(url).json(&Value::Array(forward)).send().await?;
        let items = match resp.json::<Value>().await? {
            Value::Array(v) => v,
            other => vec![other], // some nodes answer a 1-element batch with a bare object
        };
        for &i in &fwd_pos {
            let id = arr[i].get("id").cloned().unwrap_or(Value::Null);
            slots[i] = items
                .iter()
                .find(|r| r.get("id") == Some(&id))
                .cloned()
                .unwrap_or_else(|| rpc_error_json(id, -32603, "no matching response from upstream"));
        }
    }
    Ok(Json(Value::Array(slots)).into_response())
}

// Hard caps on the (unauthenticated-by-design) signed queue, so that storing a
// validly-signed tx for anyone who posts one can't be turned into a storage DoS.
const MAX_SIGNED_TOTAL: i64 = 1024 * 1024;
const MAX_SIGNED_PER_SENDER: i64 = 256;

// Build a JSON-RPC error response. The wallet posted JSON-RPC, so it must get
// JSON-RPC back (HTTP 200) - not an HTTP error with a foreign body.
fn rpc_error_json(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message.into() } })
}

fn rpc_error(id: Value, code: i64, message: impl Into<String>) -> Json<Value> {
    Json(rpc_error_json(id, code, message))
}

async fn store_raw_tx(state: &AppState, entry: &Value, chain_id: i64) -> Value {
    let id = entry.get("id").cloned().unwrap_or(Value::Null);
    let raw = match entry.get("params").and_then(|p| p.get(0)).and_then(|v| v.as_str()) {
        Some(raw) => raw.to_string(),
        None => return rpc_error_json(id, -32602, "missing params[0]"),
    };
    let decoded = match tx::decode_raw(&raw) {
        Ok(decoded) => decoded,
        Err(e) => return rpc_error_json(id, -32602, format!("cannot decode raw tx: {e}"))
    };
    let hash = format!("{}", decoded.tx.hash);
    let from = decoded.call.by;
    tracing::info!("intercepted tx hash={hash} from={from} chain={chain_id}");

    match db::count_total(&state.pool).await {
        Ok(n) if n >= MAX_SIGNED_TOTAL => return rpc_error_json(id, -32005, "gate queue is full"),
        Err(e) => return rpc_error_json(id, -32603, format!("db error: {e}")),
        _ => {}
    }
    match db::count_for_signer(&state.pool, &from).await {
        Ok(n) if n >= MAX_SIGNED_PER_SENDER => {
            return rpc_error_json(id, -32005, "too many txs for this sender");
        }
        Err(e) => return rpc_error_json(id, -32603, format!("db error: {e}")),
        _ => {}
    }
    if let Err(e) = db::insert(&state.pool, &hash, &from, &raw, chain_id).await {
        return rpc_error_json(id, -32603, format!("db error: {e}"));
    }
    json!({ "jsonrpc": "2.0", "id": id, "result": hash })
}

async fn intercept_send_raw(
    state: Shared,
    body: Value,
    chain_id: i64,
) -> Result<Json<Value>, AppError> {
    Ok(Json(store_raw_tx(&state, &body, chain_id).await))
}

fn is_admin(state: &AppState, caller: &Acc) -> bool {
    state.admin.as_ref() == Some(caller)
}

fn decoded_json(signed: &db::SignedTx) -> Result<Value, AppError> {
    let decoded = tx::decode_raw(&signed.raw).map_err(|e| {
        AppError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            eyre!("stored tx {} failed to decode: {e}", signed.hash),
        )
    })?;
    Ok(json!({
        "from": format!("{}", decoded.call.by),
        "hash": format!("{}", decoded.tx.hash),
        "call": decoded.call,
        "tx": decoded.tx,
    }))
}

async fn api_list(
    AxumState(state): AxumState<Shared>,
    axum::Extension(Caller(caller)): axum::Extension<Caller>,
) -> Result<Json<Value>, AppError> {
    let filter = if is_admin(&state, &caller) {
        None
    } else {
        Some(&caller)
    };
    let mut list = Vec::new();
    for tx in db::list(&state.pool, filter).await? {
        match decoded_json(&tx) {
            Ok(json) => list.push(json),
            Err(e) => tracing::warn!("skipping tx {}: {}", tx.hash, e.report),
        }
    }
    Ok(Json(json!(list)))
}

async fn owned(state: &AppState, caller: &Acc, hash: &str) -> Result<db::SignedTx, AppError> {
    let signed = db::get(&state.pool, hash)
        .await?
        .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, eyre!("tx not found: {hash}")))?;
    if !is_admin(state, caller) && signed.from != *caller {
        return Err(AppError::new(StatusCode::FORBIDDEN, eyre!("not your tx")));
    }
    Ok(signed)
}

async fn api_get(
    AxumState(state): AxumState<Shared>,
    axum::Extension(Caller(caller)): axum::Extension<Caller>,
    Path(hash): Path<String>,
) -> Result<Json<Value>, AppError> {
    let tx = owned(&state, &caller, &hash).await?;
    Ok(Json(decoded_json(&tx)?))
}

async fn api_submit(
    AxumState(state): AxumState<Shared>,
    axum::Extension(Caller(caller)): axum::Extension<Caller>,
    Path(hash): Path<String>,
) -> Result<Json<Value>, AppError> {
    let signed = owned(&state, &caller, &hash).await?;
    let url = resolve_url(&state, signed.chain_id).await;
    Ok(Json(json!({
        "raw": signed.raw,
        "chain_id": format!("0x{:x}", signed.chain_id),
        "url": url,
    })))
}

async fn api_delete(
    AxumState(state): AxumState<Shared>,
    axum::Extension(Caller(caller)): axum::Extension<Caller>,
    Path(hash): Path<String>,
) -> Result<Json<Value>, AppError> {
    owned(&state, &caller, &hash).await?;
    db::delete(&state.pool, &hash).await?;
    Ok(Json(json!({ "deleted": hash })))
}

async fn chains_get(AxumState(state): AxumState<Shared>) -> Json<Value> {
    let map: tokio::sync::RwLockReadGuard<'_, HashMap<i64, String>> = state.chains.read().await;
    let obj: serde_json::Map<String, Value> = map
        .iter()
        .map(|(id, url)| (format!("0x{id:x}"), Value::String(url.clone())))
        .collect();
    Json(Value::Object(obj))
}

async fn ui() -> Html<&'static str> {
    Html(include_str!("../web/index.html"))
}

async fn serve(Path(path): Path<String>) -> Response {
    let asset: Option<(&'static [u8], &'static str)> = match path.as_str() {
        "yevm_wasm.js" => Some((WASM_JS, "application/javascript")),
        "yevm_wasm_bg.wasm" => Some((WASM_BG, "application/wasm")),
        _ => None,
    };
    match asset {
        Some((bytes, ct)) => ([(header::CONTENT_TYPE, ct)], bytes).into_response(),
        None => (StatusCode::NOT_FOUND, "no such wasm asset").into_response(),
    }
}

struct AppError {
    status: StatusCode,
    report: eyre::Report,
}

impl AppError {
    fn new(status: StatusCode, report: eyre::Report) -> Self {
        Self { status, report }
    }
}

impl From<eyre::Report> for AppError {
    fn from(report: eyre::Report) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, report)
    }
}

impl From<reqwest::Error> for AppError {
    fn from(e: reqwest::Error) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, e.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let body = json!({ "error": self.report.to_string() });
        (self.status, Json(body)).into_response()
    }
}