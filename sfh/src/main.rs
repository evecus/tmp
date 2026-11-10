// main.rs — sfh (Static File Host), pure-Rust rewrite of the Go original.
// Every handler mirrors main.go byte-for-byte in observable behavior:
// routing (ServeMux cleanPath 301), public listing / file serving (ServeContent
// preconditions + ranges), /update panel (multipart POST forms), and the JSON
// API (ls/cat/upload/delete/mkdir/move/download with zip streaming).

mod http;
mod multipart;
mod tmpl;
mod util;
mod zip;

use std::fs;
use std::io::{Read, Seek, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use http::{query_param, Request, Response};
use tmpl::Val;
use util::{fmt_rfc3339, fmt_size, gobase, goclean, gojoin, gojson_string, now_unix_nanos};

const MAX_UPLOAD_SIZE: u64 = 10 << 30; // 10 GB total per request
const MAX_CAT_SIZE: u64 = 10 << 20; // 10 MB for /api/cat
const MAX_ZIP_WALK_SIZE: u64 = 20 << 30; // 20 GB total for zip download

struct State {
    data_root: String,
    auth_user: String,
    auth_pass: String,
}

fn main() {
    // ── parseArgs (mirrors main.go) ──
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut data_root = ".".to_string();
    let mut auth_user = String::new();
    let mut auth_pass = String::new();
    let mut port = "4040".to_string();
    let mut i = 0usize;
    while i < args.len() {
        if args[i] == "--auth" && i + 1 < args.len() {
            i += 1;
            if let Some((u, p)) = args[i].split_once(':') {
                auth_user = u.to_string();
                auth_pass = p.to_string();
            }
        } else if args[i] == "--port" && i + 1 < args.len() {
            i += 1;
            port = args[i].clone();
        } else if args[i] == "--dir" && i + 1 < args.len() {
            i += 1;
            data_root = args[i].clone();
        } else if !args[i].starts_with("--") {
            port = args[i].clone();
        }
        i += 1;
    }

    let data_root = abs_of(&data_root);
    if let Err(e) = fs::DirBuilder::new().mode(0o755).recursive(true).create(&data_root) {
        eprintln!("cannot create {}: {}", data_root, util::goerr("mkdir", &data_root, &e));
        std::process::exit(1);
    }

    let state = Arc::new(State {
        data_root: data_root.clone(),
        auth_user: auth_user.clone(),
        auth_pass: auth_pass.clone(),
    });

    // ── startup banner (byte-for-byte) ──
    let auth_disp = if !auth_user.is_empty() {
        format!("{}:***", auth_user)
    } else {
        "none".to_string()
    };
    print!("\n  sfh — Static File Host\n\n");
    print!("  📁 dir    {}\n  🌐 port   :{}\n  🔑 auth   {}\n\n", data_root, port, auth_disp);
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    if auth_user.is_empty() {
        eprintln!("  ⚠️  WARNING: no --auth set. /update and write API endpoints are DISABLED.");
        eprintln!("              Start with --auth user:pass to enable file management.\n");
    }
    print!("  Browse → http://localhost:{}/\n", port);
    print!("  Manage → http://localhost:{}/update  (needs auth)\n\n", port);
    print!("  curl API (read — public):\n");
    print!("    curl http://HOST:{}/api/ls\n", port);
    print!("    curl http://HOST:{}/api/ls?path=/subdir\n", port);
    print!("    curl http://HOST:{}/api/cat?path=/file.txt\n", port);
    print!("    curl http://HOST:{}/api/download?path=/file.txt -o file.txt\n\n", port);
    if !auth_user.is_empty() {
        print!("  curl API (write — requires -u {}:PASS):\n", auth_user);
        print!("    curl -u {}:PASS -X POST 'http://HOST:{}/api/mkdir?path=/dir'\n", auth_user, port);
        print!(
            "    curl -u {}:PASS -T file.txt 'http://HOST:{}/api/upload?path=/&name=file.txt'\n",
            auth_user, port
        );
        print!(
            "    curl -u {}:PASS -F 'files=@photo.png' 'http://HOST:{}/api/upload?path=/'\n",
            auth_user, port
        );
        print!(
            "    curl -u {}:PASS -X DELETE 'http://HOST:{}/api/delete?path=/file.txt'\n",
            auth_user, port
        );
        print!(
            "    curl -u {}:PASS -X POST 'http://HOST:{}/api/move?from=/a.txt&to=/b.txt'\n\n",
            auth_user, port
        );
    }
    let _ = std::io::stdout().flush();

    // ── serve ──
    let st = Arc::clone(&state);
    let handler: http::HandlerFn = Arc::new(move |req: &mut Request, resp: &mut Response| {
        route(&st, req, resp);
    });
    if let Err(e) = http::serve(&format!(":{}", port), handler) {
        eprintln!("error: {}", listen_err(&port, &e));
        std::process::exit(1);
    }
}

fn listen_err(port: &str, e: &std::io::Error) -> String {
    let msg = util::go_errno(e);
    format!("listen tcp :{}: bind: {}", port, msg)
}

// ─── path helpers ─────────────────────────────────────────────────────────────

fn cwd_str() -> String {
    std::env::current_dir()
        .map(|d| d.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Go filepath.Abs
fn abs_of(p: &str) -> String {
    if Path::new(p).is_absolute() {
        goclean(p)
    } else {
        gojoin(&cwd_str(), p)
    }
}

/// Go safePath: Clean("/"+rel), join under dataRoot, enforce root boundary.
fn safe_path(state: &State, rel: &str) -> Option<String> {
    let cleaned = goclean(&format!("/{}", rel));
    let joined = gojoin(&state.data_root, &cleaned);
    let abs = if Path::new(&joined).is_absolute() {
        goclean(&joined)
    } else {
        gojoin(&cwd_str(), &joined)
    };
    let root = abs_of(&state.data_root);
    if abs == root || abs.starts_with(&format!("{}/", root)) {
        Some(abs)
    } else {
        None
    }
}

/// Go filepath.Ext
fn file_ext(path: &str) -> String {
    let b = path.as_bytes();
    let mut i = b.len();
    while i > 0 {
        i -= 1;
        if b[i] == b'/' {
            break;
        }
        if b[i] == b'.' {
            return path[i..].to_string();
        }
    }
    String::new()
}

/// Go filepath.Dir
fn go_dir(path: &str) -> String {
    match path.rfind('/') {
        Some(0) => "/".to_string(),
        Some(i) => goclean(&path[..i]),
        None => ".".to_string(),
    }
}

fn mtime_unix(md: &fs::Metadata) -> i64 {
    match md.modified() {
        Ok(t) => match t.duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_secs() as i64,
            Err(e) => -(e.duration().as_secs() as i64),
        },
        Err(_) => 0,
    }
}

// ─── routing (net/http ServeMux behavior) ─────────────────────────────────────

/// net/http cleanPath: path.Clean + trailing slash preserved.
fn clean_path(p: &str) -> String {
    if p.is_empty() {
        return "/".to_string();
    }
    let p2 = if p.starts_with('/') { p.to_string() } else { format!("/{}", p) };
    let mut np = goclean(&p2);
    if p2.ends_with('/') && np != "/" {
        np.push('/');
    }
    np
}

/// Go shouldEscape(c, encodePath) escaping (url.URL.String for Path).
fn go_url_path_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &c in s.as_bytes() {
        match c {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' => out.push(c as char),
            b'-' | b'_' | b'.' | b'~' => out.push(c as char),
            b'$' | b'&' | b'+' | b',' | b'/' | b':' | b';' | b'=' | b'@' => out.push(c as char),
            _ => out.push_str(&format!("%{:02X}", c)),
        }
    }
    out
}

/// http.Redirect's hexEscapeNonASCII for the Location header.
fn hex_escape_non_ascii(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &c in s.as_bytes() {
        if c < 0x80 {
            out.push(c as char);
        } else {
            out.push_str(&format!("%{:02X}", c));
        }
    }
    out
}

/// Go http.Redirect
fn http_redirect(req: &Request, resp: &mut Response, url: &str, code: u16) {
    let had_ct = resp.get_header("Content-Type").is_some();
    resp.set_header("Location", &hex_escape_non_ascii(url));
    // Go Redirect: Content-Type for GET or HEAD, body only for GET.
    if !had_ct && (req.method == "GET" || req.method == "HEAD") {
        resp.set_header("Content-Type", "text/html; charset=utf-8");
    }
    resp.write_header(code);
    if !had_ct && req.method == "GET" {
        let body = format!(
            "<a href=\"{}\">{}</a>.\n\n",
            tmpl::html_escape(url),
            http::status_text(code)
        );
        let _ = resp.write(body.as_bytes());
    }
}

/// Go http.Error
fn http_error(resp: &mut Response, msg: &str, code: u16) {
    resp.set_header("Content-Type", "text/plain; charset=utf-8");
    resp.set_header("X-Content-Type-Options", "nosniff");
    resp.write_header(code);
    let _ = resp.write(format!("{}\n", msg).as_bytes());
}

fn http_not_found(resp: &mut Response) {
    http_error(resp, "404 page not found", 404);
}

fn route(state: &State, req: &mut Request, resp: &mut Response) {
    let path = req.path.clone();
    // ServeMux: cleanPath mismatch → 301 redirect (query preserved)
    let cleaned = clean_path(&path);
    if cleaned != path {
        let mut loc = go_url_path_escape(&cleaned);
        if !req.raw_query.is_empty() {
            loc = format!("{}?{}", loc, req.raw_query);
        }
        http_redirect(req, resp, &loc, 301);
        return;
    }
    if path.starts_with("/api/") {
        api_handler(state, req, resp);
    } else if path == "/update" || path.starts_with("/update/") {
        update_handler(state, req, resp);
    } else {
        public_handler(state, req, resp);
    }
}

// ─── auth ─────────────────────────────────────────────────────────────────────

fn check_auth(state: &State, req: &Request, require_creds: bool) -> bool {
    if state.auth_user.is_empty() && state.auth_pass.is_empty() {
        return !require_creds;
    }
    match basic_auth(req) {
        Some((u, p)) => u == state.auth_user && p == state.auth_pass,
        None => false,
    }
}

fn basic_auth(req: &Request) -> Option<(String, String)> {
    let auth = req.header("Authorization");
    const PREFIX: &str = "Basic ";
    if auth.len() < PREFIX.len() || !auth[..PREFIX.len()].eq_ignore_ascii_case(PREFIX) {
        return None;
    }
    let decoded = util::base64_decode(&auth[PREFIX.len()..])?;
    let cs = String::from_utf8(decoded).ok()?;
    let (u, p) = cs.split_once(':')?;
    Some((u.to_string(), p.to_string()))
}

fn require_auth(state: &State, req: &Request, resp: &mut Response) -> bool {
    if check_auth(state, req, true) {
        return true;
    }
    resp.set_header("Www-Authenticate", "Basic realm=\"sfh\"");
    http_error(resp, "Unauthorized", 401);
    false
}

// ─── listing helpers ──────────────────────────────────────────────────────────

struct DirEntryInfo {
    name: String,
    is_dir: bool,
    size: u64,
    mtime: i64,
}

fn read_entries(abs: &str) -> Vec<DirEntryInfo> {
    let Ok(rd) = fs::read_dir(abs) else {
        return Vec::new();
    };
    let mut out: Vec<DirEntryInfo> = Vec::new();
    for e in rd.flatten() {
        // byte-wise name sort order comes from the OS; keep raw names
        let name = e.file_name().to_string_lossy().into_owned();
        let ft = e.file_type().ok();
        let is_dir = match ft {
            Some(t) => t.is_dir(),
            None => fs::metadata(e.path()).map(|m| m.is_dir()).unwrap_or(false),
        };
        // Go DirEntry.Info() = lstat
        let (size, mtime) = match fs::symlink_metadata(e.path()) {
            Ok(md) => (md.len(), mtime_unix(&md)),
            Err(_) => (0, 0),
        };
        out.push(DirEntryInfo { name, is_dir, size, mtime });
    }
    // os.ReadDir sorts byte-wise by name
    out.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
    out
}

fn is_browser(req: &Request) -> bool {
    let ua = req.header("User-Agent").to_lowercase();
    if ua.is_empty() {
        return false;
    }
    for c in ["curl", "wget", "httpie", "python-requests", "go-http-client"] {
        if ua.contains(c) {
            return false;
        }
    }
    true
}

fn val_map(pairs: Vec<(&str, Val)>) -> Val {
    Val::M(Rc::new(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()))
}

/// Go readDir for templates: dirs first, then ToLower(name).
fn read_dir_fi(abs: &str) -> Vec<Val> {
    let mut entries = read_entries(abs);
    entries.sort_by(|a, b| {
        if a.is_dir != b.is_dir {
            return if a.is_dir {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            };
        }
        a.name.to_lowercase().cmp(&b.name.to_lowercase())
    });
    entries
        .into_iter()
        .map(|e| {
            let sz = if !e.is_dir { util::fmt_size(e.size) } else { String::new() };
            val_map(vec![
                ("Name", Val::S(e.name)),
                ("Size", Val::S(sz)),
                ("ModTime", Val::S(util::fmt_modtime(e.mtime))),
                ("IsDir", Val::B(e.is_dir)),
            ])
        })
        .collect()
}

fn crumbs(path: &str) -> Vec<Val> {
    let mut list = vec![val_map(vec![("Name", Val::S("~".into())), ("URL", Val::S("/".into()))])];
    let mut cur = String::new();
    for p in path.trim_matches('/').split('/') {
        if p.is_empty() {
            continue;
        }
        cur = format!("{}/{}", cur, p);
        list.push(val_map(vec![("Name", Val::S(p.into())), ("URL", Val::S(cur.clone()))]));
    }
    list
}

fn upd_crumbs(path: &str) -> Vec<Val> {
    let mut list = vec![val_map(vec![("Name", Val::S("~".into())), ("URL", Val::S("/update".into()))])];
    let mut cur = String::new();
    for p in path.trim_matches('/').split('/') {
        if p.is_empty() {
            continue;
        }
        cur = format!("{}/{}", cur, p);
        list.push(val_map(vec![
            ("Name", Val::S(p.into())),
            ("URL", Val::S(format!("/update{}", cur))),
        ]));
    }
    list
}

// ─── public handler ───────────────────────────────────────────────────────────

fn public_handler(state: &State, req: &mut Request, resp: &mut Response) {
    let p = req.path.clone();
    let Some(abs) = safe_path(state, &p) else {
        http_error(resp, "Forbidden", 403);
        return;
    };
    let md = match fs::metadata(&abs) {
        Ok(m) => m,
        Err(_) => {
            http_not_found(resp);
            return;
        }
    };

    if md.is_dir() {
        if !p.ends_with('/') {
            http_redirect(req, resp, &format!("{}/", p), 302);
            return;
        }
        if is_browser(req) {
            dir_page(resp, &p, &abs);
        } else {
            text_listing(resp, &abs);
        }
        return;
    }

    // files served raw (previewable render natively, others download)
    serve_file(resp, req, &abs);
}

fn dir_page(resp: &mut Response, p: &str, abs: &str) {
    resp.set_header("Content-Type", "text/html; charset=utf-8");
    let data = val_map(vec![
        ("Path", Val::S(p.to_string())),
        ("Crumbs", Val::L(Rc::new(crumbs(p.strip_suffix('/').unwrap_or(p))))),
        ("Files", Val::L(Rc::new(read_dir_fi(abs)))),
    ]);
    let html = tmpl::render(tmpl::DIR_TMPL, &data);
    let _ = resp.write(html.as_bytes());
}

fn text_listing(resp: &mut Response, abs: &str) {
    resp.set_header("Content-Type", "text/plain; charset=utf-8");
    let entries = read_entries(abs);
    let lines: Vec<String> = entries
        .iter()
        .map(|e| {
            if e.is_dir {
                format!("{}/", e.name)
            } else {
                e.name.clone()
            }
        })
        .collect();
    let body = format!("{}\n", lines.join("\n"));
    let _ = resp.write(body.as_bytes());
}

// ─── ServeContent port (preconditions + ranges) ───────────────────────────────

#[derive(PartialEq, Clone, Copy)]
enum Cond {
    None,
    False,
    True,
}

fn write_not_modified(resp: &mut Response) {
    resp.del_header("Content-Type");
    resp.del_header("Content-Length");
    resp.del_header("Content-Encoding");
    // Etag is never set by this application → Last-Modified kept
    resp.write_header(304);
}

fn check_if_match(req: &Request) -> Cond {
    let im = req.header("If-Match");
    if im.is_empty() {
        return Cond::None;
    }
    let mut im = im.as_str();
    loop {
        im = im.trim();
        if im.is_empty() {
            break;
        }
        if im.as_bytes()[0] == b',' {
            im = &im[1..];
            continue;
        }
        if im.as_bytes()[0] == b'*' {
            return Cond::True;
        }
        let Some((etag, remain)) = scan_etag(im) else { break };
        // no Etag is ever set → strong match impossible
        let _ = etag;
        im = remain;
    }
    Cond::False
}

fn check_if_unmodified_since(req: &Request, mtime: i64) -> Cond {
    let ius = req.header("If-Unmodified-Since");
    if ius.is_empty() || mtime == 0 {
        return Cond::None;
    }
    let Some(t) = util::parse_http_date(&ius) else { return Cond::None };
    if mtime <= t {
        Cond::True
    } else {
        Cond::False
    }
}

fn check_if_none_match(req: &Request) -> Cond {
    let inm = req.header("If-None-Match");
    if inm.is_empty() {
        return Cond::None;
    }
    let mut buf = inm.as_str();
    loop {
        buf = buf.trim();
        if buf.is_empty() {
            break;
        }
        if buf.as_bytes()[0] == b',' {
            buf = &buf[1..];
            continue;
        }
        if buf.as_bytes()[0] == b'*' {
            return Cond::False;
        }
        let Some((etag, remain)) = scan_etag(buf) else { break };
        // weak match against "" (no Etag set) never matches
        if etag.is_empty() {
            break;
        }
        buf = remain;
    }
    Cond::True
}

fn check_if_modified_since(req: &Request, mtime: i64) -> Cond {
    if req.method != "GET" && req.method != "HEAD" {
        return Cond::None;
    }
    let ims = req.header("If-Modified-Since");
    if ims.is_empty() || mtime == 0 {
        return Cond::None;
    }
    let Some(t) = util::parse_http_date(&ims) else { return Cond::None };
    if mtime <= t {
        Cond::False
    } else {
        Cond::True
    }
}

fn check_if_range(req: &Request, mtime: i64) -> Cond {
    if req.method != "GET" && req.method != "HEAD" {
        return Cond::None;
    }
    let ir = req.header("If-Range");
    if ir.is_empty() {
        return Cond::None;
    }
    if let Some((etag, _)) = scan_etag(&ir) {
        if !etag.is_empty() {
            // strong match against "" → never matches
            return Cond::False;
        }
    }
    if mtime == 0 {
        return Cond::False;
    }
    let Some(t) = util::parse_http_date(&ir) else { return Cond::False };
    if t == mtime {
        Cond::True
    } else {
        Cond::False
    }
}

/// Go scanETag
fn scan_etag(s: &str) -> Option<(String, &str)> {
    let s = s.trim();
    let start = if s.starts_with("W/") { 2 } else { 0 };
    let b = s.as_bytes();
    if b.len() - start < 2 || b[start] != b'"' {
        return None;
    }
    for i in start + 1..b.len() {
        let c = b[i];
        if c == 0x21 || (0x23..=0x7E).contains(&c) || c >= 0x80 {
            // allowed etag char
        } else if c == b'"' {
            return Some((s[..i + 1].to_string(), &s[i + 1..]));
        } else {
            return None;
        }
    }
    None
}

/// Go checkPreconditions → true when the request was answered (412/304).
fn check_preconditions(req: &Request, resp: &mut Response, mtime: i64) -> bool {
    let mut ch = check_if_match(req);
    if ch == Cond::None {
        ch = check_if_unmodified_since(req, mtime);
    }
    if ch == Cond::False {
        resp.write_header(412);
        return true;
    }
    match check_if_none_match(req) {
        Cond::False => {
            if req.method == "GET" || req.method == "HEAD" {
                write_not_modified(resp);
            } else {
                resp.write_header(412);
            }
            return true;
        }
        Cond::None => {
            if check_if_modified_since(req, mtime) == Cond::False {
                write_not_modified(resp);
                return true;
            }
        }
        Cond::True => {}
    }
    false
}

/// Go parseRange: Ok(ranges) or Err(no_overlap).
fn parse_range(s: &str, size: i64) -> Result<Vec<(i64, i64)>, bool> {
    if s.is_empty() {
        return Ok(Vec::new());
    }
    const B: &str = "bytes=";
    if !s.starts_with(B) {
        return Err(false);
    }
    let mut ranges: Vec<(i64, i64)> = Vec::new();
    let mut no_overlap = false;
    for ra in s[B.len()..].split(',') {
        let ra = ra.trim();
        if ra.is_empty() {
            continue;
        }
        let Some((start_s, end_s)) = ra.split_once('-') else {
            return Err(false);
        };
        let (start_s, end_s) = (start_s.trim(), end_s.trim());
        if start_s.is_empty() {
            if end_s.is_empty() || end_s.starts_with('-') {
                return Err(false);
            }
            let Ok(mut i) = end_s.parse::<i64>() else { return Err(false) };
            if i < 0 {
                return Err(false);
            }
            if i > size {
                i = size;
            }
            let start = size - i;
            ranges.push((start, size - start));
        } else {
            let Ok(i) = start_s.parse::<i64>() else { return Err(false) };
            if i < 0 {
                return Err(false);
            }
            if i >= size {
                no_overlap = true;
                continue;
            }
            let start = i;
            let len = if end_s.is_empty() {
                size - start
            } else {
                let Ok(mut e) = end_s.parse::<i64>() else { return Err(false) };
                if start > e {
                    return Err(false);
                }
                if e >= size {
                    e = size - 1;
                }
                e - start + 1
            };
            ranges.push((start, len));
        }
    }
    if no_overlap && ranges.is_empty() {
        return Err(true);
    }
    Ok(ranges)
}

fn random_boundary() -> String {
    // Go: 30 random bytes → 60 hex chars (fixed length)
    let mut seed = (now_unix_nanos() as u64)
        ^ ((std::process::id() as u64) << 32)
        ^ 0x9E3779B97F4A7C15;
    let mut out = String::with_capacity(60);
    for _ in 0..60 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        out.push_str(&format!("{:x}", (seed >> 60) & 0xF));
    }
    out
}

