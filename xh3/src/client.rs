//! The proxy client: SOCKS5 inbound + XHTTP-style h3 sessions outbound.
//!
//! Session layout (mirrors the server):
//!   GET  /<token>/<sid>  -> download stream (VLESS response header first)
//!   POST /<token>/<sid>  -> upload stream of length-prefixed frames; the
//!                           first frame carries the VLESS request header.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use bytes::{Buf, Bytes, BytesMut};
use http::{Method, Request, StatusCode};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex;

use crate::config::ClientConfig;
use crate::wire::{
    self, TargetAddr, VlessRequest, CMD_TCP,
};

const MAX_FRAME: usize = 1 << 20;

// ------------------------------------------------------------ h3 transport

/// A lazily-established, shared QUIC+h3 connection to the server.
pub struct Transport {
    cfg: Arc<ClientConfig>,
    inner: Mutex<Option<Shared>>,
}

struct Shared {
    #[allow(dead_code)]
    conn: h3::client::Connection<h3_quinn::Connection, Bytes>,
    send: h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>,
}

impl Transport {
    pub fn new(cfg: Arc<ClientConfig>) -> Self {
        Self {
            cfg,
            inner: Mutex::new(None),
        }
    }

    async fn shared(&self) -> Result<Shared> {
        let mut guard = self.inner.lock().await;
        if let Some(s) = &*guard {
            return Ok(Shared {
                conn: clone_h3_conn(&s.conn),
                send: s.send.clone(),
            });
        }
        let shared = self.connect().await?;
        *guard = Some(Shared {
            conn: clone_h3_conn(&shared.conn),
            send: shared.send.clone(),
        });
        Ok(shared)
    }

