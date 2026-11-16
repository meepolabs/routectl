//! A TLS front for a plain-http mock server.
//!
//! Some egress decisions hold only for an `https` base URL on a first-party
//! host with no port. A test reaches them by keeping that base URL, resolving
//! the host to [`TlsFront::addr`] and trusting [`TlsFront::ca_pem`]: the
//! front terminates TLS with a leaf for the host and relays the bytes to the
//! mock, which records and answers as usual.

use std::net::SocketAddr;
use std::sync::Arc;

use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;

/// A TLS listener serving `host` that relays every connection to a backend.
/// The listener stops when the front is dropped.
pub struct TlsFront {
    /// Where the front listens; resolve the host to this address.
    pub addr: SocketAddr,
    /// PEM of the root that signed the front's leaf.
    pub ca_pem: Vec<u8>,
    task: JoinHandle<()>,
}

impl TlsFront {
    /// Start a front for `host` relaying to `backend`.
    ///
    /// # Panics
    ///
    /// When the certificates cannot be minted or the listener cannot bind.
    pub async fn start(host: &str, backend: SocketAddr) -> Self {
        let (acceptor, ca_pem) = acceptor_for(host);
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the TLS front");
        let addr = listener.local_addr().expect("TLS front address");
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(relay(acceptor.clone(), stream, backend));
            }
        });
        Self { addr, ca_pem, task }
    }
}

impl Drop for TlsFront {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn relay(acceptor: TlsAcceptor, stream: TcpStream, backend: SocketAddr) {
    let Ok(mut tls) = acceptor.accept(stream).await else {
        return;
    };
    let Ok(mut upstream) = TcpStream::connect(backend).await else {
        return;
    };
    let _ = tokio::io::copy_bidirectional(&mut tls, &mut upstream).await;
}

fn acceptor_for(host: &str) -> (TlsAcceptor, Vec<u8>) {
    let ca_key = KeyPair::generate().expect("CA key");
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca_cert = ca_params.self_signed(&ca_key).expect("CA certificate");

    let leaf_key = KeyPair::generate().expect("leaf key");
    let leaf_params = CertificateParams::new(vec![host.to_string()]).expect("leaf params");
    let issuer = Issuer::from_params(&ca_params, &ca_key);
    let leaf_cert = leaf_params
        .signed_by(&leaf_key, &issuer)
        .expect("leaf certificate");

    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("TLS protocol versions")
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(leaf_cert.der().to_vec())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der())),
        )
        .expect("TLS server config");
    (
        TlsAcceptor::from(Arc::new(config)),
        ca_cert.pem().into_bytes(),
    )
}
