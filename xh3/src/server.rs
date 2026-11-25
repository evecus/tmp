//! The camouflage proxy server.
//!
//! A single QUIC endpoint (ALPN `h3`). Every h3 request is classified:
//!   - path starts with /<token>/<sid>  -> XHTTP-style proxy session
//!   - anything else                    -> embedded static website
//!
//! Session model:
//!   - one long-lived GET download stream per session;
//!   - one POST upload stream per session whose body carries
//!     length-prefixed frames; the first frame holds the VLESS header;
//!   - the server dials the target and splices TCP <-> h3 streams.

use std::collections::VecDeque;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use bytes::Bytes;
use dashmap::DashMap;
use http::{Request, Response, StatusCode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::lookup_host;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::Mutex;

use crate::config::ServerConfig;
use crate::site;
use crate::wire::{self, FrameBuffer, TargetAddr, VlessRequest, CMD_TCP};

const MAX_FRAME: usize = 1 << 20; // 1 MiB

// ---------------------------------------------------------------- session

enum Down {
    Data(Bytes),
    End,
}

/// Where download-side chunks go while the GET stream is not yet open.
enum DownSide {
    Buffering(VecDeque<Down>),
    Open(tokio::sync::mpsc::UnboundedSender<Down>),
}

struct SessionInner {
    down: Mutex<DownSide>,
    /// Write half of the target connection; set after the first upload
    /// frame (VLESS header) is processed.
    target_tx: Mutex<Option<OwnedWriteHalf>>,
    /// VLESS header parsed from the first upload frame.
    vless: Mutex<Option<VlessRequest>>,
    closed: AtomicBool,
}

impl SessionInner {
    fn new() -> Self {
        Self {
            down: Mutex::new(DownSide::Buffering(VecDeque::new())),
            target_tx: Mutex::new(None),
            vless: Mutex::new(None),
            closed: AtomicBool::new(false),
        }
    }

    /// Route a chunk to the GET handler (open channel) or the buffer.
    async fn push_down(&self, msg: Down) {
        let mut guard = self.down.lock().await;
        match &mut *guard {
            DownSide::Open(tx) => {
                let _ = tx.send(msg);
            }
            DownSide::Buffering(q) => {
                // Cap the buffer so a stalled session cannot grow forever.
                if q.len() > 16_384 {
                    self.closed.store(true, Ordering::SeqCst);
                } else {
                    q.push_back(msg);
                }
            }
        }
    }

    async fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let mut guard = self.down.lock().await;
        match &mut *guard {
            DownSide::Open(tx) => {
                let _ = tx.send(Down::End);
            }
            DownSide::Buffering(q) => {
                q.push_back(Down::End);
            }
        }
    }
}

type Session = Arc<SessionInner>;

// ---------------------------------------------------------------- server

#[derive(Clone)]
pub struct Xh3Server {
    cfg: Arc<ServerConfig>,
    uuid: [u8; 16],
    sessions: Arc<DashMap<String, Session>>,
}

pub async fn run(cfg: ServerConfig) -> Result<()> {
    let uuid = cfg.uuid_bytes()?;
    let cfg = Arc::new(cfg);
    let server = Xh3Server {
        cfg: cfg.clone(),
        uuid,
        sessions: Arc::new(DashMap::new()),
    };

    let tls = crate::tls::server_config(&cfg).await?;
    let mut server_crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls)
        .context("quic server config")?;
    let mut transport = quinn::TransportConfig::default();
    transport.max_concurrent_bidi_streams(512u32.into());
    transport.max_concurrent_uni_streams(16u32.into());
    server_crypto.transport = Arc::new(transport);
    let qcfg = quinn::ServerConfig::with_crypto(Arc::new(server_crypto));

    let endpoint = quinn::Endpoint::server(qcfg, cfg.listen.parse()?)?;
    let local = endpoint.local_addr()?;
    tracing::info!("xh3 server listening on udp://{local} (alpn h3)");

    while let Some(incoming) = endpoint.accept().await {
        let server = server.clone();
        tokio::spawn(async move {
            match incoming.await {
                Ok(conn) => {
                    if let Err(e) = server.serve_connection(conn).await {
                        tracing::debug!("connection ended: {e}");
                    }
                }
                Err(e) => tracing::debug!("handshake failed: {e}"),
            }
        });
    }
    Ok(())
}

