//! TLS 配置:自签证书(rcgen)+ 客户端证书固定(cert pinning)。
//! 安全模型:接收端生成自签证书并打印 BLAKE3 指纹;发送端用 `--fingerprint` 固定。
//! `--insecure` 仅用于测试/调试,跳过校验。

use crate::error::FtsError;
use anyhow::Result;
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use std::sync::Arc;

#[derive(Clone, Debug)]
pub enum PinMode {
    /// 跳过校验(仅测试用)
    Insecure,
    /// 固定对端证书的 BLAKE3 指纹
    Fingerprint([u8; 32]),
}

pub struct ServerCert {
    pub cert: Vec<CertificateDer<'static>>,
    pub key: PrivatePkcs8KeyDer<'static>,
    /// 证书 DER 的 BLAKE3 hex(发送端 --fingerprint 用)
    pub fingerprint: String,
}

pub fn generate_self_signed() -> Result<ServerCert> {
    let ck = rcgen::generate_simple_self_signed(vec!["fts.local".into()])?;
    let cert = CertificateDer::from(ck.cert);
    let fingerprint = blake3::hash(cert.as_ref()).to_hex().to_string();
    let key = PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der());
    Ok(ServerCert { cert: vec![cert], key, fingerprint })
}

/// 从缓存目录加载证书(首次生成并落盘)。
/// 好处:指纹跨运行稳定,发送端可长期固定同一指纹(daemon 模式必需)。
pub fn load_or_generate_cert(dir: &std::path::Path) -> Result<ServerCert> {
    let cert_path = dir.join("fts-cert.der");
    let key_path = dir.join("fts-key.der");
    if let (Ok(cert_der), Ok(key_der)) = (std::fs::read(&cert_path), std::fs::read(&key_path)) {
        let fingerprint = blake3::hash(&cert_der).to_hex().to_string();
        return Ok(ServerCert {
            cert: vec![CertificateDer::from(cert_der)],
            key: PrivatePkcs8KeyDer::from(key_der),
            fingerprint,
        });
    }
    let sc = generate_self_signed()?;
    std::fs::create_dir_all(dir)?;
    std::fs::write(&cert_path, sc.cert[0].as_ref())?;
    std::fs::write(&key_path, sc.key.secret_pkcs8_der())?;
    Ok(sc)
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

pub fn server_config(cert: &ServerCert) -> Result<quinn::ServerConfig> {
    let mut crypto = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(
            cert.cert.clone(),
            rustls::pki_types::PrivateKeyDer::Pkcs8(cert.key.clone_key()),
        )?;
    crypto.alpn_protocols = vec![crate::protocol::ALPN.to_vec()];
    let qc = QuicServerConfig::try_from(crypto)?;
    Ok(quinn::ServerConfig::with_crypto(Arc::new(qc)))
}

pub fn client_config(mode: PinMode) -> Result<quinn::ClientConfig> {
    let mut crypto = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(ClientVerifier { mode }))
        .with_no_client_auth();
    crypto.alpn_protocols = vec![crate::protocol::ALPN.to_vec()];
    let qc = QuicClientConfig::try_from(crypto)?;
    Ok(quinn::ClientConfig::new(Arc::new(qc)))
}

#[derive(Debug)]
struct ClientVerifier {
    mode: PinMode,
}

impl rustls::client::danger::ServerCertVerifier for ClientVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        match &self.mode {
            PinMode::Insecure => Ok(rustls::client::danger::ServerCertVerified::assertion()),
            PinMode::Fingerprint(fp) => {
                if blake3::hash(end_entity.as_ref()).as_bytes() == fp {
                    Ok(rustls::client::danger::ServerCertVerified::assertion())
                } else {
                    Err(rustls::Error::General(
                        "cert fingerprint mismatch".into(),
                    ))
                }
            }
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// 把连接类错误分类成 FtsError(退出码契约)
pub fn map_conn_err(e: quinn::ConnectionError) -> FtsError {
    let s = e.to_string();
    if s.contains("fingerprint") {
        FtsError::Auth("对端证书指纹不匹配(是否用对了 --fingerprint?)".into())
    } else {
        FtsError::Connect(s)
    }
}

/// 解析 64 位 hex 指纹
pub fn decode_fingerprint(hex: &str) -> Option<[u8; 32]> {
    let bytes: Vec<u8> = (0..hex.len() / 2)
        .filter_map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok())
        .collect();
    bytes.try_into().ok()
}
