use camber::http::{self, Request, Response, Router};
use camber::{RuntimeError, runtime};
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

fn invalid_id() -> serde_json::Value {
    serde_json::json!({"error": "invalid id"})
}

fn invalid_json() -> serde_json::Value {
    serde_json::json!({"error": "invalid json"})
}

fn parse_user_id(req: &Request) -> Result<u64, Response> {
    req.param("id")
        .and_then(|v| v.parse::<u64>().ok())
        .ok_or_else(|| Response::json(400, &invalid_id()).unwrap_or_else(|e| Response::text_raw(500, &e.to_string())))
}

fn parse_json_body<T: serde::de::DeserializeOwned>(req: &Request) -> Result<T, Response> {
    req.json()
        .map_err(|_| Response::json(400, &invalid_json()).unwrap_or_else(|e| Response::text_raw(500, &e.to_string())))
}

fn main() -> Result<(), RuntimeError> {
    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".into());
    let addr = format!("0.0.0.0:{port}");

    let store: Store = Arc::new(Mutex::new(UserStore {
        users: HashMap::new(),
        next_id: 1,
    }));

    let mut router = Router::new();

    // Middleware: auth check
    router.use_middleware(move |req: &Request, next: http::Next| async move {
        match (req.header("authorization"), req.path()) {
            (Some(_), _) => next.call(req).await,
            (None, "/health") => next.call(req).await,
            (None, _) => {
                Response::json(401, &serde_json::json!({"error": "unauthorized"}))
                    .expect("valid status")
            }
        }
    });

    // Middleware: request logging
    router.use_middleware(|req: &Request, next: http::Next| {
        let start = std::time::Instant::now();
        let method = req.method();
        let path: Box<str> = req.path().into();
        async move {
            let resp = next.call(req).await;
            tracing::info!(
                method = method,
                path = %path,
                status = resp.status(),
                latency_ms = start.elapsed().as_millis(),
            );
            resp
        }
    });

    // Middleware: CORS
    router.use_middleware(http::cors::allow_origins(&["*"]));

    // Health check
    router.get("/health", |_: &Request| {
        Response::json(200, &serde_json::json!({"status": "ok"}))
    });

    // List users
    let s = Arc::clone(&store);
    router.get("/users", move |_: &Request| {
        let guard = s.lock().unwrap_or_else(|e| e.into_inner());
        let users: Vec<&User> = guard.users.values().collect();
        Response::json(200, &users)
    });

    // Get user by ID
    let s = Arc::clone(&store);
    router.get("/users/:id", move |req: &Request| {
        let id = match parse_user_id(req) {
            Ok(id) => id,
            Err(resp) => return Ok(resp),
        };
        let guard = s.lock().unwrap_or_else(|e| e.into_inner());
        match guard.users.get(&id) {
            Some(user) => Response::json(200, user),
            None => Response::json(404, &serde_json::json!({"error": "not found"})),
        }
    });

    // Create user
    let s = Arc::clone(&store);
    router.post("/users", move |req: &Request| {
        let input: CreateUser = match parse_json_body(req) {
            Ok(input) => input,
            Err(resp) => return Ok(resp),
        };
        let mut guard = s.lock().unwrap_or_else(|e| e.into_inner());
        let id = guard.next_id;
        guard.next_id += 1;
        let user = User {
            id,
            name: input.name,
            email: input.email,
        };
        guard.users.insert(id, user.clone());
        Response::json(201, &user)
    });

    // Update user
    let s = Arc::clone(&store);
    router.put("/users/:id", move |req: &Request| {
        let id = match parse_user_id(req) {
            Ok(id) => id,
            Err(resp) => return Ok(resp),
        };
        let input: UpdateUser = match parse_json_body(req) {
            Ok(input) => input,
            Err(resp) => return Ok(resp),
        };
        let mut guard = s.lock().unwrap_or_else(|e| e.into_inner());
        let user = match guard.users.get_mut(&id) {
            Some(u) => u,
            None => return Response::json(404, &serde_json::json!({"error": "not found"})),
        };
        if let Some(n) = input.name {
            user.name = n;
        }
        if let Some(e) = input.email {
            user.email = e;
        }
        Response::json(200, user)
    });

    // Delete user
    let s = Arc::clone(&store);
    router.delete("/users/:id", move |req: &Request| {
        let id = match parse_user_id(req) {
            Ok(id) => id,
            Err(resp) => return Ok(resp),
        };
        let mut guard = s.lock().unwrap_or_else(|e| e.into_inner());
        match guard.users.remove(&id) {
            Some(_) => Response::empty(204),
            None => Response::json(404, &serde_json::json!({"error": "not found"})),
        }
    });

    runtime::builder().with_tracing().run(|| {
        http::serve(&addr, router)
    })
}
