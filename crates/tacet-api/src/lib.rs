//! HTTP API for media servers to query / trigger detection.
//!
//! Endpoints:
//! - `GET  /api/v1/health`
//! - `POST /api/v1/detect` — body: `{"episode_id","file_path","series","season"}`
//! - `GET  /api/v1/series/{series}/season/{season}/markers`
//! - `GET  /api/v1/episodes/{episode_id}/markers`

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tower_http::trace::TraceLayer;

use tacet::detection;
use tacet::storage::{FingerprintKind, Store};
use tacet::{Config, SegmentMarkers};

pub struct AppState {
    pub store: Store,
    pub config: Config,
}

pub type SharedState = Arc<AppState>;

pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/api/v1/health", get(health))
        .route("/api/v1/detect", post(detect_episode))
        .route(
            "/api/v1/series/:series/season/:season/markers",
            get(get_season_markers),
        )
        .route("/api/v1/episodes/:episode_id/markers", get(get_episode_markers))
        .with_state(state)
        .layer(TraceLayer::new_for_http())
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
    version: &'static str,
}

async fn health() -> Json<Health> {
    Json(Health {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
    })
}

#[derive(Deserialize)]
struct DetectRequest {
    episode_id: String,
    file_path: PathBuf,
    series: String,
    season: u32,
}

#[derive(Serialize)]
struct DetectResponse {
    markers: SegmentMarkers,
    /// True if the season has been bootstrapped and the markers reflect a real match.
    /// False means no reference exists yet — call `/scan` for the season first.
    reference_available: bool,
}

async fn detect_episode(
    State(state): State<SharedState>,
    Json(req): Json<DetectRequest>,
) -> Result<Json<DetectResponse>, ApiError> {
    let intro_refs = state
        .store
        .load_references(&req.series, req.season, FingerprintKind::Intro)?;
    let credits_refs = state
        .store
        .load_references(&req.series, req.season, FingerprintKind::Credits)?;

    let reference_available = !intro_refs.is_empty() || !credits_refs.is_empty();
    let markers = if reference_available {
        detection::detect_single_episode(
            &req.file_path,
            &req.episode_id,
            &intro_refs,
            &credits_refs,
            &state.config,
        )?
    } else {
        SegmentMarkers {
            episode_id: req.episode_id.clone(),
            intro: None,
            credits: None,
        }
    };

    state.store.save_markers(&markers)?;
    Ok(Json(DetectResponse {
        markers,
        reference_available,
    }))
}

async fn get_season_markers(
    State(state): State<SharedState>,
    Path((series, season)): Path<(String, u32)>,
) -> Result<Json<Vec<SegmentMarkers>>, ApiError> {
    let m = state.store.get_season_markers(&series, season)?;
    Ok(Json(m))
}

async fn get_episode_markers(
    State(state): State<SharedState>,
    Path(episode_id): Path<String>,
) -> Result<Json<Option<SegmentMarkers>>, ApiError> {
    let m = state.store.get_markers(&episode_id)?;
    Ok(Json(m))
}

pub struct ApiError(anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(err: E) -> Self {
        ApiError(err.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let body = serde_json::json!({ "error": self.0.to_string() });
        (StatusCode::INTERNAL_SERVER_ERROR, Json(body)).into_response()
    }
}