/// Go rangesMIMESize — exact byte size of the multipart/byteranges body.
fn ranges_mimes_size(ranges: &[(i64, i64)], ctype: &str, size: i64, boundary: &str) -> u64 {
    let mut total = 0u64;
    for (i, &(start, len)) in ranges.iter().enumerate() {
        let head = if i == 0 {
            format!("--{}\r\n", boundary)
        } else {
            format!("\r\n--{}\r\n", boundary)
        };
        let hdrs = format!(
            "Content-Range: bytes {}-{}/{}\r\nContent-Type: {}\r\n\r\n",
            start,
            start + len - 1,
            size,
            ctype
        );
        total += (head.len() + hdrs.len()) as u64 + len as u64;
    }
    total + format!("\r\n--{}--\r\n", boundary).len() as u64
}

fn to_http_error(e: &std::io::Error) -> (&'static str, u16) {
    match e.kind() {
        std::io::ErrorKind::NotFound => ("404 page not found", 404),
        std::io::ErrorKind::PermissionDenied => ("403 Forbidden", 403),
        _ => ("500 Internal Server Error", 500),
    }
}

/// Go http.ServeFile → serveContent (redirect=false: no trailing-slash/index
/// redirects — verified against the Go oracle).
fn serve_file(resp: &mut Response, req: &Request, abs: &str) {
    let mut file = match fs::File::open(abs) {
        Ok(f) => f,
        Err(e) => {
            let (m, c) = to_http_error(&e);
            http_error(resp, m, c);
            return;
        }
    };
    let md = match file.metadata() {
        Ok(m) => m,
        Err(e) => {
            let (m, c) = to_http_error(&e);
            http_error(resp, m, c);
            return;
        }
    };
    let size = md.len() as i64;
    let mtime = mtime_unix(&md);
    if mtime != 0 {
        resp.set_header("Last-Modified", &util::http_date(mtime));
    }
    if check_preconditions(req, resp, mtime) {
        return;
    }

    // If-Range may cancel the Range header
    let mut range_hdr = req.header("Range");
    if !range_hdr.is_empty() && check_if_range(req, mtime) == Cond::False {
        range_hdr.clear();
    }

    // Content-Type: pre-set (api download) → keep; else extension → sniff
    let ctype = match resp.get_header("Content-Type") {
        Some(ct) => ct,
        None => {
            let ext = file_ext(&gobase(abs));
            let mut ct = util::mime_by_extension(&ext);
            if ct.is_empty() {
                let mut buf = [0u8; 512];
                let n = file.read(&mut buf).unwrap_or(0);
                ct = util::detect_content_type(&buf[..n]);
                let _ = file.seek(std::io::SeekFrom::Start(0));
            }
            resp.set_header("Content-Type", &ct);
            ct
        }
    };

    let mut ranges = match parse_range(&range_hdr, size) {
        Ok(r) => r,
        Err(true) => {
            if size == 0 {
                Vec::new() // ignore range on empty files → 200
            } else {
                resp.set_header("Content-Range", &format!("bytes */{}", size));
                http_error(resp, "invalid range: failed to overlap", 416);
                return;
            }
        }
        Err(false) => {
            http_error(resp, "invalid range", 416);
            return;
        }
    };
    if ranges.iter().map(|r| r.1).sum::<i64>() > size {
        ranges.clear();
    }

    let mut code: u16 = 200;
    let mut send_size = size;
    let mut multipart_boundary: Option<String> = None;
    match ranges.len() {
        1 => {
            let (start, len) = ranges[0];
            if file.seek(std::io::SeekFrom::Start(start as u64)).is_err() {
                http_error(resp, "invalid range: failed to overlap", 416);
                return;
            }
            send_size = len;
            code = 206;
            resp.set_header(
                "Content-Range",
                &format!("bytes {}-{}/{}", start, start + len - 1, size),
            );
        }
        n if n > 1 => {
            let b = random_boundary();
            send_size = ranges_mimes_size(&ranges, &ctype, size, &b) as i64;
            code = 206;
            resp.set_header("Content-Type", &format!("multipart/byteranges; boundary={}", b));
            multipart_boundary = Some(b);
        }
        _ => {}
    }

    resp.set_header("Accept-Ranges", "bytes");
    resp.set_header("Content-Length", &send_size.to_string());
    resp.set_mode_length(send_size as u64);
    resp.write_header(code);

    if req.method == "HEAD" {
        return;
    }
    if let Some(b) = multipart_boundary {
        let mut first = true;
        for &(start, len) in &ranges {
            let head = if first {
                format!("--{}\r\n", b)
            } else {
                format!("\r\n--{}\r\n", b)
            };
            first = false;
            let part_head = format!(
                "{}Content-Range: bytes {}-{}/{}\r\nContent-Type: {}\r\n\r\n",
                head,
                start,
                start + len - 1,
                size,
                ctype
            );
            if resp.write(part_head.as_bytes()).is_err() {
                return;
            }
            if file.seek(std::io::SeekFrom::Start(start as u64)).is_err() {
                return;
            }
            let mut lim = (&mut file).take(len as u64);
            if std::io::copy(&mut lim, resp).is_err() {
                return;
            }
        }
        let _ = resp.write(format!("\r\n--{}--\r\n", b).as_bytes());
        return;
    }
    let mut lim = (&mut file).take(send_size as u64);
    let _ = std::io::copy(&mut lim, resp);
}

