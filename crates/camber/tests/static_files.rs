mod common;

use camber::http::{self, Router};
use camber::runtime;
use std::io::Write;

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

    let css_ct = css
        .headers()
        .iter()
        .find(|(k, _)| k.as_ref().eq_ignore_ascii_case("content-type"))
        .map(|(_, v)| v.as_ref());
    let json_ct = json
        .headers()
        .iter()
        .find(|(k, _)| k.as_ref().eq_ignore_ascii_case("content-type"))
        .map(|(_, v)| v.as_ref());

    assert_eq!(css_ct, Some("text/css"));
    assert_eq!(json_ct, Some("application/json"));

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

    runtime::request_shutdown();
}
