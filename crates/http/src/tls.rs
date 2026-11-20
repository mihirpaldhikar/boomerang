/*
 * Copyright (c) Mihir Paldhikar
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the “Software”), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED “AS IS”, WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 */

use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert, ServerSessionMemoryCache};
use rustls::sign::CertifiedKey;
use rustls_pemfile::{certs, private_key};
use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::{Arc, RwLock};
use tokio_rustls::TlsAcceptor;

mod acme;

pub use acme::AcmeChallengeMode;
pub(crate) use acme::{AcmeChallengeStore, AcmeConfig, run_acme_loop};

#[derive(thiserror::Error, Debug)]
pub enum TlsConfigError {
    #[error("failed to read certificate file: {0}")]
    CertRead(std::io::Error),
    #[error("failed to read private key file: {0}")]
    KeyRead(std::io::Error),
    #[error("no private key found in key file")]
    NoPrivateKey,
    #[error("TLS config error: {0}")]
    Rustls(#[from] rustls::Error),
    #[error("ACME config error: {0}")]
    Acme(String),
}

#[derive(Debug)]
pub(crate) struct DynamicCertResolver {
    active: RwLock<Option<Arc<CertifiedKey>>>,
    // Ephemeral certs keyed by SNI domain for RFC 8737 TLS-ALPN-01 (`acme-tls/1`)
    alpn_challenges: RwLock<HashMap<String, Arc<CertifiedKey>>>,
}

impl DynamicCertResolver {
    pub(crate) fn new(initial: Option<Arc<CertifiedKey>>) -> Self {
        Self {
            active: RwLock::new(initial),
            alpn_challenges: RwLock::new(HashMap::new()),
        }
    }

    pub(crate) fn update(&self, key: Arc<CertifiedKey>) {
        *self.active.write().unwrap() = Some(key);
    }

    pub(crate) fn set_alpn_challenge(&self, domain: String, key: Arc<CertifiedKey>) {
        self.alpn_challenges
            .write()
            .unwrap()
            .insert(domain.to_ascii_lowercase(), key);
    }

    pub(crate) fn clear_alpn_challenge(&self, domain: &str) {
        self.alpn_challenges
            .write()
            .unwrap()
            .remove(&domain.to_ascii_lowercase());
    }
}

impl ResolvesServerCert for DynamicCertResolver {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        if let Some(mut alpn) = client_hello.alpn() {
            if alpn.any(|proto| proto == b"acme-tls/1") {
                let sni = client_hello.server_name()?.to_ascii_lowercase();
                return self.alpn_challenges.read().unwrap().get(&sni).cloned();
            }
        }
        self.active.read().unwrap().clone()
    }
}

pub(crate) enum TlsBackgroundTask {
    AutoReload {
        cert_path: String,
        key_path: String,
        interval: std::time::Duration,
        resolver: Arc<DynamicCertResolver>,
    },
    Acme {
        config: AcmeConfig,
        resolver: Arc<DynamicCertResolver>,
        http_challenges: Arc<AcmeChallengeStore>,
    },
}

pub(crate) struct TlsRuntime {
    pub(crate) acceptor: TlsAcceptor,
    pub(crate) http_challenges: Arc<AcmeChallengeStore>,
    pub(crate) background: Option<TlsBackgroundTask>,
}

pub enum TlsConfig {
    FromFiles {
        cert_path: String,
        key_path: String,
    },
    FromFilesAutoReload {
        cert_path: String,
        key_path: String,
        reload_interval: std::time::Duration,
    },
    Acme {
        directory_url: String,
        domains: Vec<String>,
        contact_email: Option<String>,
        cache_dir: String,
        challenge: AcmeChallengeMode,
        ca_cert_pem: Option<Vec<u8>>,
    },
}

impl TlsConfig {
    pub(crate) fn build(self) -> Result<TlsRuntime, TlsConfigError> {
        let http_challenges = Arc::new(AcmeChallengeStore::new());

        let (resolver, enable_acme_alpn, background) = match self {
            TlsConfig::FromFiles {
                cert_path,
                key_path,
            } => {
                let (certs, key) = load_from_files(&cert_path, &key_path)?;
                let ck = Arc::new(build_certified_key(certs, key)?);
                (Arc::new(DynamicCertResolver::new(Some(ck))), false, None)
            }
            TlsConfig::FromFilesAutoReload {
                cert_path,
                key_path,
                reload_interval,
            } => {
                let (certs, key) = load_from_files(&cert_path, &key_path)?;
                let ck = Arc::new(build_certified_key(certs, key)?);
                let resolver = Arc::new(DynamicCertResolver::new(Some(ck)));
                let bg = TlsBackgroundTask::AutoReload {
                    cert_path,
                    key_path,
                    interval: reload_interval,
                    resolver: resolver.clone(),
                };
                (resolver, false, Some(bg))
            }
            TlsConfig::Acme {
                directory_url,
                domains,
                contact_email,
                cache_dir,
                challenge,
                ca_cert_pem,
            } => {
                if domains.is_empty() {
                    return Err(TlsConfigError::Acme(
                        "at least one domain is required for ACME".into(),
                    ));
                }
                let enable_alpn = matches!(
                    challenge,
                    AcmeChallengeMode::TlsAlpn01 | AcmeChallengeMode::Auto
                );

                let acme_cfg = AcmeConfig {
                    directory_url,
                    domains,
                    contact_email,
                    cache_dir: cache_dir.clone(),
                    challenge,
                    ca_cert_pem,
                };

                let initial = acme::try_load_cached_cert(&cache_dir);
                let resolver = Arc::new(DynamicCertResolver::new(initial));
                let bg = TlsBackgroundTask::Acme {
                    config: acme_cfg,
                    resolver: resolver.clone(),
                    http_challenges: http_challenges.clone(),
                };
                (resolver, enable_alpn, Some(bg))
            }
        };

        let mut config = ServerConfig::builder()
            .with_no_client_auth()
            .with_cert_resolver(resolver);

        config.session_storage = ServerSessionMemoryCache::new(10_000);
        config.alpn_protocols = if enable_acme_alpn {
            vec![b"http/1.1".to_vec(), b"acme-tls/1".to_vec()]
        } else {
            vec![b"http/1.1".to_vec()]
        };

        Ok(TlsRuntime {
            acceptor: TlsAcceptor::from(Arc::new(config)),
            http_challenges,
            background,
        })
    }
}

pub(crate) fn build_certified_key(
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<CertifiedKey, TlsConfigError> {
    let signer = rustls::crypto::aws_lc_rs::default_provider()
        .key_provider
        .load_private_key(key)?;
    let ck = CertifiedKey::new(certs, signer);
    ck.keys_match()?;
    Ok(ck)
}

pub(crate) fn load_from_files(
    cert_path: &str,
    key_path: &str,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), TlsConfigError> {
    let cert_file = File::open(Path::new(cert_path)).map_err(TlsConfigError::CertRead)?;
    let key_file = File::open(Path::new(key_path)).map_err(TlsConfigError::KeyRead)?;

    let certs: Vec<CertificateDer<'static>> = certs(&mut BufReader::new(cert_file))
        .collect::<Result<_, _>>()
        .map_err(TlsConfigError::CertRead)?;

    if certs.is_empty() {
        return Err(TlsConfigError::CertRead(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "no certificates found in cert file",
        )));
    }

    let key: PrivateKeyDer<'static> = private_key(&mut BufReader::new(key_file))
        .map_err(TlsConfigError::KeyRead)?
        .ok_or(TlsConfigError::NoPrivateKey)?;

    Ok((certs, key))
}