// ─── update panel ─────────────────────────────────────────────────────────────

fn update_handler(state: &State, req: &mut Request, resp: &mut Response) {
    if !require_auth(state, req, resp) {
        return;
    }
    let mut p = req.path.strip_prefix("/update").unwrap_or("").to_string();
    if p.is_empty() {
        p = "/".to_string();
    }
    if !p.ends_with('/') {
        p.push('/');
    }
    let Some(abs) = safe_path(state, &p) else {
        http_error(resp, "Forbidden", 403);
        return;
    };
    match fs::metadata(&abs) {
        Ok(md) if md.is_dir() => {
            if req.method == "POST" {
                update_post(state, req, resp, &p, &abs);
            } else {
                update_page(resp, &p, &abs);
            }
        }
        _ => {
            http_error(resp, "Not found", 404);
        }
    }
}

fn update_page(resp: &mut Response, p: &str, abs: &str) {
    resp.set_header("Content-Type", "text/html; charset=utf-8");
    let data = val_map(vec![
        ("Path", Val::S(p.to_string())),
        ("Crumbs", Val::L(Rc::new(upd_crumbs(p.strip_suffix('/').unwrap_or(p))))),
        ("Files", Val::L(Rc::new(read_dir_fi(abs)))),
    ]);
    let html = tmpl::render(tmpl::UPD_TMPL, &data);
    let _ = resp.write(html.as_bytes());
}

