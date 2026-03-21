//! Optional HTTP server for health checks and Prometheus metrics.
//!
//! Enabled with the `metrics` feature flag and the `--http-port` CLI flag.

#[cfg(feature = "metrics")]
pub mod server {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::get;
    use tracing::{info, warn};

    use crate::storage::Storage;

    /// Shared state for HTTP handlers.
    #[derive(Clone)]
    pub struct HttpState {
        pub engine: Arc<Mutex<murmur_engine::MurmurEngine>>,
        pub storage: Arc<Storage>,
        pub connected_peers: Arc<AtomicU64>,
        pub start_time: std::time::Instant,
    }

    /// Start the HTTP server on the given port.
    pub async fn start_http_server(state: HttpState, port: u16) {
        let app = Router::new()
            .route("/health", get(health_handler))
            .route("/metrics", get(metrics_handler))
            .with_state(state);

        let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
        info!(%addr, "HTTP server starting");

        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                warn!(error = %e, "failed to bind HTTP server");
                return;
            }
        };

        if let Err(e) = axum::serve(listener, app).await {
            warn!(error = %e, "HTTP server error");
        }
    }

    /// GET /health — JSON health status.
    async fn health_handler(State(state): State<HttpState>) -> impl IntoResponse {
        let peer_count = state.connected_peers.load(Ordering::Relaxed);
        let uptime_secs = state.start_time.elapsed().as_secs();

        let (dag_entries, pending_blobs) = {
            let eng = state.engine.lock().unwrap();
            let dag_count = eng.all_entries().len();
            let pending = state
                .storage
                .push_queue_items()
                .map(|v| v.len())
                .unwrap_or(0);
            (dag_count, pending)
        };

        let body = serde_json::json!({
            "status": "ok",
            "peer_count": peer_count,
            "dag_entries": dag_entries,
            "pending_blobs": pending_blobs,
            "uptime_secs": uptime_secs,
        });

        (
            StatusCode::OK,
            [("content-type", "application/json")],
            body.to_string(),
        )
    }

    /// GET /metrics — Prometheus text format.
    async fn metrics_handler() -> impl IntoResponse {
        let body = crate::metrics::encode_metrics();
        (
            StatusCode::OK,
            [("content-type", "text/plain; charset=utf-8")],
            body,
        )
    }
}
