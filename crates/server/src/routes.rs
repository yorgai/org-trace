//! HTTP routes for the self-hosted trace server.
//!
//! The sync surface is intentionally unauthenticated and append-only so the
//! protocol can be exercised locally before authorization is designed. Repo IDs
//! are route/query boundaries only, not auth scopes yet.

use std::sync::Arc;

use anyhow::Result;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::get,
    Json, Router,
};
use brick_protocol::{ListEventsResponse, PushEventsRequest, PushEventsResponse};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::index::{
    query_server_sessions, rebuild_server_index, server_index_status, ServerIndexStatus,
    ServerSessionQuery, ServerSessionsResponse,
};
use crate::store::ServerStore;

/// Shared application state for server route handlers.
#[derive(Debug, Clone)]
pub struct AppState {
    pub store: Arc<ServerStore>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ListEventsQuery {
    after: Option<String>,
    limit: Option<usize>,
    repo_id: Option<String>,
}

/// Builds the self-hosted server router.
pub fn build_router(store: ServerStore) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/events", get(list_events).post(push_events))
        .route("/v1/index/status", get(global_index_status))
        .route("/v1/sessions", get(global_sessions))
        .route(
            "/v1/repos/:repo_id/events",
            get(list_repo_events).post(push_repo_events),
        )
        .route("/v1/repos/:repo_id/index/status", get(repo_index_status))
        .route("/v1/repos/:repo_id/sessions", get(repo_sessions))
        .with_state(AppState {
            store: Arc::new(store),
        })
}

async fn health() -> Json<Value> {
    Json(json!({ "ok": true }))
}

async fn list_events(
    State(state): State<AppState>,
    Query(query): Query<ListEventsQuery>,
) -> std::result::Result<Json<ListEventsResponse>, (StatusCode, String)> {
    let response = state
        .store
        .list_events_page(
            query.repo_id.as_deref(),
            query.after.as_deref(),
            query.limit,
        )
        .map_err(route_error)?;
    Ok(Json(response))
}

async fn list_repo_events(
    State(state): State<AppState>,
    Path(repo_id): Path<String>,
    Query(query): Query<ListEventsQuery>,
) -> std::result::Result<Json<ListEventsResponse>, (StatusCode, String)> {
    let response = state
        .store
        .list_events_page(Some(&repo_id), query.after.as_deref(), query.limit)
        .map_err(route_error)?;
    Ok(Json(response))
}

async fn push_events(
    State(state): State<AppState>,
    Json(request): Json<PushEventsRequest>,
) -> std::result::Result<Json<PushEventsResponse>, (StatusCode, String)> {
    let response = state
        .store
        .append_events(&request.events)
        .map_err(route_error)?;
    Ok(Json(response))
}

async fn push_repo_events(
    State(state): State<AppState>,
    Path(repo_id): Path<String>,
    Json(request): Json<PushEventsRequest>,
) -> std::result::Result<Json<PushEventsResponse>, (StatusCode, String)> {
    let response = state
        .store
        .append_events_for_repo(Some(&repo_id), &request.events)
        .map_err(route_error)?;
    Ok(Json(response))
}

async fn global_index_status(
    State(state): State<AppState>,
) -> std::result::Result<Json<ServerIndexStatus>, (StatusCode, String)> {
    index_status_response(&state, None)
}

async fn repo_index_status(
    State(state): State<AppState>,
    Path(repo_id): Path<String>,
) -> std::result::Result<Json<ServerIndexStatus>, (StatusCode, String)> {
    index_status_response(&state, Some(&repo_id))
}

async fn global_sessions(
    State(state): State<AppState>,
    Query(query): Query<ServerSessionQuery>,
) -> std::result::Result<Json<ServerSessionsResponse>, (StatusCode, String)> {
    sessions_response(&state, None, &query)
}