// ─── multipart form parsing (Go ParseMultipartForm equivalent) ────────────────

struct LimitedRead<'a> {
    inner: &'a mut dyn Read,
    remaining: u64,
}

impl Read for LimitedRead<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "http: request body too large",
            ));
        }
        let want = buf.len().min(self.remaining as usize);
        let n = self.inner.read(&mut buf[..want])?;
        self.remaining -= n as u64;
        Ok(n)
    }
}

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn multipart_boundary(ct: &str) -> Option<String> {
    let lower = ct.to_ascii_lowercase();
    let pos = lower.find("boundary=")?;
    let rest = &ct[pos + "boundary=".len()..];
    let end = rest.find(';').unwrap_or(rest.len());
    let v = rest[..end].trim();
    let v = v.strip_prefix('"').unwrap_or(v);
    let v = v.strip_suffix('"').unwrap_or(v);
    if v.is_empty() {
        None
    } else {
        Some(v.to_string())
    }
}

/// Parsed multipart form: values + staged files (field, filename, temp path).
struct StagedFile {
    field: String,
    filename: String,
    path: PathBuf,
}

struct StagedForm {
    values: Vec<(String, String)>,
    files: Vec<StagedFile>,
    _tmp: TempDir,
}

fn parse_multipart(req: &mut Request) -> Result<StagedForm, ()> {
    let ct = req.header("Content-Type");
    if !ct.starts_with("multipart/") {
        return Err(());
    }
    let Some(boundary) = multipart_boundary(&ct) else { return Err(()) };
    let mut dir = std::env::temp_dir();
    dir.push(format!("sfh-multipart-{}-{}", std::process::id(), now_unix_nanos()));
    if fs::DirBuilder::new().mode(0o755).recursive(true).create(&dir).is_err() {
        return Err(());
    }
    let tmp = TempDir(dir.clone());
    let mut limited = LimitedRead { inner: req.body_mut(), remaining: MAX_UPLOAD_SIZE };
    let mut mp = multipart::Multipart::new(&mut limited, &boundary);
    let mut values: Vec<(String, String)> = Vec::new();
    let mut files: Vec<StagedFile> = Vec::new();
    loop {
        let mut part = match mp.next_part() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(_) => return Err(()),
        };
        let field = part.name;
        // Go mime/multipart Part.FileName() applies filepath.Base: directory
        // components supplied by the client are stripped before any handler
        // sees the name (this is why upload_folder stores flat basenames).
        if !part.filename.is_empty() {
            part.filename = gobase(&part.filename);
        }
        if part.filename.is_empty() {
            if field.is_empty() {
                let _ = mp.consume_body(&mut |_| Ok(()));
                continue;
            }
            let mut buf: Vec<u8> = Vec::new();
            let r = mp.consume_body(&mut |c| {
                buf.extend_from_slice(c);
                Ok(())
            });
            if r.is_err() {
                return Err(());
            }
            values.push((field, String::from_utf8_lossy(&buf).into_owned()));
        } else if field.is_empty() {
            let _ = mp.consume_body(&mut |_| Ok(()));
        } else {
            let tmpf = dir.join(format!("f{}", files.len()));
            let mut f = match fs::File::create(&tmpf) {
                Ok(f) => f,
                Err(_) => return Err(()),
            };
            let r = mp.consume_body(&mut |c| f.write_all(c));
            if r.is_err() {
                return Err(());
            }
            files.push(StagedFile { field, filename: part.filename, path: tmpf });
        }
    }
    Ok(StagedForm { values, files, _tmp: tmp })
}

