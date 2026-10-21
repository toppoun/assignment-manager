//! Authenticated, synthetic-data-only, IPv4 loopback HTTP PoC.
#![forbid(unsafe_code)]
mod assets;
pub mod auth;
mod dto;
mod error;
pub mod poc;
mod requests;
mod routes;
mod state;

pub use error::WebError;
pub use routes::router;
pub use state::{Clock, PocState, SystemClock};

use poc::PocDatabase;
use std::{
    future::Future,
    net::{Ipv4Addr, SocketAddr},
};
use tokio::net::TcpListener;

pub const DEFAULT_PORT: u16 = 3000;

pub struct PocServer {
    listener: TcpListener,
    state: PocState,
    address: SocketAddr,
}
impl PocServer {
    /// Readiness completes before a socket is bound. No IP/bind option exists.
    pub async fn start(
        database: &PocDatabase,
        clock: impl Clock,
        port: u16,
    ) -> Result<Self, WebError> {
        let state = PocState::open(database, clock).await?;
        let listener = match TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await {
            Ok(listener) => listener,
            Err(_) => {
                state.shutdown().await;
                return Err(WebError::Unavailable);
            }
        };
        let address = match listener.local_addr() {
            Ok(address) => address,
            Err(_) => {
                state.shutdown().await;
                return Err(WebError::Unavailable);
            }
        };
        Ok(Self {
            listener,
            state,
            address,
        })
    }
    pub fn local_addr(&self) -> SocketAddr {
        self.address
    }
    pub async fn serve(
        self,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> Result<(), WebError> {
        let state = self.state;
        let admission = state.clone();
        let result = axum::serve(self.listener, router(state.clone(), self.address.port()))
            .with_graceful_shutdown(async move {
                shutdown.await;
                admission.begin_shutdown();
            })
            .await;
        // axum has drained HTTP requests before worker admission/resources close.
        state.shutdown().await;
        result.map_err(|_| WebError::Unavailable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        sync::Notify,
    };

    // Only the test clock blocks: a deterministic barrier holds one admitted
    // request before application enqueue, exposing incorrect shutdown ordering.
    struct PausedClock {
        calls: AtomicUsize,
        entered: Arc<Notify>,
        resume: Mutex<mpsc::Receiver<()>>,
    }
    impl Clock for PausedClock {
        fn now(&self) -> DateTime<Utc> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 1 {
                self.entered.notify_one();
                self.resume
                    .lock()
                    .unwrap()
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap();
            }
            "2026-10-21T15:00:00.123456789Z".parse().unwrap()
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn graceful_stop_closes_admission_then_drains_http_before_workers() {
        use axum::{body::Body, http::Request};
        use tower::ServiceExt;
        let root = tempfile::tempdir().unwrap();
        let db = PocDatabase::prepare(&root.path().join("poc")).unwrap();
        crate::auth::bootstrap_admin(&db, "owner", "synthetic-unit-password-3A".to_owned().into())
            .unwrap();
        let entered = Arc::new(Notify::new());
        let (resume, paused) = mpsc::channel();
        let server = PocServer::start(
            &db,
            PausedClock {
                calls: AtomicUsize::new(0),
                entered: Arc::clone(&entered),
                resume: Mutex::new(paused),
            },
            0,
        )
        .await
        .unwrap();
        let state = server.state.clone();
        let (token, session) = state
            .auth()
            .login(
                "owner".into(),
                "synthetic-unit-password-3A".to_owned().into(),
                None,
            )
            .await
            .unwrap();
        let address = server.local_addr();
        let (stop, signal) = tokio::sync::oneshot::channel();
        let serving = tokio::spawn(server.serve(async {
            let _ = signal.await;
        }));
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        client.write_all(format!("GET /api/v1/assignments HTTP/1.1\r\nHost: {address}\r\nCookie: {}={token}\r\nConnection: close\r\n\r\n", crate::auth::COOKIE_NAME).as_bytes()).await.unwrap();
        entered.notified().await;
        stop.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while state.accepting() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let rejected = router(state.clone(), address.port())
            .oneshot(
                Request::builder()
                    .uri("/api/v1/assignments")
                    .header("host", address.to_string())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            rejected.status(),
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        assert!(!serving.is_finished());
        resume.send(()).unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.read_to_end(&mut response),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(response.starts_with(b"HTTP/1.1 200 OK"));
        tokio::time::timeout(std::time::Duration::from_secs(5), serving)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(
            state.query(&session.user_id, Default::default()).await,
            Err(WebError::Unavailable)
        ));
    }
}
