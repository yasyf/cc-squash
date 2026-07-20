//! Property test: arbitrary request bodies forward byte-for-byte. The framing
//! bug was fundamentally "some bytes/lengths don't survive forwarding", so this
//! generalises the exact-body regression across sizes and contents.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::OnceLock;

use ccs_proxy::{router, AppState};
use ccs_refs::RefStore;
use proptest::prelude::*;
use reqwest::Url;
use tempfile::TempDir;
use tokio::runtime::Runtime;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct Harness {
    rt: Runtime,
    upstream: MockServer,
    proxy: SocketAddr,
    _refs_dir: TempDir,
}

fn harness() -> &'static Harness {
    static HARNESS: OnceLock<Harness> = OnceLock::new();
    HARNESS.get_or_init(|| {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let refs_dir = TempDir::new().expect("temp dir");
        let (upstream, proxy) = rt.block_on(async {
            let upstream = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .respond_with(ResponseTemplate::new(200))
                .mount(&upstream)
                .await;
            let store = Arc::new(
                RefStore::open(refs_dir.path().join("refs-v1.db"))
                    .await
                    .expect("open refs db"),
            );
            let state = AppState::with_upstream(Url::parse(&upstream.uri()).expect("url"), store)
                .expect("state");
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let proxy = listener.local_addr().expect("addr");
            tokio::spawn(async move {
                axum::serve(listener, router(state)).await.expect("serve");
            });
            (upstream, proxy)
        });
        Harness {
            rt,
            upstream,
            proxy,
            _refs_dir: refs_dir,
        }
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn forward_preserves_arbitrary_bodies(body in proptest::collection::vec(any::<u8>(), 0..8192)) {
        let h = harness();
        let received = h.rt.block_on(async {
            let resp = reqwest::Client::new()
                .post(format!("http://{}/v1/messages", h.proxy))
                .body(body.clone())
                .send()
                .await
                .expect("send");
            assert_eq!(resp.status(), 200);
            // Cases run sequentially, so the last recorded request is this one.
            h.upstream
                .received_requests()
                .await
                .expect("recorded")
                .pop()
                .expect("at least one request")
                .body
        });
        prop_assert_eq!(received, body);
    }
}