/// Look up a form value the way Go's r.FormValue does: URL query first,
/// then multipart form values (first occurrence wins).
fn form_value(req: &Request, values: &[(String, String)], key: &str) -> Option<String> {
    if let Some(v) = query_param(&req.raw_query, key) {
        return Some(v);
    }
    values.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
}

fn update_post(state: &State, req: &mut Request, resp: &mut Response, dir: &str, abs_dir: &str) {
    // MaxBytesReader + ParseMultipartForm: any failure → 400
    let form = match parse_multipart(req) {
        Ok(v) => v,
        Err(_) => {
            http_error(resp, "request too large or malformed", 400);
            return;
        }
    };
    let values = &form.values;
    let action = form_value(req, values, "action").unwrap_or_default();

    match action.as_str() {
        "upload" => {
            for f in &form.files {
                let safe_name = gobase(&f.filename);
                let Some(dst) = safe_path(state, &gojoin(dir, &safe_name)) else { continue };
                let Ok(mut out) = fs::File::create(&dst) else { continue };
                if let Ok(mut src) = fs::File::open(&f.path) {
                    let _ = std::io::copy(&mut src, &mut out);
                }
            }
        }
        "upload_folder" => {
            for f in form.files.iter().filter(|f| f.field == "files") {
                let rel_name = goclean(&format!("/{}", f.filename));
                let Some(dst) = safe_path(state, &gojoin(dir, &rel_name)) else { continue };
                if fs::DirBuilder::new()
                    .mode(0o755)
                    .recursive(true)
                    .create(go_dir(&dst))
                    .is_err()
                {
                    continue;
                }
                let Ok(mut out) = fs::File::create(&dst) else { continue };
                if let Ok(mut src) = fs::File::open(&f.path) {
                    let _ = std::io::copy(&mut src, &mut out);
                }
            }
        }
        "mkdir" => {
            let name = form_value(req, values, "name").unwrap_or_default();
            let name = name.trim().to_string();
            if !name.is_empty() && !name.contains('/') && !name.contains('\\') {
                let _ = fs::DirBuilder::new().mode(0o755).recursive(true).create(gojoin(abs_dir, &name));
            }
        }
        "delete" => {
            let name = form_value(req, values, "name").unwrap_or_default();
            if !name.is_empty() {
                if let Some(target) = safe_path(state, &gojoin(dir, &name)) {
                    let root = abs_of(&state.data_root);
                    if target != root {
                        let _ = remove_all(&target);
                    }
                }
            }
        }
        "rename" => {
            let old = form_value(req, values, "old").unwrap_or_default();
            let nw = form_value(req, values, "new").unwrap_or_default();
            if !old.is_empty() && !nw.is_empty() && !nw.contains('/') && !nw.contains('\\') {
                if let (Some(old_a), Some(new_a)) = (
                    safe_path(state, &gojoin(dir, &old)),
                    safe_path(state, &gojoin(dir, &nw)),
                ) {
                    let _ = go_rename(&old_a, &new_a);
                }
            }
        }
        _ => {}
    }
    http_redirect(req, resp, &format!("/update{}", dir), 303);
}

