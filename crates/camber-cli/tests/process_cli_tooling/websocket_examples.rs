//! Every shipped WebSocket example must compile against this tree's Camber.
//!
//! The examples come from `camber context` and from the public documents a
//! user reads first. Each snippet body is compiled unchanged, as one module of
//! one generated Cargo project. A snippet that does not compile fails the
//! test; it never drops out of the selection.

use std::path::Path;

use crate::support::FixtureError;
use crate::support::generated_project::{
    camber_context, cargo_succeeds, in_temp_dir, pin_workspace_lock, workspace_crate_path,
    workspace_root,
};

/// The documents that must each ship at least one WebSocket example.
const DOCUMENTS: [&str; 4] = [
    "README.md",
    "docs/guides/go-to-camber.md",
    "docs/guides/tokio-to-camber.md",
    "docs/reference/http.md",
];
const CONTEXT_SOURCE: &str = "camber context";
/// A Rust fence that names any of these is a WebSocket example.
const WEBSOCKET_MARKERS: [&str; 4] = [".ws(", "WsConn", "WsSender", "WsReceiver"];
/// A snippet with a top-level line that starts like this declares items, and
/// compiles as written. Any other snippet is a statement body.
const ITEM_STARTS: [&str; 8] = [
    "fn ",
    "async fn ",
    "pub ",
    "struct ",
    "enum ",
    "impl ",
    "mod ",
    "#[",
];
/// Names a snippet may use without importing them, as the documents assume.
/// A glob import, so a snippet's own `use` of the same name shadows it.
const PRELUDE: &str = "mod prelude {
    pub use camber::RuntimeError;
    pub use camber::http::{
        self, Bytes, Request, Response, Router, WsCloseCause, WsConn, WsMessage, WsReceive,
        WsReceiver, WsSender,
    };
    pub use std::time::Duration;
}
";

/// One WebSocket Rust fence and where it came from.
struct Snippet {
    source: &'static str,
    line: usize,
    body: Box<str>,
}

#[test]
fn public_websocket_examples_compile() -> Result<(), FixtureError> {
    in_temp_dir(compile_examples)
}

/// Select every shipped WebSocket example and compile them as one project
/// under `dir`.
fn compile_examples(dir: &Path) -> Result<(), FixtureError> {
    let root = workspace_root()?;
    let mut snippets = context_snippets(dir)?;
    for document in DOCUMENTS {
        let text = std::fs::read_to_string(root.join(document))?;
        snippets.extend(websocket_snippets(document, &text)?);
    }
    assert_inventory(&snippets);

    let project_dir = dir.join("websocket-examples");
    write_project(&project_dir, &snippets)?;
    cargo_succeeds(&project_dir, &["check", "--lib"], &inventory(&snippets))
}

/// The WebSocket examples `camber context` writes into `llms.txt`.
fn context_snippets(dir: &Path) -> Result<Vec<Snippet>, FixtureError> {
    camber_context(dir)?;
    let text = std::fs::read_to_string(dir.join("llms.txt"))?;
    websocket_snippets(CONTEXT_SOURCE, &text)
}

/// Every ```rust fence in `text` that names a WebSocket marker. A fence still
/// open at the end of `text` fails.
fn websocket_snippets(source: &'static str, text: &str) -> Result<Vec<Snippet>, FixtureError> {
    let mut snippets = Vec::new();
    let mut open: Option<(usize, Vec<&str>)> = None;
    for (index, line) in text.lines().enumerate() {
        let fence = line.trim_start();
        open = match (open, fence.starts_with("```")) {
            (None, true) if fence.starts_with("```rust") => Some((index + 1, Vec::new())),
            (None, _) => None,
            (Some((start, body)), true) => {
                snippets.extend(websocket_snippet(source, start, &body));
                None
            }
            (Some((start, mut body)), false) => {
                body.push(line);
                Some((start, body))
            }
        };
    }
    match open {
        None => Ok(snippets),
        Some((start, _)) => Err(FixtureError::new(format!(
            "{source}:{start}: ```rust fence is never closed"
        ))),
    }
}

fn websocket_snippet(source: &'static str, line: usize, body: &[&str]) -> Option<Snippet> {
    let body = body.join("\n");
    WEBSOCKET_MARKERS
        .iter()
        .any(|marker| body.contains(marker))
        .then(|| Snippet {
            source,
            line,
            body: body.into_boxed_str(),
        })
}

/// The selection is nonempty and reaches every document that must ship one.
fn assert_inventory(snippets: &[Snippet]) {
    let missing: Box<[&str]> = std::iter::once(CONTEXT_SOURCE)
        .chain(DOCUMENTS)
        .filter(|source| !snippets.iter().any(|snippet| snippet.source == *source))
        .collect();
    assert!(
        missing.is_empty(),
        "no WebSocket Rust example found in: {}",
        missing.join(", ")
    );
}

/// Each module's name and its source, for a failure to point at.
fn inventory(snippets: &[Snippet]) -> Box<str> {
    snippets
        .iter()
        .enumerate()
        .map(|(index, snippet)| format!("\n  snippet_{index}: {}:{}", snippet.source, snippet.line))
        .collect::<String>()
        .into_boxed_str()
}

fn write_project(project_dir: &Path, snippets: &[Snippet]) -> Result<(), FixtureError> {
    std::fs::create_dir_all(project_dir.join("src"))?;
    let manifest = format!(
        "[package]\nname = \"camber-websocket-examples\"\nversion = \"0.0.0\"\nedition = \"2024\"\npublish = false\n\n[workspace]\n\n[lib]\npath = \"src/lib.rs\"\n\n[dependencies]\ncamber = {{ path = \"{}\", features = [\"ws\"] }}\n",
        workspace_crate_path("camber")?.display()
    );
    std::fs::write(project_dir.join("Cargo.toml"), manifest)?;
    pin_workspace_lock(project_dir)?;
    let modules = snippets
        .iter()
        .enumerate()
        .map(|(index, snippet)| snippet_module(index, snippet));
    let lib = std::iter::once(format!("#![allow(unused)]\n\n{PRELUDE}"))
        .chain(modules)
        .collect::<Box<[String]>>()
        .join("\n");
    std::fs::write(project_dir.join("src/lib.rs"), lib)?;
    Ok(())
}

/// One snippet as a module. A statement body runs inside an async function
/// that returns `Result`, with the `router` the documents assume.
fn snippet_module(index: usize, snippet: &Snippet) -> String {
    let declares_items = snippet
        .body
        .lines()
        .any(|line| ITEM_STARTS.iter().any(|start| line.starts_with(start)));
    let body = match declares_items {
        true => snippet.body.to_string(),
        false => format!(
            "pub async fn snippet() -> Result<(), RuntimeError> {{\nlet mut router = Router::new();\n{}\nOk(())\n}}",
            snippet.body
        ),
    };
    format!(
        "// {}:{}\npub mod snippet_{index} {{\nuse crate::prelude::*;\n{body}\n}}\n",
        snippet.source, snippet.line
    )
}

#[test]
fn unclosed_rust_fence_fails() {
    let text = "intro\n\n```rust\nrouter.ws(\"/chat\", handler);\n";
    let error = websocket_snippets("doc.md", text).err();
    assert_eq!(
        error.map(|error| error.to_string()),
        Some("doc.md:3: ```rust fence is never closed".to_owned())
    );
}
