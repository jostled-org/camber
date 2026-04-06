use axum::extract::{Json, Path, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::IntoResponse;
use axum::routing::{delete, get, post, put};
use axum::Router;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct User {
    id: u64,
    name: String,
    email: String,
}

#[derive(Debug, Deserialize)]
struct CreateUser {
    name: String,
    email: String,
}

#[derive(Debug, Deserialize)]
struct UpdateUser {
    name: Option<String>,
    email: Option<String>,
}

#[derive(Clone)]
struct AppState {
    store: Arc<Mutex<UserStore>>,
}

struct UserStore {
    users: HashMap<u64, User>,
    next_id: u64,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".into());
    let addr = format!("0.0.0.0:{port}");

    let state = AppState {
        store: Arc::new(Mutex::new(UserStore {
            users: HashMap::new(),
            next_id: 1,
        })),
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/users", get(list_users).post(create_user))
        .route(
            "/users/{id}",
            get(get_user).put(update_user).delete(delete_user),
        )
        .layer(tower_http::cors::CorsLayer::permissive())
        .layer(middleware::from_fn(logging_middleware))
        .layer(middleware::from_fn(auth_middleware))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    tracing::info!("listening on {addr}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install signal handler");
}

async fn auth_middleware(
    req: axum::http::Request<axum::body::Body>,
    next: Next,
) -> impl IntoResponse {
    let is_health = req.uri().path() == "/health";
    let has_auth = req.headers().contains_key("authorization");

    match (has_auth, is_health) {
        (true, _) | (_, true) => next.run(req).await.into_response(),
        (false, false) => (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "unauthorized"})),
        )
            .into_response(),
    }
}

async fn logging_middleware(
    req: axum::http::Request<axum::body::Body>,
    next: Next,
) -> impl IntoResponse {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let start = std::time::Instant::now();

    let resp = next.run(req).await;

    tracing::info!(
        method = %method,
        path = %path,
        status = resp.status().as_u16(),
        latency_ms = start.elapsed().as_millis(),
    );

    resp
}

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({"status": "ok"}))
}

async fn list_users(State(state): State<AppState>) -> impl IntoResponse {
    let guard = state.store.lock().unwrap_or_else(|e| e.into_inner());
    let users: Vec<&User> = guard.users.values().collect();
    Json(serde_json::to_value(&users).unwrap_or_default())
}

async fn get_user(
    State(state): State<AppState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    let guard = state.store.lock().unwrap_or_else(|e| e.into_inner());
    match guard.users.get(&id) {
        Some(user) => (StatusCode::OK, Json(serde_json::to_value(user).unwrap_or_default())).into_response(),
        None => (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "not found"}))).into_response(),
    }
}

async fn create_user(
    State(state): State<AppState>,
    Json(input): Json<CreateUser>,
) -> impl IntoResponse {
    let mut guard = state.store.lock().unwrap_or_else(|e| e.into_inner());
    let id = guard.next_id;
    guard.next_id += 1;
    let user = User {
        id,
        name: input.name,
        email: input.email,
    };
    guard.users.insert(id, user.clone());
    (StatusCode::CREATED, Json(serde_json::to_value(&user).unwrap_or_default()))
}

async fn update_user(
    State(state): State<AppState>,
    Path(id): Path<u64>,
    Json(input): Json<UpdateUser>,
) -> impl IntoResponse {
    let mut guard = state.store.lock().unwrap_or_else(|e| e.into_inner());
    let user = match guard.users.get_mut(&id) {
        Some(u) => u,
        None => return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "not found"}))).into_response(),
    };
    input.name.map(|n| user.name = n);
    input.email.map(|e| user.email = e);
    (StatusCode::OK, Json(serde_json::to_value(user).unwrap_or_default())).into_response()
}

async fn delete_user(
    State(state): State<AppState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    let mut guard = state.store.lock().unwrap_or_else(|e| e.into_inner());
    match guard.users.remove(&id) {
        Some(_) => StatusCode::NO_CONTENT.into_response(),
        None => (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "not found"}))).into_response(),
    }
}
