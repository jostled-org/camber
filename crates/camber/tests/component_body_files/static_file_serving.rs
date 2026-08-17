use crate::runtime_support as common;

use camber::http::{self, HeaderPair, Response, Router};
use camber::runtime;
use std::io::Write;
use std::path::Path;

fn temp_dir_with_files(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (name, content) in files {
        let path = dir.path().join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }
    dir
}

/// The same file, taken from the direct owner every routed request delegates to.
///
/// Each row below states its claim twice: once through the route a peer reaches
/// and once through the async fallible entry point that answers it. Path
/// confinement, MIME selection, and not-found behavior belong to that owner, so
/// a row that only asked the router could not say which of the two decided.
async fn served_directly(base: &Path, file_path: &str) -> Response {
    http::serve_file(base, file_path)
        .await
        .expect("a file under the default maximum")
}

/// The Content-Type one answer carries, however it was spelled.
fn content_type(headers: &[HeaderPair]) -> Option<&str> {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .map(|(_, value)| value.as_ref())
}

#[camber::test]
async fn static_files_serves_file_content() {
    let dir = temp_dir_with_files(&[("hello.txt", "hello from file")]);
    let mut router = Router::new();
    router.static_files("/assets", dir.path().to_str().unwrap());

    let addr = common::spawn_server(router);
    let resp = http::get(&format!("http://{addr}/assets/hello.txt"))
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "hello from file");

    let direct = served_directly(dir.path(), "hello.txt").await;
    assert_eq!(direct.status(), 200);
    assert_eq!(direct.body(), "hello from file");

    runtime::request_shutdown();
}

#[camber::test]
async fn static_files_serves_index_at_root() {
    let dir = temp_dir_with_files(&[("index.html", "<h1>home</h1>")]);
    let mut router = Router::new();
    router.static_files("", dir.path().to_str().unwrap());

    let addr = common::spawn_server(router);
    let resp = http::get(&format!("http://{addr}/")).await.unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "<h1>home</h1>");

    // The bare prefix asks the same owner for the index by name.
    let direct = served_directly(dir.path(), "index.html").await;
    assert_eq!(direct.status(), 200);
    assert_eq!(direct.body(), "<h1>home</h1>");

    runtime::request_shutdown();
}

#[camber::test]
async fn static_files_sets_content_type() {
    let dir = temp_dir_with_files(&[("style.css", "body {}"), ("data.json", "{}")]);
    let mut router = Router::new();
    router.static_files("/assets", dir.path().to_str().unwrap());

    let addr = common::spawn_server(router);

    let css = http::get(&format!("http://{addr}/assets/style.css"))
        .await
        .unwrap();
    let json = http::get(&format!("http://{addr}/assets/data.json"))
        .await
        .unwrap();

    assert_eq!(content_type(css.headers()), Some("text/css"));
    assert_eq!(content_type(json.headers()), Some("application/json"));

    assert_eq!(
        content_type(served_directly(dir.path(), "style.css").await.headers()),
        Some("text/css"),
    );
    assert_eq!(
        content_type(served_directly(dir.path(), "data.json").await.headers()),
        Some("application/json"),
    );

    runtime::request_shutdown();
}

#[camber::test]
async fn static_files_returns_404_for_missing() {
    let dir = temp_dir_with_files(&[]);
    let mut router = Router::new();
    router.static_files("/assets", dir.path().to_str().unwrap());

    let addr = common::spawn_server(router);
    let resp = http::get(&format!("http://{addr}/assets/nonexistent.txt"))
        .await
        .unwrap();

    assert_eq!(resp.status(), 404);

    // A missing file is an answer, not a failure: the fallible signature
    // reports 404 through `Ok` and keeps its errors for the runtime, the
    // maximum, and a read that broke.
    assert_eq!(
        served_directly(dir.path(), "nonexistent.txt")
            .await
            .status(),
        404
    );

    runtime::request_shutdown();
}

#[camber::test]
async fn static_files_blocks_directory_traversal() {
    let dir = temp_dir_with_files(&[("safe.txt", "safe content")]);
    let mut router = Router::new();
    router.static_files("/assets", dir.path().to_str().unwrap());

    let addr = common::spawn_server(router);
    let resp = http::get(&format!("http://{addr}/assets/../../../etc/passwd"))
        .await
        .unwrap();

    assert_eq!(resp.status(), 404);

    // Confinement is the owner's, so it holds for a caller that never went
    // through a route at all.
    assert_eq!(
        served_directly(dir.path(), "../../../etc/passwd")
            .await
            .status(),
        404,
    );

    runtime::request_shutdown();
}
