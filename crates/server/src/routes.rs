//! HTTP routes for the self-hosted trace server.
//!
//! The sync surface is intentionally unauthenticated and append-only so the
//! protocol can be exercised locally before authorization is designed. Repo IDs
//! are route/query boundaries only, not auth scopes yet.

use std::{path::PathBuf, process::Stdio, sync::Arc};

use anyhow::Result;
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use brick_protocol::{ListEventsResponse, PushEventsRequest, PushEventsResponse};
use serde::Deserialize;
use serde_json::{json, Value};
use std::process::Command;

use crate::auth::{self, AuditLog, TokenStore};
use crate::index::{
    query_server_sessions, rebuild_server_index, server_index_status, ServerIndexStatus,
    ServerSessionQuery, ServerSessionsResponse,
};
use crate::store::ServerStore;

/// Shared application state for server route handlers.
#[derive(Debug, Clone)]
pub struct AppState {
    pub store: Arc<ServerStore>,
    pub local_history: Option<LocalHistoryBridge>,
}

/// Optional token gate. When `Some`, every route except `/health` requires a
/// bearer token from the table, and the token's scope + access must cover the
/// requested resource. When `None`, the server stays open (append-only MVP).
#[derive(Clone)]
pub struct AuthConfig {
    tokens: Arc<TokenStore>,
    audit: Arc<AuditLog>,
}

impl AuthConfig {
    pub fn new(tokens: TokenStore, audit: AuditLog) -> Self {
        Self {
            tokens: Arc::new(tokens),
            audit: Arc::new(audit),
        }
    }
}

#[derive(Debug, Clone)]
pub struct LocalHistoryBridge {
    brick_bin: Arc<PathBuf>,
    repo_root: Option<Arc<PathBuf>>,
}

