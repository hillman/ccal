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
//! Config (env):
//!   CCAL_SYNC_TOKEN  required — shared bearer token
//!   CCAL_SYNC_ADDR   listen address           (default 127.0.0.1:8787)
//!   CCAL_SYNC_DATA   directory for {docid}.automerge replicas
//!                    (default: OS data dir / ccal-server)

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use ccal::{Store, SyncState};
use tokio::sync::{broadcast, Mutex, Notify};

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
    let token = std::env::var("CCAL_SYNC_TOKEN")
        .context("CCAL_SYNC_TOKEN must be set (shared bearer token)")?;
    if token.trim().is_empty() {
        anyhow::bail!("CCAL_SYNC_TOKEN must not be empty");
    }
    let addr = std::env::var("CCAL_SYNC_ADDR").unwrap_or_else(|_| "127.0.0.1:8787".into());
    let data_dir = match std::env::var_os("CCAL_SYNC_DATA") {
        Some(d) => PathBuf::from(d),
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

    let router = Router::new()
        .route("/sync/:doc", get(sync_handler))
        .with_state(app);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    eprintln!("ccal-server: listening on {addr}, data in {}", data_dir.display());
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            eprintln!("ccal-server: shutting down");
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
