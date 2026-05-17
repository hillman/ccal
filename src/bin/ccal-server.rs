//! ccal sync server — a tiny always-on Automerge *peer*.
//!
//! It holds the same Automerge document as every client, runs the standard
//! `automerge` sync protocol over a WebSocket per connection, merges
//! everything server-side, and rebroadcasts so other connected peers
//! converge. There is no DB engine, no schema, no migrations: the
//! `ccal::Store` is the only thing that knows Automerge exists, exactly as
//! in the interactive client.
//!
//! Wire protocol (deliberately language-neutral so `automerge-swift` on iOS
//! speaks it unchanged):
//!   - `wss://host/sync/{docid}` WebSocket upgrade
//!   - `Authorization: Bearer <token>` checked at the upgrade; a wrong/absent
//!     token is rejected before the socket opens — no in-band auth frame
//!   - every binary frame is raw `automerge::sync::Message` bytes, nothing
//!     else; text/ping frames are ignored (reserved for future control)
//!
//! Trust model: the operator owns and trusts this box. The document is
//! stored as plaintext (atomic temp+rename, debounced). That also makes the
//! server a free backup of the whole corpus.
//!
//! Config — env var or `[server]` in the TOML config file (see
//! `ccal::config`), env winning, with these resolved values:
//!   token     required — shared bearer token (CCAL_SYNC_TOKEN / token)
//!   addr      listen address      (CCAL_SYNC_ADDR / addr, def 127.0.0.1:8787)
//!   data_dir  {docid}.automerge replica dir (CCAL_SYNC_DATA / data_dir,
//!             default: OS data dir / ccal-server)
//! Use `addr = "0.0.0.0:PORT"` to listen on all interfaces.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::{Path, Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use ccal::{Store, SyncState};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use tokio::sync::{broadcast, Mutex, Notify};
use tokio_util::sync::CancellationToken;

// Binary-private, exactly like the TUI's `app`/`ui`/`sync_client`: the
// embedded MCP server lives in a binary, never the lib, so `ccal` stays
// tokio-free and Automerge stays sealed in `ccal::store`. `#[path]` keeps
// the file out of `src/bin/` so cargo doesn't treat it as its own binary.
#[path = "../server_mcp.rs"]
mod mcp;

/// Everything one document needs to be a shared peer: the merged replica,
/// a change-notify so live connections re-run their sync loop when *another*
/// peer pushes, and a dirty-flag the debounced saver waits on.
struct Doc {
    store: Mutex<Store>,
    /// Pinged whenever the document changes, so every connection flushes
    /// fresh sync messages to its peer. `()` payload — it's a wakeup, the
    /// real state lives in `store`.
    changed: broadcast::Sender<()>,
    /// Raised on any change; the per-doc saver debounces on it.
    dirty: Notify,
}

struct App {
    token: String,
    data_dir: PathBuf,
    /// One `Doc` per docid, created on first use. A plain mutex map is fine:
    /// it's only touched at connect time, not per message.
    docs: Mutex<HashMap<String, Arc<Doc>>>,
}

const SAVE_DEBOUNCE: Duration = Duration::from_secs(2);

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = ccal::Config::load()?;
    let token = cfg.server_token().context(
        "no bearer token: set CCAL_SYNC_TOKEN or `token` in the config file",
    )?;
    let addr = cfg.server_addr();
    let data_dir = match cfg.server_data_dir() {
        Some(d) => d,
        None => directories::ProjectDirs::from("", "", "ccal-server")
            .context("could not determine a data directory")?
            .data_dir()
            .to_path_buf(),
    };
    std::fs::create_dir_all(&data_dir)?;

    let app = Arc::new(App {
        token,
        data_dir: data_dir.clone(),
        docs: Mutex::new(HashMap::new()),
    });

    let mut router = Router::new().route("/sync/:doc", get(sync_handler));

    // Optional embedded MCP server (opt-in via CCAL_MCP / `[server] mcp`).
    // Built before `.with_state` and nested as a state-agnostic service —
    // its handler carries the shared `Arc<Doc>` itself. Returns a
    // CancellationToken so graceful shutdown also tears the MCP sessions
    // down; `None` when disabled.
    let mcp_ct = if cfg.server_mcp_enabled() {
        let docid = cfg.server_mcp_doc();
        // Pre-resolve (and start the debounced saver for) the very doc the
        // assistant edits. A TUI later connecting to /sync/{docid} fetches
        // this same `Arc<Doc>` from the map, so an MCP mutation's
        // `dirty`/`changed` signals drive that peer's live sync — no new
        // sync code, the existing `serve_peer` path does the rest.
        let doc = doc_for(&app, &docid).await?;
        let ct = CancellationToken::new();
        // rmcp's default reaps an idle MCP session after 300s
        // (`SessionConfig::keep_alive`), after which the client's cached
        // session id 404s with "Session not found". An interactive
        // assistant routinely goes minutes between notes calls, so the
        // 5-min default fired constantly. Stretch it to 24h: still a
        // safety net against zombie sessions from silently-dropped
        // connections, but long enough that a live assistant session
        // never trips it.
        let mut sessions = LocalSessionManager::default();
        sessions.session_config.keep_alive = Some(Duration::from_secs(24 * 60 * 60));
        let svc = StreamableHttpService::new(
            move || Ok(mcp::Ccal::new(doc.clone())),
            Arc::new(sessions),
            StreamableHttpServerConfig::default()
                .with_cancellation_token(ct.child_token())
                // The bearer gate + network layer (Tailscale/TLS) is the
                // trust boundary, identical to the WS sync path which also
                // does no Host check. The MCP client is a CLI assistant,
                // not a browser, so DNS-rebinding isn't the threat; and the
                // default loopback-only host list would otherwise reject a
                // `0.0.0.0` bind reached by its Tailscale name.
                .disable_allowed_hosts(),
        );
        let tok = app.token.clone();
        let guarded = Router::new().nest_service("/mcp", svc).layer(
            // Same check as `bearer()` on the WS upgrade, before any work.
            middleware::from_fn(move |req: Request, next: Next| {
                let tok = tok.clone();
                async move {
                    let ok = req
                        .headers()
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|h| h.strip_prefix("Bearer "))
                        == Some(tok.as_str());
                    if ok {
                        next.run(req).await
                    } else {
                        (StatusCode::UNAUTHORIZED, "bad or missing bearer token")
                            .into_response()
                    }
                }
            }),
        );
        router = router.merge(guarded);
        eprintln!("ccal-server: MCP enabled at /mcp (doc `{docid}`)");
        Some(ct)
    } else {
        None
    };

    let router = router.with_state(app);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    eprintln!("ccal-server: listening on {addr}, data in {}", data_dir.display());
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            eprintln!("ccal-server: shutting down");
            if let Some(ct) = mcp_ct {
                ct.cancel();
            }
        })
        .await
        .context("server error")?;
    Ok(())
}

