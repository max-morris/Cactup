//! Minimal static-file preview server for `cactupdocs serve`.

use anyhow::Result;
use std::fs;
use std::path::Path;
use tiny_http::{Header, Request, Response, Server};

/// Serve files from `root` directory on `addr`.
///
/// - Maps `/` and paths ending `/` to `index.html`
/// - If a path has no extension, tries `.html`
/// - Sets Content-Type by extension
/// - Loops forever handling requests
pub fn serve(root: &Path, addr: &str) -> Result<()> {
    let server = Server::http(addr)
        .map_err(|e| anyhow::anyhow!("Failed to bind to {}: {}", addr, e))?;

    println!("Serving on http://{}", addr);

    for request in server.incoming_requests() {
        handle_request(request, root);
    }

    Ok(())
}

fn handle_request(request: Request, root: &Path) {
    let path = request.url();

    // Map / to index.html
    let requested_path = if path == "/" {
        "index.html".to_string()
    } else if path.ends_with('/') {
        format!("{}index.html", path)
    } else {
        path.to_string()
    };

    // Remove leading slash
    let requested_path = requested_path.trim_start_matches('/');

    // Try the requested path first
    let mut file_path = root.join(requested_path);
    let mut found = file_path.is_file();

    // If not found and no extension, try .html
    if !found && !requested_path.contains('.') {
        file_path = root.join(format!("{}.html", requested_path));
        found = file_path.is_file();
    }

    if found {
        match fs::read(&file_path) {
            Ok(content) => {
                let content_type = get_content_type(&file_path);
                let response = Response::from_data(content).with_header(
                    Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes())
                        .unwrap_or_else(|_| Header::from_bytes(&b"Content-Type"[..], b"text/html").unwrap()),
                );
                let _ = request.respond(response);
            }
            Err(_) => {
                let _ = request.respond(Response::from_string("500 Internal Server Error").with_status_code(500));
            }
        }
    } else {
        let _ = request.respond(Response::from_string("404 Not Found").with_status_code(404));
    }
}

fn get_content_type(path: &Path) -> String {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "application/javascript; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("woff2") => "font/woff2",
        Some("ico") => "image/x-icon",
        _ => "application/octet-stream",
    }
    .to_string()
}