impl Xh3Server {
    async fn serve_connection(&self, conn: quinn::Connection) -> Result<()> {
        tracing::debug!("quic connected: {}", conn.remote_address());
        let mut h3 = h3::server::builder()
            .build(h3_quinn::Connection::new(conn))
            .await?;
        loop {
            let resolver = match h3.accept().await? {
                Some(r) => r,
                None => break,
            };
            let (req, stream) = match resolver.resolve_request().await {
                Ok(x) => x,
                Err(e) => {
                    tracing::debug!("request resolve failed: {e}");
                    continue;
                }
            };
            let this = self.clone();
            tokio::spawn(async move {
                if let Err(e) = this.dispatch(req, stream).await {
                    tracing::debug!("request handler: {e}");
                }
            });
        }
        Ok(())
    }

    async fn dispatch(
        &self,
        req: Request<()>,
        stream: h3::server::RequestStream<h3_quinn::BidiStream, Bytes>,
    ) -> Result<()> {
        let path = req.uri().path().to_string();
        let (_, token, sid) = split_path(&path);
        let is_proxy =
            token.as_deref() == Some(self.cfg.token.as_str()) && sid.is_some();

        if !is_proxy {
            // Not a proxy request: serve the real site. Drain any request
            // body first (browsers may POST forms).
            let mut stream = stream;
            while (stream.recv_data().await?).is_some() {}
            let resp = site::respond(&path);
            send_full_response(stream, resp)
                .await
                .map_err(|e| anyhow::anyhow!("site: {e}"))?;
            return Ok(());
        }

        let sid = sid.unwrap().to_string();
        match req.method().as_str() {
            "GET" => self
                .serve_download(sid, stream)
                .await
                .map_err(|e| anyhow::anyhow!("download: {e}")),
            "POST" => self
                .serve_upload(sid, stream)
                .await
                .map_err(|e| anyhow::anyhow!("upload: {e}")),
            _ => {
                let resp = site::respond(&path);
                send_full_response(stream, resp)
                    .await
                    .map_err(|e| anyhow::anyhow!("site: {e}"))?;
                Ok(())
            }
        }
    }

    async fn serve_download(
        &self,
        sid: String,
        mut stream: h3::server::RequestStream<h3_quinn::BidiStream, Bytes>,
    ) -> io::Result<()> {
        let session = self.get_or_create(&sid);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        let resp = Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/octet-stream")
            .header("cache-control", "no-store")
            .body(())
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        stream.send_response(resp).await.map_err(io::Error::other)?;
        stream
            .send_data(Bytes::from_static(&wire::RESPONSE_HEADER))
            .await
            .map_err(io::Error::other)?;

        let mut end = false;
        {
            let mut guard = session.down.lock().await;
            if let DownSide::Buffering(q) = &mut *guard {
                for msg in q.drain(..) {
                    match msg {
                        Down::Data(b) => {
                            stream.send_data(b).await.map_err(io::Error::other)?
                        }
                        Down::End => {
                            end = true;
                            break;
                        }
                    }
                }
            }
            *guard = DownSide::Open(tx);
        }

        if !end {
            while let Some(msg) = rx.recv().await {
                match msg {
                    Down::Data(b) => stream.send_data(b).await.map_err(io::Error::other)?,
                    Down::End => break,
                }
            }
        }
        stream.finish().await.map_err(io::Error::other)?;
        self.sessions.remove(&sid);
        Ok(())
    }