/// Go os.RemoveAll: missing path is not an error.
fn remove_all(path: &str) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("RemoveAll {}: {}", path, util::go_errno(&e))),
        Ok(md) => {
            let r = if md.is_dir() {
                fs::remove_dir_all(path)
            } else {
                fs::remove_file(path)
            };
            r.map_err(|e| format!("RemoveAll {}: {}", path, util::go_errno(&e)))
        }
    }
}

/// Go os.Rename semantics (os/file_unix.go): when the destination exists and
/// is a directory, fail with EEXIST without invoking the kernel — even when
/// old == new — unless both names resolve to the same file on a
/// case-insensitive filesystem. Otherwise fall through to the rename syscall.
fn go_rename(old: &str, new: &str) -> Result<(), String> {
    if let Ok(fi) = fs::symlink_metadata(new) {
        if fi.is_dir() {
            return match fs::symlink_metadata(old) {
                Err(e) => Err(format!(
                    "rename {} {}: lstat {}: {}",
                    old,
                    new,
                    old,
                    util::go_errno(&e)
                )),
                Ok(ofi) => {
                    if new == old || !same_file(&fi, &ofi) {
                        Err(format!("rename {} {}: file exists", old, new))
                    } else {
                        fs::rename(old, new).map_err(|e| {
                            format!("rename {} {}: {}", old, new, util::go_errno(&e))
                        })
                    }
                }
            };
        }
    }
    fs::rename(old, new).map_err(|e| format!("rename {} {}: {}", old, new, util::go_errno(&e)))
}

