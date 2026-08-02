use camber::http::{self, IntoResponse, Request, Response, Router};
use camber::{RuntimeError, runtime, tracing};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct User {
    id: u64,
    name: Box<str>,
    email: Box<str>,
}

#[derive(Debug, Deserialize)]
struct CreateUser {
    name: Box<str>,
    email: Box<str>,
}

#[derive(Debug, Deserialize)]
struct UpdateUser {
    name: Option<Box<str>>,
    email: Option<Box<str>>,
}

type Store = Arc<Mutex<UserStore>>;

struct UserStore {
    users: HashMap<u64, User>,
    next_id: u64,
}

fn invalid_id() -> Result<Response, RuntimeError> {
    Response::json(400, &serde_json::json!({"error": "invalid id"}))
}

fn invalid_json() -> Result<Response, RuntimeError> {
    Response::json(400, &serde_json::json!({"error": "invalid json"}))
}

fn not_found() -> Result<Response, RuntimeError> {
    Response::json(404, &serde_json::json!({"error": "not found"}))
}

fn parse_user_id(req: &Request) -> Option<u64> {
    req.param("id").and_then(|v| v.parse::<u64>().ok())
}

fn parse_json_body<T: serde::de::DeserializeOwned>(req: &Request) -> Option<T> {
    req.json().ok()
}

fn new_store() -> Store {
    Arc::new(Mutex::new(UserStore {
        users: HashMap::new(),
        next_id: 1,
    }))
}

fn register_middleware(router: &mut Router) {
    router.use_middleware(|req: &Request, next: http::Next| {
        let authorized = req.header("authorization").is_some() || req.path() == "/health";
        let downstream = match authorized {
            true => Some(next.call(req)),
            false => None,
        };
        async move {
            match downstream {
                Some(future) => future.await,
                None => Response::json(401, &serde_json::json!({"error": "unauthorized"}))
                    .into_response(),
            }
        }
    });
    router.use_middleware(|req: &Request, next: http::Next| {
        let start = std::time::Instant::now();
        let method = req.method();
        let path: Box<str> = req.path().into();
        let downstream = next.call(req);
        async move {
            let resp = downstream.await;
            tracing::info!(
                method = method,
                path = %path,
                status = resp.status(),
                latency_ms = start.elapsed().as_millis(),
            );
            resp
        }
    });
    router.use_middleware(http::cors::allow_origins(&["*"]));
}

fn get_user_response(req: &Request, store: &Store) -> Result<Response, RuntimeError> {
    let id = match parse_user_id(req) {
        Some(id) => id,
        None => return invalid_id(),
    };
    let guard = store.lock().unwrap_or_else(|e| e.into_inner());
    match guard.users.get(&id) {
        Some(user) => Response::json(200, user),
        None => not_found(),
    }
}

fn create_user_response(req: &Request, store: &Store) -> Result<Response, RuntimeError> {
    let input: CreateUser = match parse_json_body(req) {
        Some(input) => input,
        None => return invalid_json(),
    };
    let mut guard = store.lock().unwrap_or_else(|e| e.into_inner());
    let id = guard.next_id;
    guard.next_id += 1;
    let user = User {
        id,
        name: input.name,
        email: input.email,
    };
    guard.users.insert(id, user.clone());
    Response::json(201, &user)
}

fn update_user_response(req: &Request, store: &Store) -> Result<Response, RuntimeError> {
    let id = match parse_user_id(req) {
        Some(id) => id,
        None => return invalid_id(),
    };
    let input: UpdateUser = match parse_json_body(req) {
        Some(input) => input,
        None => return invalid_json(),
    };
    let mut guard = store.lock().unwrap_or_else(|e| e.into_inner());
    let user = match guard.users.get_mut(&id) {
        Some(user) => user,
        None => return not_found(),
    };
    if let Some(name) = input.name {
        user.name = name;
    }
    if let Some(email) = input.email {
        user.email = email;
    }
    Response::json(200, user)
}

fn delete_user_response(req: &Request, store: &Store) -> Result<Response, RuntimeError> {
    let id = match parse_user_id(req) {
        Some(id) => id,
        None => return invalid_id(),
    };
    let mut guard = store.lock().unwrap_or_else(|e| e.into_inner());
    match guard.users.remove(&id) {
        Some(_) => Response::empty(204),
        None => not_found(),
    }
}

fn build_router(store: &Store) -> Router {
    let mut router = Router::new();
    register_middleware(&mut router);

    router.get("/health", |_: &Request| async {
        Response::json(200, &serde_json::json!({"status": "ok"}))
    });

    let list_store = Arc::clone(store);
    router.get("/users", move |_: &Request| {
        let guard = list_store.lock().unwrap_or_else(|e| e.into_inner());
        let users: Vec<&User> = guard.users.values().collect();
        let response = Response::json(200, &users);
        async move { response }
    });

    let get_store = Arc::clone(store);
    router.get("/users/:id", move |req: &Request| {
        let response = get_user_response(req, &get_store);
        async move { response }
    });

    let create_store = Arc::clone(store);
    router.post("/users", move |req: &Request| {
        let response = create_user_response(req, &create_store);
        async move { response }
    });

    let update_store = Arc::clone(store);
    router.put("/users/:id", move |req: &Request| {
        let response = update_user_response(req, &update_store);
        async move { response }
    });

    let delete_store = Arc::clone(store);
    router.delete("/users/:id", move |req: &Request| {
        let response = delete_user_response(req, &delete_store);
        async move { response }
    });

    router
}

fn main() -> Result<(), RuntimeError> {
    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".into());
    let addr = format!("0.0.0.0:{port}");
    let store = new_store();
    let router = build_router(&store);

    runtime::builder()
        .with_tracing()
        .run(|| http::serve(&addr, router))?
}
