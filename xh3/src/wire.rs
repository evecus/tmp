//! Wire format: VLESS request/response headers + length-prefixed upload frames.
//!
//! Session layout (self-compatible subset of XHTTP semantics):
//!   GET  /<token>/<sid>   -> long-lived download stream (h3 response body)
//!   POST /<token>/<sid>   -> upload stream; body = a sequence of frames
//!   frame := u32be(len) ++ payload
//!   first frame payload := VLESS request header ++ initial payload

use std::io;

use bytes::{BufMut, Bytes, BytesMut};

pub const VLESS_VERSION: u8 = 0;
pub const CMD_TCP: u8 = 0x01;
pub const CMD_UDP: u8 = 0x02;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetAddr {
    V4([u8; 4]),
    V6([u8; 16]),
    Domain(String),
}

impl std::fmt::Display for TargetAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TargetAddr::V4(a) => write!(f, "{}", std::net::Ipv4Addr::from(*a)),
            TargetAddr::V6(a) => write!(f, "{}", std::net::Ipv6Addr::from(*a)),
            TargetAddr::Domain(d) => write!(f, "{d}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct VlessRequest {
    pub uuid: [u8; 16],
    pub command: u8,
    pub target: TargetAddr,
    pub port: u16,
    pub header_len: usize,
}

fn addr_type(addr: &TargetAddr) -> u8 {
    match addr {
        TargetAddr::V4(_) => 0x01,
        TargetAddr::Domain(_) => 0x02,
        TargetAddr::V6(_) => 0x03,
    }
}

pub fn encode_request(req: &VlessRequest) -> Bytes {
    let mut buf = BytesMut::new();
    buf.put_u8(VLESS_VERSION);
    buf.put_slice(&req.uuid);
    buf.put_u8(0); // addons length
    buf.put_u8(req.command);
    buf.put_u16(req.port);
    buf.put_u8(addr_type(&req.target));
    match &req.target {
        TargetAddr::V4(a) => buf.put_slice(a),
        TargetAddr::V6(a) => buf.put_slice(a),
        TargetAddr::Domain(d) => {
            buf.put_u8(d.len() as u8);
            buf.put_slice(d.as_bytes());
        }
    }
    buf.freeze()
}

/// Decode a VLESS request header from the start of `buf`.
/// Returns the parsed request and the number of bytes consumed.
pub fn decode_request(buf: &[u8]) -> io::Result<(VlessRequest, usize)> {
    let err = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_string());
    if buf.len() < 19 {
        return Err(err("vless header truncated"));
    }
    if buf[0] != VLESS_VERSION {
        return Err(err("unsupported vless version"));
    }
    let mut uuid = [0u8; 16];
    uuid.copy_from_slice(&buf[1..17]);
    let addons = buf[17] as usize;
    let mut pos = 18 + addons;
    if buf.len() < pos + 4 {
        return Err(err("vless header truncated (addr)"));
    }
    let command = buf[pos];
    pos += 1;
    let port = u16::from_be_bytes([buf[pos], buf[pos + 1]]);
    pos += 2;
    let target = match buf[pos] {
        0x01 => {
            if buf.len() < pos + 5 {
                return Err(err("vless header truncated (v4)"));
            }
            let mut a = [0u8; 4];
            a.copy_from_slice(&buf[pos + 1..pos + 5]);
            pos += 5;
            TargetAddr::V4(a)
        }
        0x02 => {
            let n = buf.get(pos + 1).copied().ok_or_else(|| err("vless header truncated (domain len)"))? as usize;
            if buf.len() < pos + 2 + n {
                return Err(err("vless header truncated (domain)"));
            }
            let d = String::from_utf8_lossy(&buf[pos + 2..pos + 2 + n]).into_owned();
            pos += 2 + n;
            TargetAddr::Domain(d)
        }
        0x03 => {
            if buf.len() < pos + 17 {
                return Err(err("vless header truncated (v6)"));
            }
            let mut a = [0u8; 16];
            a.copy_from_slice(&buf[pos + 1..pos + 17]);
            pos += 17;
            TargetAddr::V6(a)
        }
        t => return Err(err(&format!("unknown address type {t:#x}"))),
    };
    Ok((
        VlessRequest {
            uuid,
            command,
            target,
            port,
            header_len: pos,
        },
        pos,
    ))
}

/// VLESS response header: version byte + addons-length byte (both zero here).
pub const RESPONSE_HEADER: [u8; 2] = [VLESS_VERSION, 0];

pub fn decode_response_header(buf: &[u8]) -> io::Result<usize> {
    if buf.len() < 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "vless response truncated",
        ));
    }
    if buf[0] != VLESS_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported vless response version",
        ));
    }
    Ok(2 + buf[1] as usize)
}

/// Incremental frame decoder over an ordered byte stream (h3 recv chunks).
#[derive(Default)]
pub struct FrameBuffer {
    buf: BytesMut,
}

impl FrameBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
    }

    /// Try to pop one complete frame. `None` while more data is needed.
    pub fn pop_frame(&mut self, max_frame: usize) -> io::Result<Option<Bytes>> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
        if len > max_frame {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("frame too large: {len}"),
            ));
        }
        if self.buf.len() < 4 + len {
            return Ok(None);
        }
        let _ = self.buf.drain(..4);
        Ok(Some(self.buf.split_to(len).freeze()))
    }
}

pub fn encode_frame(payload: &[u8]) -> Bytes {
    let mut buf = BytesMut::with_capacity(4 + payload.len());
    buf.put_u32(payload.len() as u32);
    buf.put_slice(payload);
    buf.freeze()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrip_domain() {
        let req = VlessRequest {
            uuid: [7u8; 16],
            command: CMD_TCP,
            target: TargetAddr::Domain("example.com".into()),
            port: 443,
            header_len: 0,
        };
        let enc = encode_request(&req);
        let (dec, n) = decode_request(&enc).unwrap();
        assert_eq!(dec.uuid, req.uuid);
        assert_eq!(dec.target, req.target);
        assert_eq!(dec.port, 443);
        assert_eq!(n, enc.len());
    }

    #[test]
    fn request_roundtrip_v4() {
        let req = VlessRequest {
            uuid: [1u8; 16],
            command: CMD_TCP,
            target: TargetAddr::V4([127, 0, 0, 1]),
            port: 8080,
            header_len: 0,
        };
        let (dec, n) = decode_request(&encode_request(&req)).unwrap();
        assert_eq!(dec.target, TargetAddr::V4([127, 0, 0, 1]));
        assert_eq!(dec.port, 8080);
        assert_eq!(n, encode_request(&req).len());
    }

    #[test]
    fn frames_roundtrip() {
        let mut fb = FrameBuffer::new();
        let mut out = Vec::new();
        let payload = encode_frame(b"hello").to_vec();
        fb.push(&payload[..3]); // partial
        assert!(fb.pop_frame(1 << 20).unwrap().is_none());
        fb.push(&payload[3..]);
        fb.push(&encode_frame(b"world"));
        out.push(fb.pop_frame(1 << 20).unwrap().unwrap());
        out.push(fb.pop_frame(1 << 20).unwrap().unwrap());
        assert_eq!(&out[0][..], b"hello");
        assert_eq!(&out[1][..], b"world");
        assert!(fb.pop_frame(1 << 20).unwrap().is_none());
    }
}
