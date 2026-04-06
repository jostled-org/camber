use super::Response;
use std::path::Path;

/// Serve a static file from `base_dir` using the relative `file_path`.
///
/// The entire file is read into memory before sending the response.
/// This is designed for small assets (HTML, CSS, JS, images) — not for
/// streaming large files. For large file serving, use a dedicated file
/// server or CDN.
///
/// Returns 404 for missing files and directory traversal attempts.
/// Detects Content-Type from the file extension.
pub fn serve_file(base_dir: &Path, file_path: &str) -> Response {
    let requested = base_dir.join(file_path);

    let canonical = match requested.canonicalize() {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!(path = %requested.display(), error = %err, "static file resolve failed");
            return Response::text_raw(404, "not found");
        }
    };

    let base_canonical = match base_dir.canonicalize() {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!(path = %base_dir.display(), error = %err, "static file base dir resolve failed");
            return Response::text_raw(404, "not found");
        }
    };

    if !canonical.starts_with(&base_canonical) {
        return Response::text_raw(404, "not found");
    }

    match std::fs::read(&canonical) {
        Ok(contents) => {
            let content_type = content_type_for(canonical.extension().and_then(|e| e.to_str()));
            Response::bytes_raw(200, contents).with_content_type(content_type)
        }
        Err(err) => {
            tracing::warn!(path = %canonical.display(), error = %err, "static file read failed");
            Response::text_raw(404, "not found")
        }
    }
}

fn content_type_for(ext: Option<&str>) -> &'static str {
    match ext {
        Some("html") => "text/html",
        Some("css") => "text/css",
        Some("js") => "application/javascript",
        Some("json") => "application/json",
        Some("txt") => "text/plain",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    }
}