    async fn connect(&self) -> Result<Shared> {
        let cfg = &self.cfg;
        let tls = crate::tls::client_config(cfg)?;
        let mut quinn_crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls)
            .context("quic client config")?;
        let mut transport = quinn::TransportConfig::default();
        transport.keep_alive_interval(Some(std::time::Duration::from_secs(15)));
        transport.max_idle_timeout(Some(std::time::Duration::from_secs(60).try_into()?));
        quinn_crypto.transport = Arc::new(transport);
        let mut endpoint =
            quinn::Endpoint::client("0.0.0.0:0".parse().context("bind client endpoint")?)?;
        endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(quinn_crypto)));

        let addr: SocketAddr = cfg.server.parse()?;
        let conn = endpoint
            .connect(addr, &cfg.server_name)
            .context("connect")?
            .await
            .context("quic handshake")?;
        tracing::info!("quic connected to {} ({})", addr, cfg.server_name);

        let (h3conn, send) = h3::client::builder()
            .build(h3_quinn::Connection::new(conn))
            .await
            .context("h3 handshake")?;

        // Keep polling the h3 connection driver until it closes.
        let mut driver = clone_h3_conn(&h3conn);
        tokio::spawn(async move {
            let _ = driver.wait_idle().await;
        });

        Ok(Shared { conn: h3conn, send })
    }

    /// Open one proxy session to `target`. Returns the download receiver
    /// and an upload writer.
    pub async fn session(&self, target: TargetAddr, port: u16) -> Result<ProxySession> {
        let shared = self.shared().await?;
        let sid = uuid::Uuid::new_v4().simple().to_string();
        let path = format!("/{}/{}/", self.cfg.token, sid);
        let uri = format!("https://{}{}", self.cfg.server_name, path);

        // Download: GET (long-lived response body).
        let get_req = Request::builder()
            .method(Method::GET)
            .uri(uri.clone())
            .header("user-agent", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36")
            .header("accept", "*/*")
            .body(())?;
        let mut get_stream = shared.send.send_request(get_req).await?;

        // Upload: POST (frames; first frame = VLESS header).
        let vless = VlessRequest {
            uuid: self.cfg.uuid_bytes()?,
            command: CMD_TCP,
            target: target,
            port,
            header_len: 0,
        };
        let first = wire::encode_frame(&wire::encode_request(&vless));
        let mut post_stream = shared.send.send_request(
            Request::builder()
                .method(Method::POST)
                .uri(uri)
                .header("content-type", "application/octet-stream")
                .header("user-agent", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36")
                .body(())?,
        ).await?;
        post_stream.send_data(first).await?;

        // Await the download response and strip the VLESS response header.
        let resp = get_stream.recv_response().await?;
        if resp.status() != StatusCode::OK {
            return Err(anyhow::anyhow!("download stream rejected: {}", resp.status()));
        }
        let mut header_state = 0u8; // 0 = need header, 1 = ok, 2 = done
        let mut header_rest = BytesMut::new();

        let (down_tx, down_rx) = tokio::sync::mpsc::channel::<io::Result<Bytes>>(32);
        let mut pump = get_stream;
        tokio::spawn(async move {
            loop {
                match pump.recv_data().await {
                    Ok(Some(mut chunk)) => {
                        if header_state == 2 {
                            let _ = down_tx.send(Ok(chunk.copy_to_bytes(chunk.remaining()))).await;
                            continue;
                        }
                        header_rest.extend_from_slice(&chunk.copy_to_bytes(chunk.remaining()));
                        if header_state == 0 {
                            match wire::decode_response_header(&header_rest) {
                                Ok(consumed) => {
                                    let _ = header_rest.split_to(consumed);
                                    header_state = if header_rest.is_empty() { 2 } else { 1 };
                                    if header_state == 1 {
                                        let b = header_rest.split();
                                        let _ = down_tx.send(Ok(b.freeze())).await;
                                        header_state = 2;
                                    }
                                }
                                Err(e) => {
                                    let _ = down_tx.send(Err(e)).await;
                                    return;
                                }
                            }
                        } else {
                            let b = header_rest.split();
                            let _ = down_tx.send(Ok(b.freeze())).await;
                        }
                    }
                    Ok(None) => {
                        if header_state == 0 {
                            let _ = down_tx
                                .send(Err(io::Error::new(
                                    io::ErrorKind::UnexpectedEof,
                                    "download closed before vless header",
                                )))
                                .await;
                        }
                        let _ = down_tx.send(Err(io::Error::new(io::ErrorKind::UnexpectedEof, "eof"))).await;
                        return;
                    }
                    Err(e) => {
                        let _ = down_tx.send(Err(io::Error::other(e.to_string()))).await;
                        return;
                    }
                }
            }
        });

        Ok(ProxySession {
            sid,
            up: Mutex::new(Some(post_stream)),
            down: down_rx,
        })
    }
}

fn clone_h3_conn(
    conn: &h3::client::Connection<h3_quinn::Connection, Bytes>,
) -> h3::client::Connection<h3_quinn::Connection, Bytes> {
    // h3 client Connection is not Clone; the driver task needs its own poll
    // handle. In h3 0.0.8 the Connection itself drives the protocol when its
    // methods are awaited; `wait_idle` must be polled by exactly one task.
    // We avoid cloning entirely by polling in the task that owns it (below).
    unreachable replaced_placeholder(conn)
}

fn unreachable replaced_placeholder(_conn: &h3::client::Connection<h3_quinn::Connection, Bytes>) -> h3::client::Connection<h3_quinn::Connection, Bytes> {
    panic!("placeholder")
}

// ------------------------------------------------------------ session api

/// One proxied connection: SOCKS5 <-> (frames over h3) <-> server.
pub struct ProxySession {
    #[allow(dead_code)]
    sid: String,
    up: Mutex<Option<h3::client::RequestStream<h3_quinn::SendStream, Bytes>>>,
    down: tokio::sync::mpsc::Receiver<io::Result<Bytes>>,
}

impl ProxySession {
    /// Write one chunk of data toward the target (framed automatically).
    pub async fn send(&self, data: &[u8]) -> io::Result<()> {
        let mut guard = self.up.lock().await;
        match guard.as_mut() {
            Some(stream) => stream
                .send_data(wire::encode_frame(data))
                .await
                .map_err(io::Error::other),
            None => Err(io::Error::new(io::ErrorKind::BrokenPipe, "session closed")),
        }
    }

    pub async fn recv(&mut self) -> Option<io::Result<Bytes>> {
        self.down.recv().await
    }

    /// Graceful end of session: FIN the upload stream.
    pub async fn finish(&self) {
        let mut guard = self.up.lock().await;
        if let Some(mut stream) = guard.take() {
            let _ = stream.finish().await;
        }
    }
}

// ------------------------------------------------------------------ socks5

pub async fn run_socks(cfg: Arc<ClientConfig>, transport: Arc<Transport>) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(&cfg.socks_listen).await?;
    tracing::info!("socks5 listening on {}", cfg.socks_listen);
    loop {
        let (stream, peer) = listener.accept().await?;
        let transport = transport.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_socks(stream, transport).await {
                tracing::debug!("socks {peer}: {e}");
            }
        });
    }
}

