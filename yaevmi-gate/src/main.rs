mod auth;
mod decode;

use auth::AuthStore;
use axum::{
    extract::{Path, Request, State as AxumState},
    middleware::{self, Next},
    response::Html,
    routing::{get, post},
    Json, Router,
};
use eyre::eyre;
use futures::StreamExt;
use serde_json::{json, Value};
use std::{collections::HashMap, net::SocketAddr, sync::Arc};
use tokio::sync::RwLock;
use yaevmi_base::{Acc, Int};
use yaevmi_core::{
    cache::Cache,
    chain::Chain,
    exe::{CallResult, Executor},
    rpc::Rpc,
    state::State,
    trace::{filter, Trace},
    Call, Head, Tx,
};

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SimResult {
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gas_used: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub success: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub traces: Vec<Trace>,
}

struct PendingTx {
    from: Acc,
    raw: String,
    sim: SimResult,
}

struct AppState {
    rpc_url: String,
    client: reqwest::Client,
    auth: Arc<AuthStore>,
    pending: RwLock<HashMap<String, PendingTx>>,
}

type Shared = Arc<AppState>;

#[derive(Clone)]
struct Caller(Acc);

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt::init();
    dotenvy::dotenv().ok();

    let rpc_url = std::env::var("YAEVMI_RPC_URL").map_err(|_| eyre!("YAEVMI_RPC_URL not set"))?;
    let bind: SocketAddr = std::env::var("YAEVMI_PROXY_BIND")
        .unwrap_or_else(|_| "127.0.0.1:8000".into())
        .parse()?;

    let state: Shared = Arc::new(AppState {
        rpc_url,
        client: reqwest::Client::new(),
        auth: AuthStore::new(),
        pending: RwLock::new(HashMap::new()),
    });

    let api = Router::new()
        .route("/api/txs", get(api_list))
        .route("/api/txs/{hash}", get(api_get))
        .route("/api/txs/{hash}/submit", post(api_submit))
        .route("/api/txs/{hash}/reject", post(api_reject))
        .layer(middleware::from_fn_with_state(state.clone(), require_auth));

    let app = Router::new()
        .route("/", get(ui))
        .route("/rpc", post(handle_rpc))
        .route("/auth/challenge", get(auth_challenge))
        .route("/auth/verify", post(auth_verify))
        .merge(api)
        .with_state(state);

    tracing::info!("yaevmi-gate listening on {bind}");
    let listener = tokio::net::TcpListener::bind(bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn require_auth(
    AxumState(state): AxumState<Shared>,
    mut req: Request,
    next: Next,
) -> Result<axum::response::Response, AppError> {
    let token = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| eyre!("missing Authorization header"))?;

    let addr = state
        .auth
        .authenticate(token)
        .await
        .ok_or_else(|| eyre!("invalid or expired session"))?;

    req.extensions_mut().insert(Caller(addr));
    Ok(next.run(req).await)
}

async fn auth_challenge(AxumState(state): AxumState<Shared>) -> Json<Value> {
    let nonce = state.auth.new_challenge().await;
    Json(json!({ "nonce": nonce }))
}

async fn auth_verify(
    AxumState(state): AxumState<Shared>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, AppError> {
    let nonce = body
        .get("nonce")
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre!("missing nonce"))?;
    let signature = body
        .get("signature")
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre!("missing signature"))?;

    let (address, token) = state.auth.verify(nonce, signature).await?;
    Ok(Json(
        json!({ "address": format!("{address}"), "token": token }),
    ))
}

async fn handle_rpc(
    AxumState(state): AxumState<Shared>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, AppError> {
    let method = body.get("method").and_then(|m| m.as_str()).unwrap_or("");
    tracing::info!("proxy {method}");
    if method == "eth_sendRawTransaction" {
        return intercept_send_raw(state, body).await;
    }
    let resp = state.client.post(&state.rpc_url).json(&body).send().await?;
    Ok(Json(resp.json::<Value>().await?))
}

async fn intercept_send_raw(state: Shared, body: Value) -> Result<Json<Value>, AppError> {
    let id = body.get("id").cloned().unwrap_or(Value::Null);
    let raw = body
        .get("params")
        .and_then(|p| p.get(0))
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre!("missing params[0]"))?
        .to_string();

    let decoded = decode::decode_raw(&raw).map_err(|e| eyre!("{e}"))?;
    let hash = format!("{}", decoded.tx.hash);
    tracing::info!("intercepted tx hash={hash} from={}", decoded.call.by);
    let from = decoded.call.by;

    {
        let mut pending = state.pending.write().await;
        pending.insert(
            hash.clone(),
            PendingTx {
                from,
                raw: raw.clone(),
                sim: SimResult {
                    status: "pending",
                    error: None,
                    gas_used: None,
                    success: None,
                    traces: vec![],
                },
            },
        );
    }

    spawn_sim(state, hash.clone(), decoded.call, decoded.tx);
    Ok(Json(json!({ "jsonrpc": "2.0", "id": id, "result": hash })))
}

