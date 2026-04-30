//! `GET /healthz` — liveness / readiness probe.
//!
//! Returns `200 OK` with a JSON body. Railway and load balancers poll this to
//! determine whether the instance is alive.

use axum::{Json, response::IntoResponse};
use serde::Serialize;

/// Response body for `GET /healthz`.
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub version: &'static str,
}

/// Handler for `GET /healthz`.
///
/// Always returns `200 OK` while the process is alive. A future issue can
/// extend this to check downstream dependencies (e.g., Postgres connectivity).
pub async fn healthz() -> impl IntoResponse {
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;

    #[tokio::test]
    async fn healthz_returns_200() {
        let app = Router::new().route("/healthz", axum::routing::get(healthz));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
