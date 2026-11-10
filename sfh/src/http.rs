// http.rs — minimal HTTP/1.1 server on std::net, mirroring the net/http behaviors
// this application depends on: keep-alive, chunked request/response bodies,
// Expect: 100-continue, HEAD suppression, 2 KB response buffering (Content-Length
// when the whole response fits, Transfer-Encoding: chunked otherwise).

use std::cell::RefCell;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::rc::Rc;
use std::sync::Arc;

const MAX_HEADER_BYTES: usize = 1 << 20; // net/http DefaultMaxHeaderBytes
const BUFFER_THRESHOLD: usize = 2048; // net/http response buffering

pub struct Request<'a> {
    pub method: String,
    pub path: String,      // decoded
    pub raw_query: String, // raw (still encoded)
    headers: Vec<(String, String)>,
    body: BodyReader<'a>,
}

impl<'a> Request<'a> {
    pub fn header(&self, name: &str) -> String {
        let lname = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| k.to_ascii_lowercase() == lname)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }
    pub fn body_mut(&mut self) -> &mut dyn Read {
        &mut self.body
    }
    pub fn query_get(&self, key: &str) -> Option<String> {
        query_param(&self.raw_query, key)
    }
}

// ─── percent / query decoding ────────────────────────────────────────────────

fn hexv(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Lenient percent-decoding (invalid escapes kept literal), like Go's unescape.
fn pct_decode(s: &str, plus_space: bool) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() + 1 && i + 2 < b.len() + 1 => {
                if i + 2 < b.len() {
                    if let (Some(h), Some(l)) = (hexv(b[i + 1]), hexv(b[i + 2])) {
                        out.push(h << 4 | l);
                        i += 3;
                        continue;
                    }
                }
                out.push(b'%');
                i += 1;
            }
            b'+' if plus_space => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Split the raw query into (key, value) pairs, decoding both ('+' → space).
pub fn query_params(raw: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for pair in raw.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        out.push((pct_decode(k, true), pct_decode(v, true)));
    }
    out
}

pub fn query_param(raw: &str, key: &str) -> Option<String> {
    query_params(raw)
        .into_iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
}

// ─── body reader ─────────────────────────────────────────────────────────────

struct BodyReader<'a> {
    io: &'a Rc<RefCell<Stream>>,
    kind: BodyKind,
    expect_continue: bool,
    sent_continue: bool,
    finished: bool,
    eof: bool,
}

enum BodyKind {
    None,
    Fixed { remaining: u64 },
    Chunked { state: ChunkState },
}

enum ChunkState {
    Size,
    Data { remaining: u64 },
    Done,
}

impl<'a> BodyReader<'a> {
    fn send_continue(&mut self) {
        if self.expect_continue && !self.sent_continue {
            self.sent_continue = true;
            let mut io = self.io.borrow_mut();
            let _ = io.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
            let _ = io.flush();
        }
    }
}