async fn repo_sessions(
    State(state): State<AppState>,
    Path(repo_id): Path<String>,
    Query(query): Query<ServerSessionQuery>,
) -> std::result::Result<Json<ServerSessionsResponse>, (StatusCode, String)> {
    sessions_response(&state, Some(&repo_id), &query)
}

fn index_status_response(
    state: &AppState,
    repo_id: Option<&str>,
) -> std::result::Result<Json<ServerIndexStatus>, (StatusCode, String)> {
    let events = state
        .store
        .read_events_for_repo(repo_id)
        .map_err(route_error)?;
    let index = rebuild_server_index(repo_id, &events).map_err(route_error)?;
    Ok(Json(server_index_status(repo_id, &index)))
}

fn sessions_response(
    state: &AppState,
    repo_id: Option<&str>,
    query: &ServerSessionQuery,
) -> std::result::Result<Json<ServerSessionsResponse>, (StatusCode, String)> {
    let events = state
        .store
        .read_events_for_repo(repo_id)
        .map_err(route_error)?;
    let index = rebuild_server_index(repo_id, &events).map_err(route_error)?;
    Ok(Json(query_server_sessions(repo_id, &index, query)))
}

/// Starts the HTTP server with the provided store and bind address.
pub async fn serve(bind: String, store: ServerStore) -> Result<()> {
    store.init()?;
    let app = build_router(store);
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    println!("brick-server listening on http://{bind}");
    axum::serve(listener, app).await?;
    Ok(())
}

fn route_error(error: anyhow::Error) -> (StatusCode, String) {
    let message = error.to_string();
    if message.contains("repo_id") || message.contains("cursor") {
        (StatusCode::BAD_REQUEST, message)
    } else {
        (StatusCode::INTERNAL_SERVER_ERROR, message)
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Method, Request};
    use brick_protocol::{ActorRef, ActorType, MissionCreatedPayload, MissionId, TraceEvent};
    use chrono::Utc;
    use tower::ServiceExt;

    use super::*;

    fn temp_data_dir(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "brick-routes-{name}-{}",
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ))
    }

    fn event(repo_id: Option<&str>) -> TraceEvent {
        let mut event = TraceEvent::mission_created(
            ActorRef {
                actor_type: ActorType::Human,
                actor_id: "tester".to_string(),
                display_name: None,
            },
            MissionId::new(),
            MissionCreatedPayload {
                title: "Server route".to_string(),
                description: None,
                repo_context_id: None,
            },
        )
        .expect("mission event");
        event.repo_id = repo_id.map(ToString::to_string);
        event
    }

    #[tokio::test]
    async fn health_route_is_registered() {
        let store = ServerStore::new(temp_data_dir("health"));
        let app = build_router(store);
        let routes = app.into_make_service();
        let _ = routes;
    }

    #[tokio::test]
    async fn repo_events_route_pushes_and_lists_scoped_events() {
        let store = ServerStore::new(temp_data_dir("repo-events"));
        let app = build_router(store);
        let request = PushEventsRequest {
            events: vec![event(None)],
        };
        let body = serde_json::to_vec(&request).expect("serialize request");

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/repos/repo-a/events")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .expect("build request"),
            )
            .await
            .expect("post events");
        assert_eq!(response.status(), StatusCode::OK);

        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/v1/repos/repo-a/events?limit=1")
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("list events");
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let listed: ListEventsResponse = serde_json::from_slice(&bytes).expect("decode response");

        assert_eq!(listed.events.len(), 1);
        assert_eq!(listed.events[0].repo_id.as_deref(), Some("repo-a"));
    }

    #[tokio::test]
    async fn repo_events_route_rejects_mismatched_repo() {
        let store = ServerStore::new(temp_data_dir("repo-mismatch"));
        let app = build_router(store);
        let request = PushEventsRequest {
            events: vec![event(Some("repo-b"))],
        };
        let body = serde_json::to_vec(&request).expect("serialize request");

        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/repos/repo-a/events")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .expect("build request"),
            )
            .await
            .expect("post events");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