fn spawn_sim(state: Shared, hash: String, call: Call, tx: Tx) {
    let rpc_url = state.rpc_url.clone();
    tokio::spawn(async move {
        let result = run_sim(rpc_url, call, tx).await;
        let mut pending = state.pending.write().await;
        if let Some(entry) = pending.get_mut(&hash) {
            match result {
                Ok((gas, success, traces)) => {
                    entry.sim = SimResult {
                        status: "done",
                        error: None,
                        gas_used: Some(gas),
                        success: Some(success),
                        traces,
                    };
                }
                Err(e) => {
                    entry.sim = SimResult {
                        status: "failed",
                        error: Some(e.to_string()),
                        gas_used: None,
                        success: None,
                        traces: vec![],
                    };
                }
            }
        }
    });
}

async fn run_sim(rpc_url: String, call: Call, tx: Tx) -> eyre::Result<(u64, bool, Vec<Trace>)> {
    let mut rpc = Rpc::latest(rpc_url).await?;
    let chain_id = rpc.chain_id().await?;
    let head: Head = rpc.block(rpc.block_number).await?.head;
    rpc.reset(head.number.as_u64(), head.hash);

    let (ytx, yrx) = futures::channel::mpsc::channel(1024 * 1024);
    let mut cache = Cache::with_sender(
        ytx,
        filter::MOVE | filter::PUT | filter::FEE | filter::LOG | filter::CREATE,
    );
    cache.set_chain_id(chain_id);

    let mut exe = Executor::new(call);
    let result = exe.run(tx, head, &mut cache, &rpc).await?;
    drop(cache);

    let traces: Vec<Trace> = yrx.collect().await;
    let (gas, success) = match result {
        CallResult::Done { gas, status, .. } => (gas.finalized as u64, status == Int::from(1u64)),
        CallResult::Created { gas, .. } => (gas.finalized as u64, true),
    };
    Ok((gas, success, traces))
}

async fn api_list(
    AxumState(state): AxumState<Shared>,
    axum::Extension(Caller(caller)): axum::Extension<Caller>,
) -> Json<Value> {
    let pending = state.pending.read().await;
    let list: Vec<_> = pending
        .iter()
        .filter(|(_, p)| p.from == caller)
        .map(|(hash, p)| json!({ "hash": hash, "sim": p.sim }))
        .collect();
    Json(json!(list))
}

async fn api_get(
    AxumState(state): AxumState<Shared>,
    axum::Extension(Caller(caller)): axum::Extension<Caller>,
    Path(hash): Path<String>,
) -> Result<Json<Value>, AppError> {
    let pending = state.pending.read().await;
    let entry = pending
        .get(&hash)
        .ok_or_else(|| eyre!("tx not found: {hash}"))?;
    if entry.from != caller {
        return Err(eyre!("not your tx").into());
    }
    Ok(Json(json!({ "hash": hash, "sim": entry.sim })))
}

async fn api_submit(
    AxumState(state): AxumState<Shared>,
    axum::Extension(Caller(caller)): axum::Extension<Caller>,
    Path(hash): Path<String>,
) -> Result<Json<Value>, AppError> {
    let raw = {
        let pending = state.pending.read().await;
        let entry = pending
            .get(&hash)
            .ok_or_else(|| eyre!("tx not found: {hash}"))?;
        if entry.from != caller {
            return Err(eyre!("not your tx").into());
        }
        entry.raw.clone()
    };
    let body =
        json!({ "jsonrpc": "2.0", "method": "eth_sendRawTransaction", "params": [raw], "id": 1 });
    let resp = state.client.post(&state.rpc_url).json(&body).send().await?;
    let result: Value = resp.json().await?;
    state.pending.write().await.remove(&hash);
    Ok(Json(result))
}

async fn api_reject(
    AxumState(state): AxumState<Shared>,
    axum::Extension(Caller(caller)): axum::Extension<Caller>,
    Path(hash): Path<String>,
) -> Result<Json<Value>, AppError> {
    let mut pending = state.pending.write().await;
    let entry = pending
        .get(&hash)
        .ok_or_else(|| eyre!("tx not found: {hash}"))?;
    if entry.from != caller {
        return Err(eyre!("not your tx").into());
    }
    pending.remove(&hash);
    Ok(Json(json!({ "rejected": hash })))
}

async fn ui() -> Html<&'static str> {
    Html(include_str!("../../yaevmi-gate/web/index.html"))
}

struct AppError(eyre::Report);
impl From<eyre::Report> for AppError {
    fn from(e: eyre::Report) -> Self {
        Self(e)
    }
}
impl From<reqwest::Error> for AppError {
    fn from(e: reqwest::Error) -> Self {
        Self(e.into())
    }
}
impl axum::response::IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        let body = json!({ "error": self.0.to_string() });
        (axum::http::StatusCode::UNAUTHORIZED, Json(body)).into_response()
    }
}
