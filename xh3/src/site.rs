//! Embedded static website served to any request that is not a proxy
//! session. This is the "real site" camouflage layer.

use http::{header, Response, StatusCode};
use include_dir::{include_dir, Dir};

static SITE: Dir = include_dir!("$CARGO_MANIFEST_DIR/assets/site");

/// Build a realistic response for `path`. Unknown paths get the site's 404
/// page (with a 404 status) — no bare responses, no timing shortcuts.
pub fn respond(path: &str) -> Response<Vec<u8>> {
    let path = path.trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    let (status, body, mime) = match SITE.get_file(path) {
        Some(f) => {
            let mime = mime_guess::from_path(path)
                .first_or_octet_stream()
                .to_string();
            (StatusCode::OK, f.contents(), mime)
        }
        None => {
            // try /<path>/index.html for clean URLs
            let idx = format!("{path}/index.html");
            match SITE.get_file(&idx) {
                Some(f) => (
                    StatusCode::OK,
                    f.contents(),
                    "text/html; charset=utf-8".to_string(),
                ),
                None => (
                    StatusCode::NOT_FOUND,
                    SITE.get_file("404.html")
                        .map(|f| f.contents())
                        .unwrap_or(b"404 Not Found".as_slice()),
                    "text/html; charset=utf-8".to_string(),
                ),
            }
        }
    };

    let mut resp = Response::builder().status(status);
    let headers = resp.headers_mut().unwrap();
    headers.insert(header::CONTENT_TYPE, mime.parse().unwrap());
    headers.insert(header::CONTENT_LENGTH, body.len().to_string().parse().unwrap());
    headers.insert(header::SERVER, "nginx".parse().unwrap());
    headers.insert(header::DATE, httpdate::fmt_http_date(std::time::SystemTime::now()).parse().unwrap());
    headers.insert(header::CACHE_CONTROL, "no-cache".parse().unwrap());
    resp.body(body.to_vec()).unwrap()
}