impl<'a> Read for BodyReader<'a> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.finished || self.eof {
            return Ok(0);
        }
        self.send_continue();
        match &mut self.kind {
            BodyKind::None => {
                self.eof = true;
                Ok(0)
            }
            BodyKind::Fixed { remaining } => {
                if *remaining == 0 {
                    self.eof = true;
                    return Ok(0);
                }
                let want = std::cmp::min(out.len() as u64, *remaining) as usize;
                let mut io = self.io.borrow_mut();
                let n = io.read(&mut out[..want])?;
                if n == 0 {
                    self.eof = true;
                    self.finished = true;
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "body truncated",
                    ));
                }
                *remaining -= n as u64;
                if *remaining == 0 {
                    self.eof = true;
                }
                Ok(n)
            }
            BodyKind::Chunked { state } => {
                loop {
                    match state {
                        ChunkState::Done => {
                            self.eof = true;
                            return Ok(0);
                        }
                        ChunkState::Size => {
                            let mut io = self.io.borrow_mut();
                            let mut line = Vec::new();
                            let n = read_crlf_line(&mut *io, &mut line, 128)?;
                            if n == 0 {
                                self.eof = true;
                                self.finished = true;
                                return Err(std::io::Error::new(
                                    std::io::ErrorKind::UnexpectedEof,
                                    "chunked body truncated",
                                ));
                            }
                            drop(io);
                            let s = String::from_utf8_lossy(&line);
                            let s = s.split(';').next().unwrap_or("").trim();
                            let sz = u64::from_str_radix(s, 16).map_err(|_| {
                                std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    "bad chunk size",
                                )
                            })?;
                            if sz == 0 {
                                // consume trailers until empty line
                                let mut io = self.io.borrow_mut();
                                loop {
                                    let mut t = Vec::new();
                                    let n = read_crlf_line(&mut *io, &mut t, 8192)?;
                                    if n == 0 || t.is_empty() {
                                        break;
                                    }
                                }
                                *state = ChunkState::Done;
                                self.eof = true;
                                return Ok(0);
                            }
                            *state = ChunkState::Data { remaining: sz };
                        }
                        ChunkState::Data { remaining } => {
                            let want = std::cmp::min(out.len() as u64, *remaining) as usize;
                            let mut io = self.io.borrow_mut();
                            let n = io.read(&mut out[..want])?;
                            if n == 0 {
                                self.eof = true;
                                self.finished = true;
                                return Err(std::io::Error::new(
                                    std::io::ErrorKind::UnexpectedEof,
                                    "chunked body truncated",
                                ));
                            }
                            *remaining -= n as u64;
                            if *remaining == 0 {
                                // consume chunk-ending CRLF
                                let mut crlf = [0u8; 2];
                                io.read_exact(&mut crlf)?;
                                *state = ChunkState::Size;
                            }
                            return Ok(n);
                        }
                    }
                }
            }
        }
    }
}

// ─── response writer ─────────────────────────────────────────────────────────

pub struct Response<'a> {
    io: &'a Rc<RefCell<Stream>>,
    status: u16,
    status_written: bool,
    headers: Vec<(String, String)>,
    buf: Vec<u8>,
    mode: RespMode,
    is_head: bool,
    pub close_after: bool,
    http10: bool,
    finished: bool,
    head_count: u64,
    head_overflow: bool, // HEAD body exceeded the buffer: Go would have gone chunked, no auto CL
    headers_sent: bool,
}

#[derive(PartialEq)]
enum RespMode {
    Detect,      // buffering; decide at first overflow / finish
    Chunked,     // TE: chunked (streaming)
    Close,       // HTTP/1.0 streaming: close-delimited
    Length(u64), // explicit Content-Length
}