/// Get-or-create the `Doc` for `docid`, loading its replica from disk and
/// spawning its debounced saver the first time it's seen.
async fn doc_for(app: &Arc<App>, docid: &str) -> Result<Arc<Doc>> {
    let mut docs = app.docs.lock().await;
    if let Some(d) = docs.get(docid) {
        return Ok(d.clone());
    }
    let path = app.data_dir.join(format!("{docid}.automerge"));
    let store = Store::open_at(&path).with_context(|| format!("opening {}", path.display()))?;
    let (changed, _) = broadcast::channel(16);
    let doc = Arc::new(Doc {
        store: Mutex::new(store),
        changed,
        dirty: Notify::new(),
    });
    docs.insert(docid.to_string(), doc.clone());
    tokio::spawn(saver(doc.clone()));
    Ok(doc)
}

/// Debounced persister: wait for a change, then settle for `SAVE_DEBOUNCE`
/// (coalescing bursts) before one atomic write.
async fn saver(doc: Arc<Doc>) {
    loop {
        doc.dirty.notified().await;
        tokio::time::sleep(SAVE_DEBOUNCE).await;
        if let Err(e) = doc.store.lock().await.save() {
            eprintln!("ccal-server: save failed: {e:#}");
        }
    }
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("authorization")?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

async fn sync_handler(
    Path(docid): Path<String>,
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    // `Option<_>` so a non-upgrade request can't reject *before* the auth
    // check: token enforcement must not depend on extractor ordering.
    ws: Option<WebSocketUpgrade>,
) -> Response {
    // Reject at the handshake so the socket never opens unauthenticated.
    // Constant-ish comparison is unnecessary here (single trusted operator),
    // but keep the check before any work.
    if bearer(&headers) != Some(app.token.as_str()) {
        return (StatusCode::UNAUTHORIZED, "bad or missing bearer token").into_response();
    }
    let Some(ws) = ws else {
        return (StatusCode::BAD_REQUEST, "expected a WebSocket upgrade").into_response();
    };
    // Disallow path tricks turning into stray files outside data_dir.
    if docid.is_empty() || docid.contains(['/', '\\', '.']) {
        return (StatusCode::BAD_REQUEST, "invalid doc id").into_response();
    }
    let doc = match doc_for(&app, &docid).await {
        Ok(d) => d,
        Err(e) => {
            eprintln!("ccal-server: open {docid}: {e:#}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "could not open doc").into_response();
        }
    };
    ws.on_upgrade(move |socket| serve_peer(socket, doc))
}

/// One connected peer. Fresh `SyncState` per connection (no persisted
/// per-peer state — the protocol re-derives from have-deps on reconnect).
async fn serve_peer(mut socket: WebSocket, doc: Arc<Doc>) {
    let mut state = SyncState::new();
    let mut changed = doc.changed.subscribe();

    // Kick the exchange: send whatever this peer is missing right now.
    if flush(&mut socket, &doc, &mut state).await.is_err() {
        return;
    }

    loop {
        tokio::select! {
            // This peer sent us something.
            msg = socket.recv() => {
                let bytes = match msg {
                    Some(Ok(Message::Binary(b))) => b,
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => continue, // ping/text reserved; ignore
                    Some(Err(_)) => break,
                };
                let did_change = {
                    let mut store = doc.store.lock().await;
                    match store.receive_sync_message(&mut state, &bytes) {
                        Ok(c) => c,
                        Err(e) => { eprintln!("ccal-server: bad sync msg: {e:#}"); break; }
                    }
                };
                if did_change {
                    doc.dirty.notify_one();
                    let _ = doc.changed.send(()); // wake other peers
                }
                // Always answer: the protocol may owe this peer a reply even
                // when nothing changed (e.g. acknowledging heads).
                if flush(&mut socket, &doc, &mut state).await.is_err() {
                    break;
                }
            }
            // Some other peer advanced the doc — push the delta here too.
            r = changed.recv() => {
                if r.is_err() { continue; } // lagged: flush still catches up
                if flush(&mut socket, &doc, &mut state).await.is_err() {
                    break;
                }
            }
        }
    }
}

/// Drain every sync message the protocol currently wants to send this peer.
async fn flush(socket: &mut WebSocket, doc: &Arc<Doc>, state: &mut SyncState) -> Result<(), ()> {
    loop {
        let next = doc.store.lock().await.generate_sync_message(state);
        match next {
            Some(bytes) => socket
                .send(Message::Binary(bytes))
                .await
                .map_err(|_| ())?,
            None => return Ok(()),
        }
    }
}
