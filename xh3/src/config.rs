//! Configuration shared by the server and client binaries.

use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    /// UDP listen address, e.g. "0.0.0.0:443".
    pub listen: String,
    /// Shared secret carried in the URL path. Requests not matching it are
    /// served as the embedded static website.
    pub token: String,
    /// VLESS user UUID (accepted clients must present this).
    pub uuid: String,
    /// TLS certificate path. If omitted (and `key` too), a self-signed
    /// certificate for `hostname` is generated and written next to the
    /// config file.
    pub cert: Option<String>,
    pub key: Option<String>,
    /// Host name for the auto-generated self-signed certificate.
    #[serde(default = "default_hostname")]
    pub hostname: String,
}

fn default_hostname() -> String {
    "localhost".into()
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClientConfig {
    /// Proxy server UDP address, e.g. "1.2.3.4:443".
    pub server: String,
    /// TLS SNI / certificate name.
    pub server_name: String,
    pub token: String,
    pub uuid: String,
    /// Skip certificate verification entirely (dev only).
    #[serde(default)]
    pub insecure: bool,
    /// TOFU pin file: stores the pinned SPKI sha256 of the server cert.
    /// On first connect the pin is written; later connects verify it.
    pub pin_file: Option<String>,
    /// SOCKS5 listen address, e.g. "127.0.0.1:1080".
    pub socks_listen: String,
}

impl ServerConfig {
    pub fn uuid_bytes(&self) -> anyhow::Result<[u8; 16]> {
        parse_uuid(&self.uuid)
    }
}

impl ClientConfig {
    pub fn uuid_bytes(&self) -> anyhow::Result<[u8; 16]> {
        parse_uuid(&self.uuid)
    }

    pub fn pin_path(&self) -> Option<PathBuf> {
        self.pin_file.as_ref().map(PathBuf::from)
    }
}

pub fn parse_uuid(s: &str) -> anyhow::Result<[u8; 16]> {
    let u = uuid::Uuid::parse_str(s)?;
    Ok(*u.as_bytes())
}

pub fn load<T: for<'de> Deserialize<'de>>(path: &str) -> anyhow::Result<T> {
    let raw = std::fs::read_to_string(path)?;
    let cfg = toml::from_str(&raw)?;
    Ok(cfg)
}

/// Generate a random path token (16 bytes, base64url without padding).
pub fn random_token() -> String {
    use base64::Engine;
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

pub fn random_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}