impl<'a> Response<'a> {
    /// Go http.Header.Set with textproto.CanonicalMIMEHeaderKey canonicalization.
    pub fn set_header(&mut self, k: &str, v: &str) {
        let key = canonical_key(k);
        self.headers
            .retain(|(k2, _)| !k2.eq_ignore_ascii_case(&key));
        self.headers.push((key, v.to_string()));
    }
    pub fn del_header(&mut self, k: &str) {
        self.headers
            .retain(|(k2, _)| !k2.eq_ignore_ascii_case(k));
    }
    pub fn get_header(&self, k: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|(k2, _)| k2.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.to_string())
    }
    /// Streaming mode with an explicit Content-Length already set in the map
    /// (Go ServeContent style — CL appears in the sorted user-header block).
    pub fn set_mode_length(&mut self, n: u64) {
        self.mode = RespMode::Length(n);
    }
    pub fn write_header(&mut self, status: u16) {
        if self.status_written {
            return;
        }
        self.status = status;
        self.status_written = true;
    }
    pub fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if self.is_head || no_length_status(self.status) {
            // body not allowed: count only. Once the counted bytes exceed the
            // response buffer, Go would have flushed mid-handler (chunked
            // path) and thus never emits an automatic Content-Length — mark
            // that with Chunked so finish() omits it.
            if self.is_head {
                self.head_count += data.len() as u64;
                if self.head_count > BUFFER_THRESHOLD as u64 {
                    self.head_overflow = true;
                }
            }
            return Ok(data.len());
        }
        match self.mode {
            RespMode::Length(_) => {
                self.flush_headers_once(None)?;
                self.io.borrow_mut().write_all(data)?;
                Ok(data.len())
            }
            RespMode::Chunked => {
                self.flush_headers_once(None)?;
                self.write_chunk(data)?;
                Ok(data.len())
            }
            RespMode::Close => {
                self.flush_headers_once(None)?;
                self.io.borrow_mut().write_all(data)?;
                self.close_after = true;
                Ok(data.len())
            }
            RespMode::Detect => {
                self.buf.extend_from_slice(data);
                if self.buf.len() > BUFFER_THRESHOLD {
                    // switch to streaming
                    if self.http10 {
                        self.mode = RespMode::Close;
                        self.close_after = true;
                    } else {
                        self.mode = RespMode::Chunked;
                    }
                    let buf = std::mem::take(&mut self.buf);
                    self.flush_headers_once(None)?;
                    if self.mode == RespMode::Chunked {
                        self.write_chunk(&buf)?;
                    } else {
                        self.io.borrow_mut().write_all(&buf)?;
                    }
                }
                Ok(data.len())
            }
        }
    }

    fn write_chunk(&mut self, data: &[u8]) -> std::io::Result<()> {
        let mut io = self.io.borrow_mut();
        if !data.is_empty() {
            write!(io, "{:x}\r\n", data.len())?;
            io.write_all(data)?;
            io.write_all(b"\r\n")?;
        }
        Ok(())
    }

    /// Write the status line + headers exactly once, in net/http's order:
    /// user headers sorted alphabetically, then Date, then the computed
    /// Content-Length (if any), then Connection / Transfer-Encoding.
    fn flush_headers_once(&mut self, computed_cl: Option<u64>) -> std::io::Result<()> {
        if self.headers_sent {
            return Ok(());
        }
        self.headers_sent = true;
        let status = self.status;
        // Go writeStatusLine: echo the request's major version.
        let proto = if self.http10 { "HTTP/1.0" } else { "HTTP/1.1" };
        let mut head = format!("{} {} {}\r\n", proto, status, status_text(status));
        // net/http manages these itself (extraHeader); user Content-Length
        // intentionally stays in the sorted block.
        let is_auto = |k: &str| {
            k.eq_ignore_ascii_case("date")
                || k.eq_ignore_ascii_case("connection")
                || k.eq_ignore_ascii_case("transfer-encoding")
        };
        let mut user: Vec<&(String, String)> =
            self.headers.iter().filter(|(k, _)| !is_auto(k)).collect();
        user.sort_by(|a, b| a.0.cmp(&b.0));
        for (k, v) in user {
            head.push_str(k);
            head.push_str(": ");
            head.push_str(v);
            head.push_str("\r\n");
        }
        head.push_str(&format!(
            "Date: {}\r\n",
            crate::util::http_date(crate::util::now_unix())
        ));
        if let Some(n) = computed_cl {
            head.push_str(&format!("Content-Length: {}\r\n", n));
        }
        if self.http10 {
            if !self.close_after {
                head.push_str("Connection: keep-alive\r\n");
            }
        } else if self.close_after {
            head.push_str("Connection: close\r\n");
        }
        // Go deletes Transfer-Encoding for HEAD responses unconditionally.
        if self.mode == RespMode::Chunked && !self.is_head {
            head.push_str("Transfer-Encoding: chunked\r\n");
        }
        head.push_str("\r\n");
        self.io.borrow_mut().write_all(head.as_bytes())
    }

    /// Finish the response: flush buffered bytes, emit terminal chunk, etc.
    fn finish(&mut self) -> std::io::Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        let user_cl = self.get_header("Content-Length").is_some();
        if self.is_head {
            // Go: automatic Content-Length on HEAD only when the handler wrote
            // a non-zero, buffer-sized (never flushed) body and set no
            // Content-Length itself.
            let cl = if user_cl
                || no_length_status(self.status)
                || self.head_count == 0
                || self.head_overflow
            {
                None
            } else {
                Some(self.head_count)
            };
            self.flush_headers_once(cl)?;
            return self.io.borrow_mut().flush();
        }
        match self.mode {
            RespMode::Detect => {
                let total = self.buf.len() as u64;
                let cl = if user_cl || no_length_status(self.status) {
                    None
                } else {
                    Some(total)
                };
                self.flush_headers_once(cl)?;
                let buf = std::mem::take(&mut self.buf);
                if !no_length_status(self.status) && !buf.is_empty() {
                    self.io.borrow_mut().write_all(&buf)?;
                }
            }
            RespMode::Chunked => {
                self.flush_headers_once(None)?;
                self.io.borrow_mut().write_all(b"0\r\n\r\n")?;
            }
            RespMode::Close => {
                self.flush_headers_once(None)?;
                self.close_after = true;
            }
            RespMode::Length(_) => {
                self.flush_headers_once(None)?;
            }
        }
        self.io.borrow_mut().flush()
    }
}

