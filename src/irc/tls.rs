//! TLS transport for the live ZNC hop.
//!
//! The client verifies the server against a private CA (`ca.pem`) and never
//! presents a client certificate: the `CN=rakazo-beeper` certificate belongs to
//! ZNC's *upstream* identity to `pi-irc-tls`, not to this local client.
//!
//! Hostname checking is configurable because ZNC's certificate is issued for its
//! IP address (`IP:100.108.9.93`). Chain verification against the configured CA
//! is always performed; skipping the name check never skips the chain.

use super::config::{IrcConfig, expand_path};
use anyhow::{Context, Result, bail, ensure};
use rustls::{
    ClientConfig, ClientConnection, RootCertStore, StreamOwned,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{CryptoProvider, WebPkiSupportedAlgorithms},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use std::{
    io::{self, Read, Write},
    net::{TcpStream, ToSocketAddrs},
    path::Path,
    sync::Arc,
    time::Duration,
};

/// Decode every `CERTIFICATE` block in a PEM bundle. The base64 crate is already
/// a dependency, so no PEM-specific crate is needed.
pub fn pem_certificates(pem: &[u8]) -> Result<Vec<CertificateDer<'static>>> {
    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";
    let text = std::str::from_utf8(pem).context("PEM bundle is not UTF-8")?;
    let mut certificates = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(BEGIN) {
        let body_start = start + BEGIN.len();
        let Some(end) = rest[body_start..].find(END) else {
            bail!("PEM bundle has a BEGIN CERTIFICATE without a matching END");
        };
        let body = &rest[body_start..body_start + end];
        let der = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            body.split_whitespace().collect::<String>(),
        )
        .context("PEM certificate body is not valid base64")?;
        certificates.push(CertificateDer::from(der));
        rest = &rest[body_start + end + END.len()..];
    }
    ensure!(
        !certificates.is_empty(),
        "PEM bundle contains no certificates"
    );
    Ok(certificates)
}

/// Load a private-CA trust store. A missing or unparsable file fails closed.
pub fn root_store(cafile: &Path) -> Result<RootCertStore> {
    let cafile = expand_path(cafile);
    let pem =
        std::fs::read(&cafile).with_context(|| format!("read TLS cafile {}", cafile.display()))?;
    let certificates =
        pem_certificates(&pem).with_context(|| format!("parse TLS cafile {}", cafile.display()))?;
    let mut roots = RootCertStore::empty();
    for certificate in certificates {
        roots
            .add(certificate)
            .with_context(|| format!("add trust anchor from {}", cafile.display()))?;
    }
    Ok(roots)
}

/// A blocking stream that is either plain TCP or TLS over TCP.
pub enum Transport {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ClientConnection, TcpStream>>),
}

impl Read for Transport {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.read(buffer),
            Self::Tls(stream) => stream.read(buffer),
        }
    }
}

impl Write for Transport {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.write(buffer),
            Self::Tls(stream) => stream.write(buffer),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(stream) => stream.flush(),
            Self::Tls(stream) => stream.flush(),
        }
    }
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Chain verification without a name check. `rustls` exposes the pieces only
/// through the webpki crate, hence the direct `rustls-webpki` dependency.
#[derive(Debug)]
struct ChainOnlyVerifier {
    roots: RootCertStore,
    supported: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for ChainOnlyVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let cert = webpki::EndEntityCert::try_from(end_entity).map_err(|_| {
            rustls::Error::InvalidCertificate(rustls::CertificateError::BadEncoding)
        })?;
        cert.verify_for_usage(
            self.supported.all,
            &self.roots.roots,
            intermediates,
            now,
            webpki::KeyUsage::server_auth(),
            None,
            None,
        )
        .map_err(|_| rustls::Error::InvalidCertificate(rustls::CertificateError::BadSignature))?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.supported)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.supported)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.supported.supported_schemes()
    }
}

fn tls_config(config: &IrcConfig) -> Result<ClientConfig> {
    let cafile = config
        .cafile
        .as_ref()
        .context("TLS is enabled but no cafile is configured")?;
    let roots = root_store(cafile)?;
    let provider = provider();
    if config.verify_hostname {
        Ok(ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .context("configure TLS protocol versions")?
            .with_root_certificates(roots)
            .with_no_client_auth())
    } else {
        let verifier = Arc::new(ChainOnlyVerifier {
            roots,
            supported: provider.signature_verification_algorithms,
        });
        Ok(ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .context("configure TLS protocol versions")?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth())
    }
}

/// Connect to the configured IRC server, wrapping the socket in TLS when asked.
pub fn connect(config: &IrcConfig, connect_timeout: Duration) -> Result<Transport> {
    let address = config.address();
    let mut addresses = address
        .to_socket_addrs()
        .with_context(|| format!("resolve IRC address {address}"))?;
    let socket = addresses
        .next()
        .with_context(|| format!("IRC address {address} resolved to nothing"))?;
    let mut stream = TcpStream::connect_timeout(&socket, connect_timeout)
        .with_context(|| format!("connect to IRC server {address}"))?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_millis(200)))?;
    if !config.tls {
        return Ok(Transport::Plain(stream));
    }

    let tls = tls_config(config)?;
    let server_name = ServerName::try_from(config.host.clone())
        .with_context(|| format!("IRC host {:?} is not a valid TLS server name", config.host))?;
    let connection = ClientConnection::new(Arc::new(tls), server_name)
        .context("start TLS handshake with IRC server")?;
    stream.set_read_timeout(Some(connect_timeout))?;
    let mut connection = connection;
    let started = std::time::Instant::now();
    while connection.is_handshaking() {
        match connection.complete_io(&mut stream) {
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) && started.elapsed() < connect_timeout =>
            {
                continue;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("TLS handshake with IRC server {address} failed"));
            }
        }
    }
    stream.set_read_timeout(Some(Duration::from_millis(200)))?;
    Ok(Transport::Tls(Box::new(StreamOwned::new(
        connection, stream,
    ))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const CA_PEM: &str = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n";

    #[test]
    fn pem_parser_reads_every_certificate_block() {
        let pem = format!("junk\n{CA_PEM}between\n{CA_PEM}");
        let certificates = pem_certificates(pem.as_bytes()).unwrap();
        assert_eq!(certificates.len(), 2);
    }

    #[test]
    fn pem_parser_rejects_an_empty_bundle() {
        let error = pem_certificates(b"nothing here").unwrap_err();
        assert!(format!("{error:#}").contains("no certificates"));
    }

    #[test]
    fn missing_cafile_fails_closed() {
        let error = root_store(Path::new("/definitely/not/a/ca.pem")).unwrap_err();
        assert!(format!("{error:#}").contains("read TLS cafile"));
    }

    #[test]
    fn tls_enabled_without_cafile_fails_closed() {
        let config = IrcConfig {
            tls: true,
            ..IrcConfig::default()
        };
        let error = tls_config(&config).unwrap_err();
        assert!(format!("{error:#}").contains("cafile"));
    }

    #[test]
    fn cafile_is_read_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ca.pem");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(CA_PEM.as_bytes()).unwrap();
        let error = root_store(&path).unwrap_err();
        assert!(format!("{error:#}").contains("add trust anchor"));
    }
}