async fn handle_socks(mut stream: tokio::net::TcpStream, transport: Arc<Transport>) -> io::Result<()> {
    // Greeting: ver, methods.
    let mut hdr = [0u8; 2];
    stream.read_exact(&mut hdr).await?;
    if hdr[0] != 5 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not socks5"));
    }
    let n = hdr[1] as usize;
    let mut methods = vec![0u8; n];
    stream.read_exact(&mut methods).await?;
    // Accept "no auth".
    if !methods.contains(&0x00) {
        stream.write_all(&[5, 0xff]).await?;
        return Err(io::Error::new(io::ErrorKind::InvalidData, "no acceptable auth"));
    }
    stream.write_all(&[5, 0x00]).await?;

    // Request: ver cmd rsv atyp addr port
    let mut req = [0u8; 4];
    stream.read_exact(&mut req).await?;
    if req[1] != 0x01 {
        // Only CONNECT in v1.
        stream.write_all(&[5, 0x07, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
        return Err(io::Error::new(io::ErrorKind::InvalidData, "only CONNECT supported"));
    }
    let target = match req[3] {
        0x01 => {
            let mut a = [0u8; 4];
            stream.read_exact(&mut a).await?;
            TargetAddr::V4(a)
        }
        0x03 => {
            let mut l = [0u8; 1];
            stream.read_exact(&mut l).await?;
            let mut d = vec![0u8; l[0] as usize];
            stream.read_exact(&mut d).await?;
            TargetAddr::Domain(String::from_utf8_lossy(&d).into_owned())
        }
        0x04 => {
            let mut a = [0u8; 16];
            stream.read_exact(&mut a).await?;
            TargetAddr::V6(a)
        }
        t => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown atyp {t}"),
            ))
        }
    };
    let mut pb = [0u8; 2];
    stream.read_exact(&mut pb).await?;
    let port = u16::from_be_bytes(pb);

    // Open the proxy session; success is signalled by the server's VLESS
    // response header arriving on the download stream.
    let mut session = transport
        .session(target, port)
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::ConnectionAborted, e.to_string()))?;
    // Confirm the target dialed: wait for the first downstream bytes.
    let first = session
        .recv()
        .await
        .ok_or_else(|| io::Error::new(io::ErrorKind::ConnectionAborted, "no downstream"))??;
    if !first.is_empty() {
        session
            .send(&first)
            .await
            .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e.to_string()))?;
    }

    // SOCKS success reply.
    stream.write_all(&[5, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0]).await?;

    // Splice both directions until EOF.
    let (mut rd, mut wr) = stream.split();
    let up = async {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            match rd.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if session.send(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = &mut wr;
        session.finish().await;
    };
    let down = async {
        while let Some(item) = session.recv().await {
            match item {
                Ok(b) => {
                    if wr.write_all(&b).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    };
    tokio::join!(up, down);
    Ok(())
}

// Small helper so `AsyncRead`/`AsyncWrite` imports are actually used.
#[allow(unused)]
fn _traits_used<T: AsyncRead + AsyncWrite>(t: &T) {}

// Silence unused-error conversion helper introduced during prototyping.
#[allow(unused)]
fn _unused_b64(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data)
}
