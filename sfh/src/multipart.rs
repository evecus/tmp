// multipart.rs — streaming multipart/form-data parser.
// Streams file parts directly to their destination without buffering whole
// uploads in memory (Go's mime/multipart behaves the same for large files).

use std::io::{self, Read};

pub struct Part {
    pub name: String,
    pub filename: String,
}

pub struct Multipart<'a> {
    src: &'a mut dyn Read,
    dash_boundary: Vec<u8>, // "--" + boundary
    buf: Vec<u8>,
    eof: bool,
    started: bool,
    done: bool,
    /// bytes kept as lookahead while streaming a body (pattern length - 1)
    body_state: BodyState,
}

enum BodyState {
    Idle,
    Streaming,
    Finished,
}

const CHUNK: usize = 65536;

impl<'a> Multipart<'a> {
    pub fn new(src: &'a mut dyn Read, boundary: &str) -> Self {
        Multipart {
            src,
            dash_boundary: format!("--{boundary}").into_bytes(),
            buf: Vec::new(),
            eof: false,
            started: false,
            done: false,
            body_state: BodyState::Idle,
        }
    }

    fn fill(&mut self, need: usize) -> io::Result<bool> {
        // ensure at least `need` bytes buffered (or EOF)
        while !self.eof && self.buf.len() < need {
            let mut chunk = [0u8; CHUNK];
            let n = self.src.read(&mut chunk)?;
            if n == 0 {
                self.eof = true;
                break;
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
        Ok(!self.eof)
    }

    /// If the buffer contains the dash-boundary, consume through it (and the
    /// delimiter line terminator). Returns Ok(Some(true)) when the final "--"
    /// delimiter follows, Ok(Some(false)) when another part follows, Ok(None)
    /// when no delimiter is buffered.
    fn take_delim(&mut self) -> io::Result<Option<bool>> {
        let db_len = self.dash_boundary.len();
        let Some(pos) = find(&self.buf, &self.dash_boundary) else {
            return Ok(None);
        };
        self.buf.drain(..pos + db_len);
        let _ = self.fill(db_len + 4)?;
        if self.buf.starts_with(b"--") {
            return Ok(Some(true));
        }
        if self.buf.starts_with(b"\r\n") {
            self.buf.drain(..2);
        } else if !self.buf.is_empty() && self.buf[0] == b'\n' {
            self.buf.drain(..1);
        }
        Ok(Some(false))
    }

    /// Advance to the next part; returns None at the end of the stream.
    pub fn next_part(&mut self) -> io::Result<Option<Part>> {
        if self.done {
            return Ok(None);
        }
        if !self.started {
            self.started = true;
            // scan preamble for the first delimiter line: --boundary\r\n
            loop {
                match self.take_delim()? {
                    Some(true) => {
                        self.done = true;
                        return Ok(None);
                    }
                    Some(false) => break,
                    None => {}
                }
                // keep only a possible partial delimiter at the tail
                if self.buf.len() > self.dash_boundary.len() {
                    let cut = self.buf.len() - self.dash_boundary.len();
                    self.buf.drain(..cut);
                }
                if self.eof {
                    self.done = true;
                    return Ok(None); // EOF without any delimiter
                }
                if !self.fill(self.buf.len() + CHUNK)? {
                    // EOF: loop once more so take_delim scans the full buffer
                }
            }
        }

        // read headers
        let headers: Vec<(String, String)> = loop {
            self.fill(4)?;
            if let Some(pos) = find(&self.buf, b"\r\n\r\n") {
                let hdr_bytes: Vec<u8> = self.buf[..pos].to_vec();
                self.buf.drain(..pos + 4);
                break parse_headers(&hdr_bytes);
            }
            if self.eof {
                // unterminated: treat as end
                self.done = true;
                return Ok(None);
            }
            self.fill(self.buf.len() + CHUNK)?;
        };

        let mut name = String::new();
        let mut filename = String::new();
        for (k, v) in &headers {
            if k.eq_ignore_ascii_case("content-disposition") {
                let (disp, params) = parse_cd(v);
                if disp.eq_ignore_ascii_case("form-data") || disp.eq_ignore_ascii_case("file") {
                    if let Some(n) = params.iter().find(|(k, _)| k == "name") {
                        name = n.1.clone();
                    }
                    if let Some(f) = params.iter().find(|(k, _)| k == "filename") {
                        filename = f.1.clone();
                    }
                }
            }
        }
        self.body_state = BodyState::Streaming;
        Ok(Some(Part { name, filename }))
    }

    /// Stream the current part's body into `sink`. Returns the number of bytes.
    /// The sink may return a custom error to abort (propagated).
    pub fn consume_body(
        &mut self,
        sink: &mut dyn FnMut(&[u8]) -> io::Result<()>,
    ) -> io::Result<u64> {
        if !matches!(self.body_state, BodyState::Streaming) {
            return Ok(0);
        }
        let pat = {
            let mut p = b"\r\n".to_vec();
            p.extend_from_slice(&self.dash_boundary);
            p
        };
        let mut total: u64 = 0;
        loop {
            // look for \r\n--boundary
            if let Some(pos) = find(&self.buf, &pat) {
                sink(&self.buf[..pos])?;
                total += pos as u64;
                self.buf.drain(..pos + pat.len());
                // after boundary: "--" (final) or \r\n (next part) or EOF
                self.fill(4)?;
                if self.buf.starts_with(b"--") {
                    self.buf.drain(..2);
                    self.body_state = BodyState::Finished;
                    self.done = true;
                    return Ok(total);
                }
                if self.buf.starts_with(b"\r\n") {
                    self.buf.drain(..2);
                } else if !self.buf.is_empty() && self.buf[0] == b'\n' {
                    self.buf.drain(..1);
                }
                self.body_state = BodyState::Idle;
                return Ok(total);
            }
            // deliver all but the tail that might hold a partial pattern
            let keep = pat.len().saturating_sub(1);
            if self.buf.len() > keep {
                let cut = self.buf.len() - keep;
                sink(&self.buf[..cut])?;
                total += cut as u64;
                self.buf.drain(..cut);
            }
            if self.eof {
                // body ended without final boundary — malformed; deliver rest
                sink(&self.buf)?;
                self.buf.clear();
                self.body_state = BodyState::Finished;
                self.done = true;
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "multipart: missing final boundary",
                ));
            }
            self.fill(self.buf.len() + CHUNK)?;
        }
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

fn parse_headers(raw: &[u8]) -> Vec<(String, String)> {
    let text = String::from_utf8_lossy(raw);
    let mut out = Vec::new();
    for line in text.split("\r\n") {
        if let Some((k, v)) = line.split_once(':') {
            out.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    out
}

/// Parse `form-data; name="x"; filename="y"` → ("form-data", params)
fn parse_cd(v: &str) -> (String, Vec<(String, String)>) {
    let mut parts = v.split(';');
    let disp = parts.next().unwrap_or("").trim().to_string();
    let mut params = Vec::new();
    for p in parts {
        let p = p.trim();
        if p.is_empty() {
            continue;
        }
        if let Some((k, v)) = p.split_once('=') {
            let k = k.trim().to_ascii_lowercase();
            let v = v.trim();
            let v = if v.starts_with('"') && v.ends_with('"') && v.len() >= 2 {
                // quoted-string with backslash escapes
                let inner = &v[1..v.len() - 1];
                let mut s = String::new();
                let mut esc = false;
                for c in inner.chars() {
                    if esc {
                        s.push(c);
                        esc = false;
                    } else if c == '\\' {
                        esc = true;
                    } else {
                        s.push(c);
                    }
                }
                s
            } else {
                v.to_string()
            };
            params.push((k, v));
        }
    }
    (disp, params)
}
