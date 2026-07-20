//! Integration tests for the Rust end of the `proxy-v1.sock` seam, driven against a
//! fake Go peer: a `tokio` `UnixListener` stands in for the control plane, accepts
//! the proxy's connection, asserts the `register` frame's shape, then pushes
//! `mint`/`kill`/`shutdown` control frames and observes the effect on the shared
//! `AppState`. A separate fail-open test proves a missing socket never panics and
//! the relay still serves standalone.

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use ccs_proxy::demux::SessionToken;
use ccs_proxy::seam::run_seam;
use ccs_proxy::{router, AppState};
use ccs_refs::RefStore;
use reqwest::Url;
use serde_json::Value;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Notify;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A unique unix-socket path under the temp dir; unix-domain paths cap at ~104
/// bytes, so this stays short. Removed if a prior run left it behind.
fn socket_path(tag: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("ccs-seam-{}-{tag}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

/// An ephemeral refs store under a process-lifetime temp dir; each call gets its
/// own db file so concurrent tests never share state.
async fn test_store() -> std::sync::Arc<RefStore> {
    use std::sync::atomic::AtomicUsize;
    use std::sync::LazyLock;

    static TEST_DIR: LazyLock<TempDir> = LazyLock::new(|| TempDir::new().expect("temp dir"));
    static DB_SEQ: AtomicUsize = AtomicUsize::new(0);

    let path = TEST_DIR.path().join(format!(
        "refs-{}.db",
        DB_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::sync::Arc::new(RefStore::open(path).await.expect("open refs db"))
}

async fn state() -> AppState {
    AppState::with_upstream(
        Url::parse("http://127.0.0.1:1").expect("upstream url"),
        test_store().await,
    )
    .expect("state")
}

/// Read one line-delimited JSON frame from the accepted peer side.
async fn read_frame(reader: &mut (impl AsyncBufReadExt + Unpin)) -> Value {
    let mut line = String::new();
    let n = reader.read_line(&mut line).await.expect("read frame");
    assert!(n > 0, "peer closed before sending a frame");
    serde_json::from_str(&line).expect("frame is JSON")
}

#[tokio::test]
async fn registers_then_applies_mint_kill_and_shutdown() {
    let path = socket_path("control");
    let listener = UnixListener::bind(&path).expect("bind seam");
    let state = state().await;
    let shutdown = Arc::new(Notify::new());

    // Drive the proxy's seam client against the fake peer.
    let client = UnixStream::connect(&path).await.expect("connect seam");
    let seam = tokio::spawn(run_seam(
        client,
        state.clone(),
        shutdown.clone(),
        54321,
        54322,
    ));

    let (peer, _) = listener.accept().await.expect("accept proxy");
    let (peer_read, mut peer_write) = peer.into_split();
    let mut peer_read = BufReader::new(peer_read);

    // 1. The register frame announces the bound port, version, and pid.
    let reg = read_frame(&mut peer_read).await;
    assert_eq!(reg["type"], "register");
    assert_eq!(reg["protocol"], 1);
    assert_eq!(reg["port"], 54321);
    assert_eq!(reg["mcp_port"], 54322);
    assert_eq!(reg["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(
        reg["pid"].as_u64().expect("pid is a number"),
        u64::from(std::process::id()),
    );

    // 2. mint registers the token under the exact protocol.
    peer_write
        .write_all(b"{\"type\":\"mint\",\"protocol\":1,\"token\":\"tok-seam\",\"config\":{}}\n")
        .await
        .expect("send mint");
    let token = SessionToken("tok-seam".to_owned());
    await_until(
        || state.sessions.contains_key(&token),
        "mint registers token",
    )
    .await;

    // 3. kill flips the panic button.
    peer_write
        .write_all(b"{\"type\":\"kill\",\"protocol\":1,\"on\":true}\n")
        .await
        .expect("send kill");
    await_until(|| state.kill.load(Ordering::Relaxed), "kill flips the flag").await;

    // 4. shutdown fires the shared notify and ends the read loop.
    peer_write
        .write_all(b"{\"type\":\"shutdown\",\"protocol\":1}\n")
        .await
        .expect("send shutdown");
    tokio::time::timeout(Duration::from_secs(2), shutdown.notified())
        .await
        .expect("shutdown notify fires");
    tokio::time::timeout(Duration::from_secs(2), seam)
        .await
        .expect("seam task ends after shutdown")
        .expect("seam task did not panic");

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn evict_removes_a_minted_session() {
    let path = socket_path("evict");
    let listener = UnixListener::bind(&path).expect("bind seam");
    let state = state().await;
    let shutdown = Arc::new(Notify::new());

    let client = UnixStream::connect(&path).await.expect("connect seam");
    tokio::spawn(run_seam(client, state.clone(), shutdown, 40000, 40010));

    let (peer, _) = listener.accept().await.expect("accept proxy");
    let (peer_read, mut peer_write) = peer.into_split();
    let mut peer_read = BufReader::new(peer_read);
    let _ = read_frame(&mut peer_read).await; // drain register

    let token = SessionToken("tok-evict".to_owned());
    peer_write
        .write_all(b"{\"type\":\"mint\",\"protocol\":1,\"token\":\"tok-evict\",\"config\":{}}\n")
        .await
        .expect("send mint");
    await_until(|| state.sessions.contains_key(&token), "mint registers").await;

    peer_write
        .write_all(b"{\"type\":\"evict\",\"protocol\":1,\"token\":\"tok-evict\"}\n")
        .await
        .expect("send evict");
    await_until(|| !state.sessions.contains_key(&token), "evict removes").await;

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn malformed_frame_ends_the_protocol_session() {
    let path = socket_path("malformed");
    let listener = UnixListener::bind(&path).expect("bind seam");
    let state = state().await;
    let shutdown = Arc::new(Notify::new());

    let client = UnixStream::connect(&path).await.expect("connect seam");
    let seam = tokio::spawn(run_seam(client, state.clone(), shutdown, 40001, 40011));

    let (peer, _) = listener.accept().await.expect("accept proxy");
    let (peer_read, mut peer_write) = peer.into_split();
    let mut peer_read = BufReader::new(peer_read);
    let _ = read_frame(&mut peer_read).await; // drain register

    // A garbage line ends the exact protocol session before a later frame can
    // mutate state.
    peer_write
        .write_all(
            b"this is not json\n{\"type\":\"mint\",\"protocol\":1,\"token\":\"after-garbage\",\"config\":{}}\n",
        )
        .await
        .expect("send frames");
    tokio::time::timeout(Duration::from_secs(2), seam)
        .await
        .expect("seam did not reject malformed frame")
        .expect("seam task panicked");
    assert!(!state
        .sessions
        .contains_key(&SessionToken("after-garbage".to_owned())));

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn absent_socket_does_not_panic_and_relay_still_serves() {
    // Mirror main()'s fail-open branch: a connect to a nonexistent socket path
    // returns Err, the seam is never spawned, and the relay serves standalone.
    let bogus = std::env::temp_dir().join(format!("ccs-seam-absent-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&bogus);
    assert!(
        UnixStream::connect(&bogus).await.is_err(),
        "connecting to a nonexistent socket must fail (and main logs + continues)",
    );

    // The relay still serves a request end to end with no seam attached.
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&upstream)
        .await;
    let proxy = spawn_standalone_proxy(&upstream.uri()).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{proxy}/v1/messages"))
        .body(b"{}".to_vec())
        .send()
        .await
        .expect("send");
    assert_eq!(
        resp.status(),
        200,
        "with no seam the relay serves standalone",
    );
}

/// Spawn the relay app against `upstream` with no seam, returning its address.
async fn spawn_standalone_proxy(upstream: &str) -> SocketAddr {
    let state = AppState::with_upstream(
        Url::parse(upstream).expect("upstream url"),
        test_store().await,
    )
    .expect("state");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let addr = listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        axum::serve(listener, router(state))
            .await
            .expect("serve proxy");
    });
    addr
}

/// Poll `cond` until it holds, bounded so a regression fails the test instead of
/// hanging. The seam applies frames on a spawned task, so the observer races the
/// writer; a short poll bridges that without a fixed sleep.
async fn await_until(mut cond: impl FnMut() -> bool, what: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !cond() {
        assert!(std::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}
