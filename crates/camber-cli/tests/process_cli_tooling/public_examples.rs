//! Compile WebSocket examples and HTTP fences marked `rust,no_run`.
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

/// Each document must contain at least one checked HTTP example.
const DOCUMENTS: [&str; 5] = [
    "README.md",
    "docs/guides/go-to-camber.md",
    "docs/guides/tokio-to-camber.md",
    "docs/reference/http.md",
    "docs/reference/middleware.md",
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
    pub use camber::tracing;
}
";

/// One selected Rust fence and where it came from.
struct Snippet {
    source: &'static str,
    line: usize,
    body: Box<str>,
    checked_http: bool,
}

#[test]
fn public_examples_compile() -> Result<(), FixtureError> {
    in_temp_dir(compile_examples)
}

/// Compile the selected examples together in one project under `dir`.
fn compile_examples(dir: &Path) -> Result<(), FixtureError> {
    let root = workspace_root()?;
    let mut snippets = context_snippets(dir)?;
    for document in DOCUMENTS {
        let text = std::fs::read_to_string(root.join(document))?;
        snippets.extend(selected_snippets(document, &text)?);
    }
    assert_inventory(&snippets);

    let project_dir = dir.join("public-examples");
    write_project(&project_dir, &snippets)?;
    cargo_succeeds(&project_dir, &["check", "--lib"], &inventory(&snippets))
}

/// The selected examples `camber context` writes into `llms.txt`.
fn context_snippets(dir: &Path) -> Result<Vec<Snippet>, FixtureError> {
    camber_context(dir)?;
    let text = std::fs::read_to_string(dir.join("llms.txt"))?;
    selected_snippets(CONTEXT_SOURCE, &text)
}

/// Select marked HTTP fences and every WebSocket fence. Unclosed fences fail.
fn selected_snippets(source: &'static str, text: &str) -> Result<Vec<Snippet>, FixtureError> {
    let mut snippets = Vec::new();
    let mut open: Option<(usize, bool, Vec<&str>)> = None;
    for (index, line) in text.lines().enumerate() {
        let fence = line.trim_start();
        open = match (open, fence.starts_with("```")) {
            (None, true) if fence.starts_with("```rust") => {
                Some((index + 1, fence == "```rust,no_run", Vec::new()))
            }
            (None, _) => None,
            (Some((start, checked, body)), true) => {
                snippets.extend(selected_snippet(source, start, checked, &body));
                None
            }
            (Some((start, checked, mut body)), false) => {
                body.push(line);
                Some((start, checked, body))
            }
        };
    }
    match open {
        None => Ok(snippets),
        Some((start, _, _)) => Err(FixtureError::new(format!(
            "{source}:{start}: ```rust fence is never closed"
        ))),
    }
}

fn selected_snippet(
    source: &'static str,
    line: usize,
    checked_http: bool,
    body: &[&str],
) -> Option<Snippet> {
    let body = body.join("\n");
    (checked_http || WEBSOCKET_MARKERS.iter().any(|marker| body.contains(marker))).then(|| {
        Snippet {
            source,
            line,
            body: body.into_boxed_str(),
            checked_http,
        }
    })
}

/// The selection is nonempty and reaches every document that must ship one.
fn assert_inventory(snippets: &[Snippet]) {
    assert_websocket_inventory(snippets);
    let missing: Box<[&str]> = std::iter::once(CONTEXT_SOURCE)
        .chain(DOCUMENTS)
        .filter(|source| {
            !snippets
                .iter()
                .any(|snippet| snippet.source == *source && snippet.checked_http)
        })
        .collect();
    assert!(
        missing.is_empty(),
        "no checked HTTP Rust example found in: {}",
        missing.join(", ")
    );
}

/// Keep the original WebSocket coverage when HTTP examples are added.
fn assert_websocket_inventory(snippets: &[Snippet]) {
    for source in std::iter::once(CONTEXT_SOURCE).chain(DOCUMENTS.into_iter().take(4)) {
        assert!(
            snippets.iter().any(|snippet| snippet.source == source
                && WEBSOCKET_MARKERS
                    .iter()
                    .any(|marker| snippet.body.contains(marker))),
            "no WebSocket Rust example found in: {source}",
        );
    }
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
        "[package]\nname = \"camber-public-examples\"\nversion = \"0.0.0\"\nedition = \"2024\"\npublish = false\n\n[workspace]\n\n[lib]\npath = \"src/lib.rs\"\n\n[dependencies]\ncamber = {{ path = \"{}\", features = [\"ws\"] }}\ntokio = {{ version = \"1\", features = [\"macros\", \"time\"] }}\n",
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
    let error = selected_snippets("doc.md", text).err();
    assert_eq!(
        error.map(|error| error.to_string()),
        Some("doc.md:3: ```rust fence is never closed".to_owned())
    );
}

#[test]
fn selection_keeps_marked_http_and_websocket_bodies() -> Result<(), FixtureError> {
    let text = "```rust\napplication_specific();\n```\n\
```rust,no_run\nrouter.get(\"/\", handler);\n```\n\
```rust\nrouter.ws(\"/chat\", handler);\n```\n";
    let snippets = selected_snippets("doc.md", text)?;
    let observed: Vec<_> = snippets
        .iter()
        .map(|snippet| (snippet.line, snippet.checked_http, snippet.body.as_ref()))
        .collect();
    assert_eq!(
        observed,
        [
            (4, true, "router.get(\"/\", handler);"),
            (7, false, "router.ws(\"/chat\", handler);"),
        ],
    );
    Ok(())
}
