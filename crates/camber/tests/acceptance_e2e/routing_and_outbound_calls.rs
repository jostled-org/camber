use camber::http::ClientResponse;
use camber::http::{self, Request, Response, Router};
use camber::{RuntimeError, runtime, spawn};
#[camber::test]
async fn full_server_with_routing_and_outbound_calls() {
    // Backend server
    let mut backend_router = Router::new();
    backend_router.get("/data", |_req: &Request| async {
        Response::text(200, "data-from-backend")
    });
    let backend_listener = camber::net::listen("127.0.0.1:0").unwrap();
    let backend_addr = backend_listener.local_addr().unwrap().tcp().unwrap();
    spawn(move || -> Result<(), RuntimeError> {
        http::serve_listener(backend_listener, backend_router)
    });

    // Main server with routing
    let mut router = Router::new();
    router.get("/hello", |_req: &Request| async {
        Response::text(200, "Hello, world!")
    });
    let backend_url = format!("http://{backend_addr}/data");
    router.get("/proxy", move |_req: &Request| {
        let backend_url = backend_url.clone();
        async move {
            let fetched: Result<ClientResponse, RuntimeError> = http::get(&backend_url).await;
            match fetched {
                Ok(resp) => Response::text(200, resp.text()?),
                Err(_) => Response::text(502, "upstream error"),
            }
        }
    });
    let main_listener = camber::net::listen("127.0.0.1:0").unwrap();
    let main_addr = main_listener.local_addr().unwrap().tcp().unwrap();
    spawn(move || -> Result<(), RuntimeError> { http::serve_listener(main_listener, router) });

    // Client tests
    let hello: ClientResponse = http::get(&format!("http://{main_addr}/hello"))
        .await
        .unwrap();
    assert_eq!(hello.status(), 200);
    assert_eq!(hello.text().unwrap(), "Hello, world!");

    let proxy: ClientResponse = http::get(&format!("http://{main_addr}/proxy"))
        .await
        .unwrap();
    assert_eq!(proxy.status(), 200);
    assert!(
        proxy.text().unwrap().contains("data-from-backend"),
        "expected backend data, got: {}",
        proxy.text().unwrap()
    );

    let not_found: ClientResponse = http::get(&format!("http://{main_addr}/unknown"))
        .await
        .unwrap();
    assert_eq!(not_found.status(), 404);

    runtime::request_shutdown();
}
