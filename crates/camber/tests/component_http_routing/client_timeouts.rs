use crate::runtime_support as common;

use camber::http::ClientResponse;
use camber::http::{self, Request, Response, Router};
use camber::runtime;

#[camber::test]
async fn default_client_succeeds_for_fast_responses() {
    let mut router = Router::new();
    router.get("/fast", |_req: &Request| async {
        Response::text(200, "fast")
    });

    let addr = common::spawn_server(router);

    let resp: ClientResponse = http::get(&format!("http://{addr}/fast")).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().unwrap(), "fast");

    runtime::request_shutdown();
}
