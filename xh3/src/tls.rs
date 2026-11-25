//! TLS setup: server cert loading / self-signed generation, client config
//! with TOFU SPKI pinning or explicit `insecure` mode.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ClientConfig, ServerConfig};

use crate::config::{ClientConfig, ServerConfig};

/// Build the server TLS config; generates a self-signed cert when none is
/// configured (written next to the config file is the caller's concern —
/// here we just generate in memory unless paths are given).
pub async fn server_config(cfg: &ServerConfig) -> Result<ServerConfig> {
    let (certs, key) = match (&cfg.cert, &cfg.key) {
        (Some(c), Some(k)) => load_cert_key(c, k)?,
        _ => self_signed(&cfg.hostname)?,
    };
    let mut tls = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("server tls: cert/key mismatch")?;
    tls.alpn_protocols = vec![b"h3".to_vec()];
    Ok(tls)
}

fn load_cert_key(cert_path: &str, key_path: &str) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let certs = rustls_pemfile::certs(&mut std::io::BufReader::new(std::fs::File::open(cert_path)?))
        .collect::<std::io::Result<Vec<_>>>()?;
    let key = rustls_pemfile::private_key(&mut std::io::BufReader::new(std::fs::File::open(key_path)?))?
        .context("no private key found in key file")?;
    Ok((certs, key))
}

fn self_signed(hostname: &str) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let cert = rcgen::generate_simple_self_signed(vec![hostname.to_string()])?;
    let cert_der = CertificateDer::from(cert.cert);
    let key_der = PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
    Ok((vec![cert_der], PrivateKeyDer::Pkcs8(key_der)))
}

// ------------------------------------------------------------------ client

/// Persisted pin: base64(SHA-256(SPKI)).
fn load_pin(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

#[derive(Debug)]
pub struct PinVerifier {
    /// Some(pin) -> verify; None -> accept and persist (TOFU).
    expected: Option<String>,
    pin_path: Option<std::path::PathBuf>,
    insecure: bool,
}

impl PinVerifier {
    fn spki_b64(end_entity: &CertificateDer<'_>) -> String {
        use base64::Engine;
        use sha2::{Digest, Sha256};
        // Parse the SPKI out of the DER certificate.
        let spki = extract_spki(end_entity.as_ref()).unwrap_or_else(|| end_entity.as_ref().to_vec());
        let digest = Sha256::digest(&spki);
        base64::engine::general_purpose::STANDARD.encode(digest)
    }

    fn persist(&self, end_entity: &CertificateDer<'_>) {
        if let Some(path) = &self.pin_path {
            if self.expected.is_none() {
                let pin = Self::spki_b64(end_entity);
                let _ = std::fs::write(path, pin);
            }
        }
    }
}

impl rustls::client::danger::ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if self.insecure {
            return Ok(rustls::client::danger::ServerCertVerified::assertion());
        }
        let pin = Self::spki_b64(end_entity);
        match &self.expected {
            Some(expected) if expected == &pin => {
                Ok(rustls::client::danger::ServerCertVerified::assertion())
            }
            Some(_) => Err(rustls::Error::General(
                "server certificate pin mismatch".into(),
            )),
            None => {
                // First contact: trust on first use.
                self.persist(end_entity);
                Ok(rustls::client::danger::ServerCertVerified::assertion())
            }
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct<'_>,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct<'_>,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Minimal DER walk: Certificate ::= SEQUENCE { tbsCertificate, ... }
/// tbsCertificate ::= SEQUENCE { ..., subjectPublicKeyInfo, ... }
/// We just grab the first SPKI inside the TBSCertificate.
fn extract_spki(der: &[u8]) -> Option<Vec<u8>> {
    // certificate SEQUENCE
    let (cert, _) = asn1_seq(der)?;
    // tbsCertificate
    let (tbs, _) = asn1_seq(cert)?;
    // Skip: version [0] EXPLICIT (optional), serialNumber, signature,
    // issuer, validity, subject. Then comes subjectPublicKeyInfo.
    let mut p = tbs;
    if p.first() == Some(&0xa0) {
        let (_, rest) = asn1_elem(p)?;
        p = rest;
    }
    for _ in 0..5 {
        let (_, rest) = asn1_elem(p)?;
        p = rest;
    }
    let (spki, _) = asn1_elem(p)?;
    Some(spki.to_vec())
}

/// Given a buffer starting with a TLV element, return (contents, rest).
fn asn1_elem(buf: &[u8]) -> Option<(&[u8], &[u8])> {
    if buf.len() < 2 {
        return None;
    }
    let tag = buf[0];
    let _ = tag;
    let (len, hdr) = asn1_len(&buf[1..])?;
    let end = hdr + len;
    if buf.len() < 1 + end {
        return None;
    }
    Some((&buf[1 + hdr..1 + hdr + len], &buf[1 + end..]))
}

fn asn1_seq(buf: &[u8]) -> Option<(&[u8], &[u8])> {
    if buf.first() != Some(&0x30) {
        return None;
    }
    asn1_elem(buf)
}

fn asn1_len(buf: &[u8]) -> Option<(usize, usize)> {
    match buf.first()? {
        n if *n & 0x80 == 0 => Some((*n as usize, 1)),
        n if *n == 0x81 => Some((buf.get(1).copied()? as usize, 2)),
        n if *n == 0x82 => {
            let l = u16::from_be_bytes([buf.get(1).copied()?, buf.get(2).copied()?]) as usize;
            Some((l, 3))
        }
        _ => None,
    }
}

/// Build the client TLS config according to the client settings:
/// - `insecure`: accept anything (dev only);
/// - pin file present: verify against pinned SPKI;
/// - otherwise: TOFU (accept and persist the first certificate).
pub fn client_config(cfg: &ClientConfig) -> Result<ClientConfig> {
    let pin = cfg.pin_path().and_then(|p| load_pin(&p));
    let verifier = PinVerifier {
        expected: pin,
        pin_path: cfg.pin_path(),
        insecure: cfg.insecure,
    };
    let builder = ClientConfig::builder_with_provider(
        rustls::crypto::ring::default_provider().into(),
    )
    .with_safe_default_protocol_versions()
    .context("tls protocol versions")?
    .with_custom_certificate_verifier(Arc::new(verifier))
    .with_no_client_auth();
    let mut tls = builder;
    tls.alpn_protocols = vec![b"h3".to_vec()];
    Ok(tls)
}