impl LocalHistoryBridge {
    pub fn new(brick_bin: PathBuf, repo_root: Option<PathBuf>) -> Self {
        Self {
            brick_bin: Arc::new(brick_bin),
            repo_root: repo_root.map(Arc::new),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ListEventsQuery {
    after: Option<String>,
    limit: Option<usize>,
    repo_id: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct HistorySourceQuery {
    source: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct HistoryPageQuery {
    limit: Option<usize>,
    offset: Option<usize>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct HistoryExportQuery {
    schema: Option<String>,
    format: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct SourceIndexRequest {
    sources: Vec<String>,
}

/// Builds the self-hosted server router.
pub fn build_router(
    store: ServerStore,
    local_history: Option<LocalHistoryBridge>,
    auth: Option<AuthConfig>,
) -> Router {
    let state = AppState {
        store: Arc::new(store),
        local_history,
    };
    let protected = Router::new()
        .route("/v1/events", get(list_events).post(push_events))
        .route("/v1/index/status", get(global_index_status))
        .route("/v1/sessions", get(global_sessions))
        .route("/v1/local-history/sources", get(local_history_sources))
        .route(
            "/v1/local-history/source-detection",
            get(local_source_detection).post(local_source_index_selected),
        )
        .route("/v1/local-history/doctor", get(local_history_doctor))
        .route(
            "/v1/local-history/sources/:source/refresh",
            post(local_history_refresh),
        )
        .route(
            "/v1/local-history/sources/:source/sessions",
            get(local_history_sessions),
        )
        .route(
            "/v1/local-history/sources/:source/sessions/:session_id/export",
            get(local_history_export),
        )
        .route(
            "/v1/repos/:repo_id/events",
            get(list_repo_events).post(push_repo_events),
        )
        .route("/v1/repos/:repo_id/index/status", get(repo_index_status))
        .route("/v1/repos/:repo_id/sessions", get(repo_sessions));
    let protected = if let Some(auth) = auth {
        protected.layer(axum::middleware::from_fn_with_state(
            auth,
            require_bearer_token,
        ))
    } else {
        protected
    };
    // `/health` is always reachable so liveness probes work without a token.
    Router::new()
        .route("/health", get(health))
        .merge(protected)
        .with_state(state)
}

/// Rejects requests whose bearer token is unknown (401) or lacks scope/access
/// for the requested resource (403).
async fn require_bearer_token(
    State(auth): State<AuthConfig>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let Some(token) = presented else {
        return unauthorized();
    };
    let target = auth::resource_target_for_path(request.uri().path());
    let required = auth::required_access(request.method());
    match auth.tokens.authorize(token, &target, required) {
        Ok(label) => {
            if required == auth::Access::Write {
                auth.audit.record(&auth::AuditEntry {
                    at: chrono::Utc::now(),
                    token_label: label,
                    method: request.method().to_string(),
                    path: request.uri().path().to_string(),
                });
            }
            next.run(request).await
        }
        Err(auth::AuthDenial::UnknownToken | auth::AuthDenial::Expired) => unauthorized(),
        Err(auth::AuthDenial::Forbidden) => forbidden(),
    }
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "missing or invalid bearer token" })),
    )
        .into_response()
}

fn forbidden() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({ "error": "token not permitted for this resource" })),
    )
        .into_response()
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

async fn local_history_sources(
    State(state): State<AppState>,
) -> std::result::Result<Json<Value>, (StatusCode, String)> {
    run_history_json(&state, &["sources", "--format", "json"]).await
}

async fn local_source_detection(
    State(state): State<AppState>,
) -> std::result::Result<Json<Value>, (StatusCode, String)> {
    run_source_json(&state, &["scan", "--format", "json"]).await
}

async fn local_source_index_selected(
    State(state): State<AppState>,
    Json(request): Json<SourceIndexRequest>,
) -> std::result::Result<Json<Value>, (StatusCode, String)> {
    let mut args = vec![
        "scan".to_string(),
        "--write-defaults".to_string(),
        "--format".to_string(),
        "json".to_string(),
    ];
    for source in normalized_source_list(&request.sources)? {
        args.push("--include".to_string());
        args.push(source);
    }
    let arg_refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    run_source_json(&state, &arg_refs).await
}

async fn local_history_doctor(
    State(state): State<AppState>,
    Query(query): Query<HistorySourceQuery>,
) -> std::result::Result<Json<Value>, (StatusCode, String)> {
    let source = normalized_source(query.source.as_deref());
    run_history_json(&state, &["doctor", "--source", &source, "--format", "json"]).await
}

async fn local_history_refresh(
    State(state): State<AppState>,
    Path(source): Path<String>,
    Query(query): Query<HistoryPageQuery>,
) -> std::result::Result<Json<Value>, (StatusCode, String)> {
    let source = normalized_source(Some(&source));
    let limit = normalized_history_limit(query.limit, 100);
    let limit_text = limit.to_string();
    let sessions = run_history_value(
        &state,
        &[
            "sessions",
            "--source",
            &source,
            "--limit",
            &limit_text,
            "--offset",
            "0",
            "--format",
            "json",
        ],
    )
    .await?;
    let plans = run_history_value(
        &state,
        &[
            "plans", "--source", &source, "--limit", "20", "--offset", "0", "--format", "json",
        ],
    )
    .await?;
    Ok(Json(json!({
        "source_id": source,
        "refreshed_at": chrono::Utc::now().to_rfc3339(),
        "sessions": sessions,
        "plans": plans
    })))
}

async fn local_history_sessions(
    State(state): State<AppState>,
    Path(source): Path<String>,
    Query(query): Query<HistoryPageQuery>,
) -> std::result::Result<Json<Value>, (StatusCode, String)> {
    let source = normalized_source(Some(&source));
    let limit = normalized_history_limit(query.limit, 25);
    let offset = query.offset.unwrap_or_default().min(100_000);
    let limit_text = limit.to_string();
    let offset_text = offset.to_string();
    run_history_json(
        &state,
        &[
            "sessions",
            "--source",
            &source,
            "--limit",
            &limit_text,
            "--offset",
            &offset_text,
            "--format",
            "json",
        ],
    )
    .await
}

async fn local_history_export(
    State(state): State<AppState>,
    Path((source, session_id)): Path<(String, String)>,
    Query(query): Query<HistoryExportQuery>,
) -> std::result::Result<Response, (StatusCode, String)> {
    let source = normalized_source(Some(&source));
    let schema = normalized_export_schema(query.schema.as_deref())?;
    let format = normalized_export_format(query.format.as_deref())?;
    let output = run_history_command(
        &state,
        &[
            "export",
            "--source",
            &source,
            "--session-id",
            &session_id,
            "--schema",
            &schema,
            "--format",
            &format,
        ],
    )
    .await?;
    let mut headers = HeaderMap::new();
    let content_type = if format == "csv" {
        "text/csv; charset=utf-8"
    } else {
        "application/json"
    };
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!(
            "attachment; filename=\"brick-history-{source}-{session_id}.{format}\""
        ))
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?,
    );
    Ok((headers, output).into_response())
}

async fn run_history_json(
    state: &AppState,
    history_args: &[&str],
) -> std::result::Result<Json<Value>, (StatusCode, String)> {
    let value = run_history_value(state, history_args).await?;
    Ok(Json(value))
}

async fn run_source_json(
    state: &AppState,
    source_args: &[&str],
) -> std::result::Result<Json<Value>, (StatusCode, String)> {
    let output = run_local_brick_command(state, "source", source_args).await?;
    let value = parse_local_json(&output, "local source command")?;
    Ok(Json(value))
}

async fn run_history_value(
    state: &AppState,
    history_args: &[&str],
) -> std::result::Result<Value, (StatusCode, String)> {
    let output = run_history_command(state, history_args).await?;
    parse_local_json(&output, "local history command")
}

fn parse_local_json(
    output: &[u8],
    command_name: &str,
) -> std::result::Result<Value, (StatusCode, String)> {
    serde_json::from_slice(output).map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("{command_name} returned invalid JSON: {error}"),
        )
    })
}

async fn run_history_command(
    state: &AppState,
    history_args: &[&str],
) -> std::result::Result<Vec<u8>, (StatusCode, String)> {
    run_local_brick_command(state, "history", history_args).await
}