    async fn serve_upload(
        &self,
        sid: String,
        mut stream: h3::server::RequestStream<h3_quinn::BidiStream, Bytes>,
    ) -> io::Result<()> {
        let session = self.get_or_create(&sid);

        // Acknowledge immediately with an empty 200; the body keeps
        // streaming (h3 is full duplex).
        let resp = Response::builder()
            .status(StatusCode::OK)
            .header("cache-control", "no-store")
            .body(())
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        stream.send_response(resp).await.map_err(io::Error::other)?;

        let mut fb = FrameBuffer::new();
        loop {
            match stream.recv_data().await? {
                Some(chunk) => {
                    fb.push(&chunk);
                    while let Some(frame) = fb.pop_frame(MAX_FRAME)? {
                        if self.ingest(&sid, session.clone(), frame).await.is_err() {
                            return Ok(()); // session rejected/closed
                        }
                    }
                    if session.closed.load(Ordering::SeqCst) {
                        return Ok(());
                    }
                }
                None => {
                    // Upload side finished: half-close the target and end
                    // the session.
                    if let Some(t) = session.target_tx.lock().await.take() {
                        let _ = t.shutdown().await;
                    }
                    session.close().await;
                    self.sessions.remove(&sid);
                    return Ok(());
                }
            }
        }
    }

    /// Process one upload frame: first frame = VLESS header (auth + dial),
    /// subsequent frames = raw data toward the target.
    async fn ingest(&self, sid: &str, session: Session, frame: Bytes) -> io::Result<()> {
        if session.vless.lock().await.is_none() {
            let (req, consumed) = wire::decode_request(&frame)?;
            if req.uuid != self.uuid || req.command != CMD_TCP {
                tracing::debug!("session {sid}: rejected (bad uuid/command)");
                session.close().await;
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "auth failed"));
            }
            *session.vless.lock().await = Some(req.clone());
            let rest = frame.slice(consumed..);

            let addr = resolve_target(&req).await?;
            let tcp = TcpStream::connect(addr).await?;
            tcp.set_nodelay(true).ok();
            let (mut rd, wr) = tcp.into_split();
            *session.target_tx.lock().await = Some(wr);

            // Pump target -> download side.
            let pump = session.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 16 * 1024];
                loop {
                    match rd.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => pump.push_down(Down::Data(Bytes::copy_from_slice(&buf[..n]))).await,
                    }
                }
                pump.close().await;
            });

            if !rest.is_empty() {
                let mut t = session.target_tx.lock().await;
                match t.as_mut() {
                    Some(w) => w.write_all(&rest).await?,
                    None => {
                        return Err(io::Error::new(io::ErrorKind::NotConnected, "target gone"));
                    }
                }
            }
            return Ok(());
        }

        // Subsequent frames: raw data.
        let mut guard = session.target_tx.lock().await;
        match guard.as_mut() {
            Some(t) => t.write_all(&frame).await,
            None => Err(io::Error::new(io::ErrorKind::NotConnected, "target not ready")),
        }
    }

    fn get_or_create(&self, sid: &str) -> Session {
        self.sessions
            .entry(sid.to_string())
            .or_insert_with(|| Arc::new(SessionInner::new()))
            .clone()
    }
}

fn split_path(path: &str) -> (bool, Option<&str>, Option<&str>) {
    let mut parts = path.trim_start_matches('/').splitn(3, '/');
    let token = parts.next().filter(|s| !s.is_empty());
    let sid = parts.next().filter(|s| !s.is_empty());
    let extra_ok = matches!(parts.next(), None | Some(""));
    (extra_ok, token, sid)
}

async fn resolve_target(req: &VlessRequest) -> io::Result<std::net::SocketAddr> {
    let port = req.port;
    match &req.target {
        TargetAddr::V4(a) => Ok((*a, port).into()),
        TargetAddr::V6(a) => Ok((*a, port).into()),
        TargetAddr::Domain(d) => {
            let host = format!("{d}:{port}");
            let mut addrs = lookup_host(host)
                .await
                .map_err(|e| io::Error::new(io::ErrorKind::AddrNotAvailable, e))?;
            addrs.next().ok_or_else(|| {
                io::Error::new(io::ErrorKind::AddrNotAvailable, "dns: no address")
            })
        }
    }
}

async fn send_full_response(
    mut stream: h3::server::RequestStream<h3_quinn::BidiStream, Bytes>,
    resp: Response<Vec<u8>>,
) -> io::Result<()> {
    let (parts, body) = resp.into_parts();
    let resp = Response::from_parts(parts, ());
    stream.send_response(resp).await.map_err(io::Error::other)?;
    stream
        .send_data(Bytes::from(body))
        .await
        .map_err(io::Error::other)?;
    stream.finish().await.map_err(io::Error::other)
}
