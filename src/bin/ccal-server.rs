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
//!   - the bearer token is checked at the upgrade; a wrong/absent token is
//!     rejected before the socket opens — no in-band auth frame. Three
//!     equivalent ways to present it (any one suffices):
//!       * `Authorization: Bearer <token>` — the TUI / automerge-swift path,
//!         unchanged.
//!       * `Sec-WebSocket-Protocol: ccal.bearer.<token>` — for browsers,
//!         which cannot set `Authorization` on a `WebSocket`. Preferred over
//!         the query form: it stays out of access logs. Echoed back per spec.
//!       * `?token=<token>` query param — documented fallback for clients
//!         that can't set a subprotocol (token must be URL-safe).
//!   - every binary frame is raw `automerge::sync::Message` bytes, nothing
//!     else; text/ping frames are ignored (reserved for future control)
//!
//! With the `web` cargo feature, a static-asset fallback also serves the
//! built PWA shell (`CCAL_WEB_DIR`, default `web/dist`) — see
//! `docs/plans/web-interface.md`. It never shadows `/sync` or `/mcp` (it is
//! the router *fallback*) and is unauthenticated: it is only the app shell;
//! all data still flows through the bearer-gated sync socket above.
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
    extract::{Path, RawQuery, Request, State},
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

    // Optional static-asset fallback serving the built PWA shell. Mounted as
    // the router *fallback* so `/sync/:doc` and `/mcp` always win; absent the
    // `web` feature this code (and the dependency-free handler) is gone.
    #[cfg(feature = "web")]
    let router = {
        match std::env::var("CCAL_WEB_DIR") {
            Ok(dir) => eprintln!("ccal-server: serving web app from {dir} (disk, router fallback)"),
            Err(_) => eprintln!("ccal-server: serving embedded web app (router fallback)"),
        }
        router.fallback(web_asset)
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
    // Auto-migrate a pre-line-body (schema 1) replica before opening it. The
    // server is authoritative: it re-genesises in place to shed the old
    // per-character `Text` op history (keeping a `.v1.bak`); clients detect
    // the bump and discard + re-sync rather than push their old history back.
    match Store::migrate_v1_in_place(&path) {
        Ok(true) => eprintln!(
            "ccal-server: migrated `{docid}` to the line-based body schema \
             (backup: {docid}.automerge.v1.bak)"
        ),
        Ok(false) => {}
        Err(e) => eprintln!("ccal-server: migration check for `{docid}` failed: {e:#} (continuing)"),
    }
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

/// A bearer token offered via the WebSocket subprotocol header as
/// `ccal.bearer.<token>` — the browser auth path (browsers can set a
/// subprotocol but not `Authorization`, and it stays out of access logs
/// unlike a query string). Returns `(full_protocol, token)` so the caller can
/// echo the exact protocol back per the WS spec. The header is a
/// comma-separated list; the first matching entry wins.
fn subprotocol_token(headers: &HeaderMap) -> Option<(&str, &str)> {
    headers
        .get("sec-websocket-protocol")?
        .to_str()
        .ok()?
        .split(',')
        .map(str::trim)
        .find_map(|p| p.strip_prefix("ccal.bearer.").map(|t| (p, t)))
}

/// A bearer token offered as a `?token=<token>` query param — the documented
/// fallback for clients that can set neither header nor subprotocol. The
/// value is taken raw (tokens are expected URL-safe).
fn query_token(query: Option<&str>) -> Option<&str> {
    query?.split('&').find_map(|kv| kv.strip_prefix("token="))
}

async fn sync_handler(
    Path(docid): Path<String>,
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    // `Option<_>` so a non-upgrade request can't reject *before* the auth
    // check: token enforcement must not depend on extractor ordering.
    ws: Option<WebSocketUpgrade>,
) -> Response {
    // Reject at the handshake so the socket never opens unauthenticated.
    // Constant-ish comparison is unnecessary here (single trusted operator),
    // but keep the check before any work. The token may arrive via header
    // (TUI), subprotocol (browser) or query (fallback) — any one suffices.
    let want = app.token.as_str();
    let subproto = subprotocol_token(&headers);
    let authed = bearer(&headers) == Some(want)
        || subproto.map(|(_, t)| t) == Some(want)
        || query_token(query.as_deref()) == Some(want);
    if !authed {
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
    // When the client authed via subprotocol, echo that exact protocol back
    // (per RFC 6455) so the browser's negotiated `WebSocket.protocol` is set.
    let ws = match subproto {
        Some((full, t)) if t == want => ws.protocols([full.to_string()]),
        _ => ws,
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

/// The PWA shell, embedded into the binary at build time so a release is a
/// single self-contained artifact. `CCAL_WEB_DIR` overrides this at runtime to
/// serve from disk instead (dev iteration without a rebuild).
#[cfg(feature = "web")]
#[derive(rust_embed::RustEmbed)]
#[folder = "web/dist"]
struct WebAssets;

/// Static-asset fallback serving the built PWA shell. Unknown paths fall back
/// to `index.html` so the SPA's client-side routing works. Unauthenticated by
/// design — only the app shell; all data flows through the bearer-gated sync
/// socket. Embedded by default; disk-served when `CCAL_WEB_DIR` is set.
#[cfg(feature = "web")]
async fn web_asset(uri: axum::http::Uri) -> Response {
    let req = uri.path().trim_start_matches('/');
    if let Some(dir) = std::env::var_os("CCAL_WEB_DIR") {
        return web_asset_disk(std::path::Path::new(&dir), req).await;
    }
    // Embedded: the requested file, else the SPA shell.
    let (name, file) = match WebAssets::get(req) {
        Some(f) if !req.is_empty() => (req, f),
        _ => match WebAssets::get("index.html") {
            Some(f) => ("index.html", f),
            None => return (StatusCode::NOT_FOUND, "web app not embedded").into_response(),
        },
    };
    (
        [(axum::http::header::CONTENT_TYPE, web_content_type(std::path::Path::new(name)))],
        file.data.into_owned(),
    )
        .into_response()
}

/// Disk variant of [`web_asset`] used when `CCAL_WEB_DIR` is set.
#[cfg(feature = "web")]
async fn web_asset_disk(root: &std::path::Path, req: &str) -> Response {
    use std::path::{Component, Path as FsPath};
    // Only normal path components — no traversal out of the web dir.
    let safe = !req.is_empty()
        && FsPath::new(req).components().all(|c| matches!(c, Component::Normal(_)));
    let candidate = if safe { root.join(req) } else { root.to_path_buf() };
    let path = if candidate.is_file() { candidate } else { root.join("index.html") };
    match tokio::fs::read(&path).await {
        Ok(bytes) => (
            [(axum::http::header::CONTENT_TYPE, web_content_type(&path))],
            bytes,
        )
            .into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "web app not built (web/dist missing)").into_response(),
    }
}

#[cfg(feature = "web")]
fn web_content_type(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("webmanifest") => "application/manifest+json",
        Some("wasm") => "application/wasm",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}