async fn run_local_brick_command(
    state: &AppState,
    command_group: &str,
    command_args: &[&str],
) -> std::result::Result<Vec<u8>, (StatusCode, String)> {
    let bridge = state.local_history.as_ref().ok_or_else(|| {
        (
            StatusCode::FORBIDDEN,
            "local history bridge is disabled; restart brick-server with --enable-local-history"
                .to_string(),
        )
    })?;
    let brick_bin = bridge.brick_bin.clone();
    let repo_root = bridge.repo_root.clone();
    let command_group = command_group.to_string();
    let command_args = command_args
        .iter()
        .map(|arg| arg.to_string())
        .collect::<Vec<_>>();
    let output = tokio::task::spawn_blocking(move || {
        let mut command = Command::new(brick_bin.as_os_str());
        if let Some(repo_root) = repo_root.as_ref() {
            command.current_dir(repo_root.as_ref());
        }
        command
            .arg(&command_group)
            .args(command_args)
            .stdin(Stdio::null())
            .output()
    })
    .await
    .map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to join local brick command task: {error}"),
        )
    })?
    .map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to run local brick command: {error}"),
        )
    })?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Err((
            StatusCode::BAD_REQUEST,
            if stderr.is_empty() { stdout } else { stderr },
        ))
    }
}

fn normalized_source(value: Option<&str>) -> String {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("all")
        .to_string()
}

fn normalized_source_list(
    values: &[String],
) -> std::result::Result<Vec<String>, (StatusCode, String)> {
    let mut sources = Vec::new();
    for value in values {
        let source_name = value.trim();
        if source_name.is_empty() {
            continue;
        }
        if !source_name.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '_' || character == '-'
        }) {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("invalid source name: {source_name}"),
            ));
        }
        if !sources.iter().any(|existing| existing == source_name) {
            sources.push(source_name.to_string());
        }
    }
    Ok(sources)
}

fn normalized_history_limit(value: Option<usize>, default: usize) -> usize {
    value.unwrap_or(default).clamp(1, 1000)
}

fn normalized_export_schema(
    value: Option<&str>,
) -> std::result::Result<String, (StatusCode, String)> {
    match value.unwrap_or("audit-v1") {
        "audit-v1" | "source-metadata-v1" => Ok(value.unwrap_or("audit-v1").to_string()),
        other => Err((
            StatusCode::BAD_REQUEST,
            format!("unsupported export schema: {other}"),
        )),
    }
}

fn normalized_export_format(
    value: Option<&str>,
) -> std::result::Result<String, (StatusCode, String)> {
    match value.unwrap_or("json") {
        "json" | "csv" => Ok(value.unwrap_or("json").to_string()),
        other => Err((
            StatusCode::BAD_REQUEST,
            format!("unsupported export format: {other}"),
        )),
    }
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
pub async fn serve(
    bind: String,
    store: ServerStore,
    local_history: Option<LocalHistoryBridge>,
    auth: Option<AuthConfig>,
) -> Result<()> {
    store.init()?;
    let authenticated = auth.is_some();
    let app = build_router(store, local_history, auth);
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    println!(
        "brick-server listening on http://{bind} (auth: {})",
        if authenticated {
            "bearer-token"
        } else {
            "disabled"
        }
    );
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
    use brick_protocol::{
        ActorRef, ActorType, MissionCreatedPayload, MissionId, MissionStatus, ProjectId, TraceEvent,
    };
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
                project_id: ProjectId::new(),
                title: "Server route".to_string(),
                description: None,
                status: MissionStatus::Planned,
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
        let app = build_router(store, None, None);
        let routes = app.into_make_service();
        let _ = routes;
    }

    #[tokio::test]
    async fn local_history_routes_require_explicit_enablement() {
        let store = ServerStore::new(temp_data_dir("history-disabled"));
        let app = build_router(store, None, None);

        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/v1/local-history/sources")
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("request local history sources");

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn repo_events_route_pushes_and_lists_scoped_events() {
        let store = ServerStore::new(temp_data_dir("repo-events"));
        let app = build_router(store, None, None);
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
        let app = build_router(store, None, None);
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

    #[tokio::test]
    async fn auth_token_gates_protected_routes_but_not_health() {
        let audit_dir = temp_data_dir("auth-audit");
        let store = ServerStore::new(temp_data_dir("auth"));
        let app = build_router(
            store,
            None,
            Some(AuthConfig::new(
                crate::auth::single_token_store("s3cret"),
                crate::auth::AuditLog::new(&audit_dir),
            )),
        );

        // /health stays open without a token.
        let health = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/health")
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("health request");
        assert_eq!(health.status(), StatusCode::OK);

        // A protected route with no token is rejected.
        let missing = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/v1/sessions")
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("unauthed request");
        assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);

        // A protected route with the wrong token is rejected.
        let wrong = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/v1/sessions")
                    .header(header::AUTHORIZATION, "Bearer nope")
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("wrong-token request");
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

        // The correct token passes the gate (route then handles normally).
        let ok = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/v1/sessions")
                    .header(header::AUTHORIZATION, "Bearer s3cret")
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("authed request");
        assert_ne!(ok.status(), StatusCode::UNAUTHORIZED);
    }
}