/// textproto.CanonicalMIMEHeaderKey
fn canonical_key(k: &str) -> String {
    let mut out = String::with_capacity(k.len());
    let mut upper = true;
    for c in k.chars() {
        let valid = c.is_ascii_alphanumeric()
            || matches!(
                c,
                '!' | '#' | '%' | '&' | '\'' | '*' | '+' | '-' | '.' | '^' | '_' | '`' | '|' | '~'
            );
        if !valid {
            return k.to_string();
        }
        if upper {
            out.extend(c.to_uppercase());
        } else {
            out.extend(c.to_lowercase());
        }
        upper = c == '-';
    }
    out
}

impl Write for Response<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Response::write(self, buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn no_length_status(s: u16) -> bool {
    matches!(s, 304 | 204) || (100..200).contains(&s)
}

pub fn status_text(code: u16) -> &'static str {
    match code {
        200 => "OK",
        206 => "Partial Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        412 => "Precondition Failed",
        416 => "Requested Range Not Satisfiable",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        _ => "",
    }
}

// ─── server ──────────────────────────────────────────────────────────────────

/// Wraps the socket so a single `Rc<RefCell<Stream>>` serves both the read
/// (request parsing) and write (response) sides.
pub struct Stream {
    inner: BufReader<TcpStream>,
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf)
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.get_mut().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.get_mut().flush()
    }
}

impl std::ops::Deref for Stream {
    type Target = BufReader<TcpStream>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl std::ops::DerefMut for Stream {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

pub type HandlerFn = Arc<dyn Fn(&mut Request, &mut Response) + Send + Sync>;

pub fn serve(addr: &str, handler: HandlerFn) -> std::io::Result<()> {
    // Go's net accepts ":port" (all interfaces); Rust needs an explicit host.
    let addr = if let Some(p) = addr.strip_prefix(':') {
        format!("0.0.0.0:{}", p)
    } else {
        addr.to_string()
    };
    let listener = TcpListener::bind(addr)?;
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let h = Arc::clone(&handler);
                std::thread::spawn(move || {
                    let _ = handle_conn(s, h);
                });
            }
            Err(_) => continue,
        }
    }
    Ok(())
}

fn handle_conn(stream: TcpStream, handler: HandlerFn) -> std::io::Result<()> {
    let io = Rc::new(RefCell::new(Stream {
        inner: BufReader::with_capacity(65536, stream),
    }));
    loop {
        // ── parse request head ──
        let (method, target, version, headers) = {
            let mut io_ref = io.borrow_mut();
            match read_request_head(&mut io_ref) {
                Ok(Some(h)) => h,
                Ok(None) => return Ok(()), // clean EOF
                Err(_e) => {
                    // malformed head: 400 and close
                    let _ = io_ref.write_all(
                        b"HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\nContent-Length: 12\r\n\r\nBad Request\n",
                    );
                    let _ = io_ref.flush();
                    return Ok(());
                }
            }
        };

        // target → path + query
        let (raw_path, raw_query) = match target.split_once('?') {
            Some((p, q)) => (p, q.to_string()),
            None => (target.as_str(), String::new()),
        };
        // strip absolute-form prefix if present
        let raw_path = if raw_path.starts_with("http://") || raw_path.starts_with("https://") {
            match raw_path[raw_path.find("://").map(|i| i + 3).unwrap_or(0)..].find('/') {
                Some(i) => {
                    let start = raw_path.find("://").map(|i| i + 3).unwrap_or(0) + i;
                    &raw_path[start..]
                }
                None => "/",
            }
        } else {
            raw_path
        };
        let path = pct_decode(raw_path, false);

        let is_http10 = version == "HTTP/1.0";
        let conn_hdr = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("connection"))
            .map(|(_, v)| v.to_ascii_lowercase())
            .unwrap_or_default();
        let keep_alive = if is_http10 {
            conn_hdr.contains("keep-alive")
        } else {
            !conn_hdr.split(',').any(|t| t.trim() == "close")
        };

        // body framing
        let te = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("transfer-encoding"))
            .map(|(_, v)| v.to_ascii_lowercase())
            .unwrap_or_default();
        let cl = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, v)| v.trim().parse::<u64>().ok());
        let expect_continue = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("expect"))
            .map(|(_, v)| v.to_ascii_lowercase().contains("100-continue"))
            .unwrap_or(false);

        let kind = if te.contains("chunked") {
            BodyKind::Chunked {
                state: ChunkState::Size,
            }
        } else if let Some(n) = cl {
            if n == 0 {
                BodyKind::None
            } else {
                BodyKind::Fixed { remaining: n }
            }
        } else {
            BodyKind::None
        };

        let body = BodyReader {
            io: &io,
            kind,
            expect_continue,
            sent_continue: false,
            finished: false,
            eof: false,
        };

        let req_keep_alive = keep_alive;
        let mut req = Request {
            method,
            path,
            raw_query,
            headers,
            body,
        };
        let is_head = req.method == "HEAD";
        let mut resp = Response {
            io: &io,
            status: 200,
            status_written: false,
            headers: Vec::new(),
            buf: Vec::new(),
            mode: RespMode::Detect,
            is_head,
            close_after: !req_keep_alive,
            http10: is_http10,
            finished: false,
            head_count: 0,
            head_overflow: false,
            headers_sent: false,
        };

        handler(&mut req, &mut resp);
        resp.finish()?;

        // drain unread body so keep-alive framing stays intact
        if !resp.close_after {
            let mut sink = [0u8; 16384];
            let r = &mut req.body;
            if !r.eof {
                use std::io::Read as _;
                let mut total: u64 = 0;
                loop {
                    match r.read(&mut sink) {
                        Ok(0) => break,
                        Ok(n) => {
                            total += n as u64;
                            if total > 1 << 30 {
                                resp.close_after = true;
                                break;
                            }
                        }
                        Err(_) => {
                            resp.close_after = true;
                            break;
                        }
                    }
                }
            }
        }

        if resp.close_after {
            let _ = io.borrow_mut().flush();
            return Ok(());
        }
    }
}

