//! Contract test for the *blocking* transport the TUI's `sync_client` uses
//! (plain `ws://`, bearer handshake, non-blocking socket, WouldBlock-driven
//! pump) against the real `ccal-server`. `sync_client.rs` is binary-private,
//! so this mirrors its transport choices rather than calling it — if these
//! tungstenite assumptions break, the TUI's background sync breaks.

use std::io::ErrorKind;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use ccal::{Store, SyncState};
use tungstenite::client::IntoClientRequest;
use tungstenite::{Error as WsError, Message};

const TOKEN: &str = "transport-secret";

struct Guard(Child);
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn tmpdir() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let p = std::env::temp_dir().join(format!(
        "ccal-tx-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn start_server(addr: &str) -> Guard {
    let child = Command::new(env!("CARGO_BIN_EXE_ccal-server"))
        .env("CCAL_SYNC_TOKEN", TOKEN)
        .env("CCAL_SYNC_ADDR", addr)
        .env("CCAL_SYNC_DATA", tmpdir())
        .spawn()
        .expect("spawn ccal-server");
    for _ in 0..50 {
        if std::net::TcpStream::connect(addr).is_ok() {
            return Guard(child);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("server did not come up");
}

type Ws = tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>;

/// Connect exactly like `sync_client::session`: bearer header, then flip the
/// plain socket non-blocking.
fn connect(addr: &str) -> Ws {
    let mut req = format!("ws://{addr}/sync/ccal")
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {TOKEN}").parse().unwrap());
    let (ws, _) = tungstenite::connect(req).expect("handshake");
    match ws.get_ref() {
        tungstenite::stream::MaybeTlsStream::Plain(s) => s.set_nonblocking(true).unwrap(),
        _ => panic!("expected a plain ws:// stream"),
    }
    ws
}

fn would_block(e: &WsError) -> bool {
    matches!(e, WsError::Io(io)
        if io.kind() == ErrorKind::WouldBlock || io.kind() == ErrorKind::TimedOut)
}

/// Drive one replica to quiescence over a non-blocking socket, the way the
/// TUI thread does: flush all outgoing, then absorb incoming until a whole
/// idle period passes with no traffic.
fn drive(ws: &mut Ws, store: &mut Store, state: &mut SyncState) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut idle_since: Option<Instant> = None;
    loop {
        let mut activity = false;
        while let Some(bytes) = store.generate_sync_message(state) {
            ws.write(Message::Binary(bytes)).unwrap();
            activity = true;
        }
        match ws.flush() {
            Ok(()) => {}
            Err(e) if would_block(&e) => {}
            Err(e) => panic!("flush: {e}"),
        }
        match ws.read() {
            Ok(Message::Binary(b)) => {
                if store.receive_sync_message(state, &b).unwrap() {
                    store.save().unwrap();
                }
                activity = true;
            }
            Ok(_) => {}
            Err(e) if would_block(&e) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => panic!("read: {e}"),
        }
        if activity {
            idle_since = None;
        } else {
            let since = *idle_since.get_or_insert_with(Instant::now);
            if since.elapsed() > Duration::from_millis(600) {
                return;
            }
        }
        assert!(Instant::now() < deadline, "did not converge in time");
    }
}

#[test]
fn blocking_client_converges_through_server() {
    let addr = "127.0.0.1:8793";
    let _srv = start_server(addr);
    let dir = tmpdir();

    let mut a = Store::open_at(dir.join("a.automerge")).unwrap();
    let id = a.create_note(&["inbox".into()], "blocking client works").unwrap();
    a.set_note_body(&id, "body via tungstenite").unwrap();

    let mut wa = connect(addr);
    let mut sa = SyncState::new();
    drive(&mut wa, &mut a, &mut sa);

    let mut b = Store::open_at(dir.join("b.automerge")).unwrap();
    let mut wb = connect(addr);
    let mut sb = SyncState::new();
    drive(&mut wb, &mut b, &mut sb);

    let n = b.note(&id).expect("note reached B over blocking ws");
    assert_eq!(n.title, "blocking client works");
    assert_eq!(n.body, "body via tungstenite");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn blocking_client_rejected_without_token() {
    let addr = "127.0.0.1:8794";
    let _srv = start_server(addr);
    let req = format!("ws://{addr}/sync/ccal")
        .into_client_request()
        .unwrap();
    assert!(
        tungstenite::connect(req).is_err(),
        "handshake must fail without a bearer token"
    );
}