fn same_file(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino()
}

// ─── JSON API ─────────────────────────────────────────────────────────────────

fn j_ok(resp: &mut Response, data: &str) {
    let body = format!("{{\"data\":{},\"ok\":true}}\n", data);
    let _ = resp.write(body.as_bytes());
}

fn j_err(resp: &mut Response, msg: &str, code: u16) {
    resp.write_header(code);
    let body = format!("{{\"error\":{},\"ok\":false}}\n", gojson_string(msg.as_bytes()));
    let _ = resp.write(body.as_bytes());
}

fn qp(req: &Request) -> String {
    req.query_get("path").unwrap_or_else(|| "/".to_string())
}

fn api_handler(state: &State, req: &mut Request, resp: &mut Response) {
    let sub = req.path.strip_prefix("/api").unwrap_or("").trim_matches('/').to_string();
    let is_write = req.method == "POST" || req.method == "DELETE" || req.method == "PUT";
    if is_write && !require_auth(state, req, resp) {
        return;
    }
    resp.set_header("Content-Type", "application/json; charset=utf-8");
    match sub.as_str() {
        "ls" | "" => api_ls(state, req, resp),
        "cat" => api_cat(state, req, resp),
        "upload" => api_upload(state, req, resp),
        "delete" => api_delete(state, req, resp),
        "mkdir" => api_mkdir(state, req, resp),
        "move" => api_move(state, req, resp),
        "download" => api_download(state, req, resp),
        _ => j_err(resp, &format!("unknown: {}", sub), 404),
    }
}

fn api_ls(state: &State, req: &mut Request, resp: &mut Response) {
    let rel = qp(req);
    let Some(abs) = safe_path(state, &rel) else {
        j_err(resp, "invalid path", 400);
        return;
    };
    let entries = match fs::read_dir(&abs) {
        Ok(rd) => rd,
        Err(e) => {
            j_err(resp, &util::goerr("open", &abs, &e), 500);
            return;
        }
    };
    let mut list: Vec<(String, bool, u64, i64)> = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let is_dir = match e.file_type() {
            Ok(t) => t.is_dir(),
            Err(_) => fs::metadata(e.path()).map(|m| m.is_dir()).unwrap_or(false),
        };
        let (size, mtime) = match fs::symlink_metadata(e.path()) {
            Ok(md) => (md.len(), mtime_unix(&md)),
            Err(_) => (0, 0),
        };
        list.push((name, is_dir, size, mtime));
    }
    list.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let base = rel.strip_suffix('/').unwrap_or(&rel);
    let items: Vec<String> = list
        .iter()
        .map(|(name, is_dir, size, mtime)| {
            let fp = format!("{}/{}", base, name);
            let mut s = format!(
                "{{\"name\":{},\"path\":{},\"is_dir\":{}",
                gojson_string(name.as_bytes()),
                gojson_string(fp.as_bytes()),
                is_dir
            );
            if !*is_dir && *size != 0 {
                s.push_str(&format!(",\"size\":{}", size));
            }
            s.push_str(&format!(",\"mod_time\":{}}}", gojson_string(fmt_rfc3339(*mtime).as_bytes())));
            s
        })
        .collect();
    j_ok(resp, &format!("[{}]", items.join(",")));
}

// safe_path needs State; api handlers receive it explicitly.

fn api_cat(state: &State, req: &mut Request, resp: &mut Response) {
    let Some(abs) = safe_path(state, &qp(req)) else {
        j_err(resp, "invalid path", 400);
        return;
    };
    let md = match fs::metadata(&abs) {
        Ok(m) => m,
        Err(e) => {
            j_err(resp, &util::goerr("stat", &abs, &e), 500);
            return;
        }
    };
    if md.len() > MAX_CAT_SIZE {
        j_err(resp, &format!("file too large for cat (max {})", fmt_size(MAX_CAT_SIZE)), 400);
        return;
    }
    match fs::read(&abs) {
        Ok(b) => {
            j_ok(resp, &gojson_string(&b));
        }
        Err(e) => {
            let op = if md.is_dir() { "read" } else { "open" };
            j_err(resp, &util::goerr(op, &abs, &e), 500);
        }
    }
}