/// Read one request head; returns None on clean EOF before any byte.
fn read_request_head(
    io: &mut BufReader<TcpStream>,
) -> std::io::Result<Option<(String, String, String, Vec<(String, String)>)>> {
    // request line
    let mut line = Vec::new();
    let n = read_crlf_line(io, &mut line, MAX_HEADER_BYTES)?;
    if n == 0 && line.is_empty() {
        // check for EOF
        return Ok(None);
    }
    let first = String::from_utf8_lossy(&line).into_owned();
    let mut it = first.split_whitespace();
    let (method, target, version) = match (it.next(), it.next(), it.next()) {
        (Some(m), Some(t), Some(v)) => (m.to_string(), t.to_string(), v.to_string()),
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "bad request line",
            ))
        }
    };
    // headers
    let mut headers = Vec::new();
    let mut total = line.len();
    loop {
        let mut hl = Vec::new();
        let n = read_crlf_line(io, &mut hl, MAX_HEADER_BYTES)?;
        total += n;
        if total > MAX_HEADER_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "header too large",
            ));
        }
        if hl.is_empty() {
            break;
        }
        let s = String::from_utf8_lossy(&hl).into_owned();
        if let Some((k, v)) = s.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    Ok(Some((method, target, version, headers)))
}

/// Read a CRLF- (or LF-) terminated line; returns bytes WITHOUT the terminator.
/// Returns (0 bytes, empty) on EOF.
fn read_crlf_line(
    io: &mut BufReader<TcpStream>,
    out: &mut Vec<u8>,
    cap: usize,
) -> std::io::Result<usize> {
    out.clear();
    let mut read_total = 0usize;
    loop {
        // use fill_buf/consume for efficiency
        let available = match io.fill_buf() {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if available.is_empty() {
            return Ok(read_total);
        }
        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            out.extend_from_slice(&available[..pos]);
            let consumed = pos + 1;
            io.consume(consumed);
            read_total += consumed;
            if out.last() == Some(&b'\r') {
                out.pop();
            }
            return Ok(read_total);
        } else {
            out.extend_from_slice(available);
            let n = available.len();
            io.consume(n);
            read_total += n;
            if read_total > cap {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "line too long",
                ));
            }
        }
    }
}

// Suppress dead-code warnings for placeholder plumbing
#[allow(dead_code)]
fn _unused(_: &TcpStream) {}
