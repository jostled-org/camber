use std::fmt::Debug;
use std::future::{Future, IntoFuture};

use camber::RuntimeError;

const SERVER_SOURCE: &str = include_str!("../src/http/server.rs");

fn assert_flat_future<F>()
where
    F: Future<Output = Result<(), RuntimeError>>,
{
}

fn assert_server_handle_into_future<T>()
where
    T: IntoFuture<Output = Result<(), RuntimeError>, IntoFuture = camber::http::ServerHandleFuture>,
{
}

fn assert_lifecycle_enum_traits<T>()
where
    T: Copy + Clone + Debug + Eq + PartialEq,
{
}

fn attached_rustdoc(declaration: &str) -> Option<Box<str>> {
    let lines = SERVER_SOURCE.lines().collect::<Vec<_>>();
    let declaration_index = lines.iter().position(|line| line.trim() == declaration)?;
    let mut docs = lines[..declaration_index]
        .iter()
        .rev()
        .skip_while(|line| line.trim_start().starts_with("#["))
        .map_while(|line| line.trim_start().strip_prefix("///"))
        .collect::<Vec<_>>();
    docs.reverse();

    match docs.is_empty() {
        true => None,
        false => Some(docs.join(" ").into_boxed_str()),
    }
}

fn sentence_contains_all(docs: &str, terms: &[&str]) -> bool {
    docs.split(['.', '!', '?']).any(|sentence| {
        let sentence = sentence.to_ascii_lowercase();
        terms.iter().all(|term| sentence.contains(term))
    })
}

fn blocks_contain_coherent_statement(blocks: &[&str], terms: &[&str]) -> bool {
    blocks.iter().any(|docs| sentence_contains_all(docs, terms))
}

fn blocks_contain_coherent_exclusion(blocks: &[&str], terms: &[&str]) -> bool {
    blocks.iter().any(|docs| {
        docs.split(['.', '!', '?']).any(|sentence| {
            let sentence = sentence.to_ascii_lowercase();
            let excludes = sentence.contains("does not")
                || sentence.contains("cannot")
                || sentence.contains("not proof")
                || sentence.contains("outside");
            excludes && terms.iter().all(|term| sentence.contains(term))
        })
    })
}

// 2.T6
#[test]
fn additive_lifecycle_api_compiles_through_exact_public_paths() {
    let _: fn(&camber::http::ServerHandle) = camber::http::ServerHandle::shutdown;
    let _: fn(camber::http::ServerHandle) -> camber::http::ServerHandleFuture =
        camber::http::ServerHandle::join;
    let _: fn(camber::http::ServerHandle) -> camber::http::ServerHandleFuture =
        camber::http::ServerHandle::shutdown_and_join;
    let _: fn(&camber::http::ServerHandleFuture) = camber::http::ServerHandleFuture::shutdown;
    let _: fn(&camber::http::ServerHandleFuture) = camber::http::ServerHandleFuture::cancel;

    assert_flat_future::<camber::http::ServerHandleFuture>();
    assert_server_handle_into_future::<camber::http::ServerHandle>();

    assert_lifecycle_enum_traits::<camber::http::mock::LifecycleCheckpoint>();
    assert_lifecycle_enum_traits::<camber::http::mock::LifecycleFault>();
    assert_lifecycle_enum_traits::<camber::http::mock::SupervisorJoinProbe>();
}

// 2.T6
#[test]
fn public_lifecycle_rustdoc_states_completion_boundaries() {
    let handle_docs = attached_rustdoc("pub struct ServerHandle {");
    let future_docs = attached_rustdoc("pub struct ServerHandleFuture {");
    assert!(
        handle_docs.is_some(),
        "ServerHandle must have an attached public rustdoc block"
    );
    assert!(
        future_docs.is_some(),
        "ServerHandleFuture must have an attached public rustdoc block"
    );
    let handle_docs = handle_docs.unwrap_or_default();
    let future_docs = future_docs.unwrap_or_default();
    let public_owner_docs = [handle_docs.as_ref(), future_docs.as_ref()];

    assert!(
        blocks_contain_coherent_statement(
            &public_owner_docs,
            &[
                "join",
                "prove",
                "accepted transport",
                "connection permit",
                "websocket bridge",
            ],
        ),
        "owner rustdoc must state one coherent positive join proof for transports, permits, and bridges"
    );
    assert!(
        blocks_contain_coherent_exclusion(&public_owner_docs, &["non-yielding async"]),
        "owner rustdoc must exclude non-yielding async execution from join proof"
    );
    assert!(
        blocks_contain_coherent_exclusion(
            &public_owner_docs,
            &["callback", "request", "captures", "wsconn"],
        ),
        "owner rustdoc must exclude callback-held Request, captures, and WsConn from join proof"
    );
    assert!(
        blocks_contain_coherent_exclusion(&public_owner_docs, &["runtime teardown", "watcher"]),
        "owner rustdoc must exclude runtime teardown after the watcher is gone"
    );
}