fn api_upload(state: &State, req: &mut Request, resp: &mut Response) {
    let dir = qp(req);
    let Some(abs_dir) = safe_path(state, &dir) else {
        j_err(resp, "invalid path", 400);
        return;
    };
    let _ = fs::DirBuilder::new().mode(0o755).recursive(true).create(&abs_dir);

    let ct = req.header("Content-Type");
    if ct.starts_with("multipart/") {
        let form = match parse_multipart(req) {
            Ok(v) => v,
            Err(_) => {
                j_err(resp, "request too large or malformed", 400);
                return;
            }
        };
        let mut up: Vec<String> = Vec::new();
        for f in &form.files {
            let safe_name = gobase(&f.filename);
            let Some(dst) = safe_path(state, &gojoin(&dir, &safe_name)) else { continue };
            let Ok(mut out) = fs::File::create(&dst) else { continue };
            if let Ok(mut src) = fs::File::open(&f.path) {
                let _ = std::io::copy(&mut src, &mut out);
            }
            up.push(safe_name);
        }
        let items: Vec<String> = up.iter().map(|s| gojson_string(s.as_bytes())).collect();
        let data = if items.is_empty() {
            "null".to_string()
        } else {
            format!("[{}]", items.join(","))
        };
        j_ok(resp, &format!("{{\"uploaded\":{}}}", data));
        return;
    }

    // raw-body upload
    let name_q = req.query_get("name").unwrap_or_default();
    let mut name = gobase(&name_q);
    if name.is_empty() || name == "." {
        name = format!("upload_{}", now_unix_nanos());
    }
    let Some(dst) = safe_path(state, &gojoin(&dir, &name)) else {
        j_err(resp, "invalid name", 400);
        return;
    };
    let mut f = match fs::File::create(&dst) {
        Ok(f) => f,
        Err(e) => {
            j_err(resp, &util::goerr("open", &dst, &e), 500);
            return;
        }
    };
    let mut limited = LimitedRead { inner: req.body_mut(), remaining: MAX_UPLOAD_SIZE };
    let n = std::io::copy(&mut limited, &mut f).unwrap_or(0);
    let exceeded = limited.remaining == 0;
    if exceeded {
        resp.close_after = true;
    }
    j_ok(resp, &format!("{{\"bytes\":{},\"uploaded\":{}}}", n, gojson_string(name.as_bytes())));
}

fn api_delete(state: &State, req: &mut Request, resp: &mut Response) {
    let Some(abs) = safe_path(state, &qp(req)) else {
        j_err(resp, "invalid path", 400);
        return;
    };
    let root = abs_of(&state.data_root);
    if abs == root {
        j_err(resp, "cannot delete root", 400);
        return;
    }
    if let Err(e) = remove_all(&abs) {
        j_err(resp, &e, 500);
        return;
    }
    j_ok(resp, "\"deleted\"");
}

fn api_mkdir(state: &State, req: &mut Request, resp: &mut Response) {
    let Some(abs) = safe_path(state, &qp(req)) else {
        j_err(resp, "invalid path", 400);
        return;
    };
    if let Err(e) = fs::DirBuilder::new().mode(0o755).recursive(true).create(&abs) {
        j_err(resp, &util::goerr("mkdir", &abs, &e), 500);
        return;
    }
    j_ok(resp, "\"created\"");
}

fn api_move(state: &State, req: &mut Request, resp: &mut Response) {
    let from = req.query_get("from").unwrap_or_default();
    let to = req.query_get("to").unwrap_or_default();
    let (abs_from, abs_to) = match (safe_path(state, &from), safe_path(state, &to)) {
        (Some(a), Some(b)) => (a, b),
        _ => {
            j_err(resp, "invalid path", 400);
            return;
        }
    };
    if let Err(msg) = go_rename(&abs_from, &abs_to) {
        // Go LinkError: "rename <old> <new>: <err>"
        j_err(resp, &msg, 500);
        return;
    }
    j_ok(resp, "\"moved\"");
}

fn api_download(state: &State, req: &mut Request, resp: &mut Response) {
    let Some(abs) = safe_path(state, &qp(req)) else {
        http_error(resp, "invalid path", 400);
        return;
    };
    let md = match fs::metadata(&abs) {
        Ok(m) => m,
        Err(_) => {
            http_error(resp, "not found", 404);
            return;
        }
    };
    if md.is_dir() {
        resp.set_header(
            "Content-Disposition",
            &format!("attachment; filename=\"{}.zip\"", file_name_of(&abs)),
        );
        resp.set_header("Content-Type", "application/zip");
        let mut zw = zip::ZipWriter::new(resp);
        let mut total: u64 = 0;
        let stopped = walk_zip(&abs, &abs, &mut zw, &mut total);
        let _ = stopped;
        let _ = zw.finish();
        return;
    }
    resp.set_header("Content-Disposition", &format!("attachment; filename=\"{}\"", file_name_of(&abs)));
    serve_file(resp, req, &abs);
}

/// Go filepath.Base of an absolute path (no cleaning semantics needed here).
fn file_name_of(abs: &str) -> String {
    let trimmed = abs.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(i) => trimmed[i + 1..].to_string(),
        None => trimmed.to_string(),
    }
}

/// Go filepath.Walk DFS with byte-sorted entries; returns true when the
/// 20 GB limit stopped it. Directory entries are skipped; unreadable files
/// still produce (empty) zip entries, matching Go.
fn walk_zip<W: Write>(root: &str, dir: &str, zw: &mut zip::ZipWriter<W>, total: &mut u64) -> bool {
    let md = match fs::symlink_metadata(dir) {
        Ok(m) => m,
        Err(_) => return false,
    };
    if !md.is_dir() {
        // root itself a file/symlink: single "." entry (Go filepath.Rel → ".")
        return add_zip_entry(root, dir, ".", zw, total);
    }
    let entries = read_entries(dir);
    for e in entries {
        let path = format!("{}/{}", dir.trim_end_matches('/'), e.name);
        if e.is_dir {
            if walk_zip(root, &path, zw, total) {
                return true;
            }
        } else {
            let rel = if path == root {
                ".".to_string()
            } else {
                path[root.len() + 1..].to_string()
            };
            if add_zip_entry(root, &path, &rel, zw, total) {
                return true;
            }
        }
    }
    false
}

fn add_zip_entry<W: Write>(
    _root: &str,
    path: &str,
    rel: &str,
    zw: &mut zip::ZipWriter<W>,
    total: &mut u64,
) -> bool {
    let md = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return false,
    };
    *total += md.len();
    if *total > MAX_ZIP_WALK_SIZE {
        return true; // zip size limit exceeded → stop
    }
    if zw.start_file(rel).is_err() {
        return false;
    }
    // Go: zw.Create succeeded → entry exists even if open/copy fails
    if let Ok(mut f) = fs::File::open(path) {
        let mut buf = [0u8; 65536];
        loop {
            match f.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if zw.write_data(&buf[..n]).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    }
    let _ = zw.finish_file_if_open();
    false
}
