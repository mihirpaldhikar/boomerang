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

use crate::protocol::{JsonValue, write_json_str};
use crate::tls::{DynamicCertResolver, build_certified_key, load_from_files};
use aws_lc_rs::digest::{SHA256, digest};
use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::sign::CertifiedKey;
use rustls::{ClientConfig, RootCertStore};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

const RENEW_BEFORE_SECS: i64 = 30 * 86_400; // Renew 30 days before expiry
const RENEW_CHECK_INTERVAL_SECS: u64 = 12 * 3600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcmeChallengeMode {
    /// Port 80 (or HTTP listener) `/.well-known/acme-challenge/<token>`
    Http01,
    /// Port 443 `acme-tls/1` ALPN (no port 80 needed)
    TlsAlpn01,
    /// Prefer TLS-ALPN-01 when offered by CA, fall back to HTTP-01
    Auto,
}

#[derive(Clone)]
pub(crate) struct AcmeConfig {
    pub(crate) directory_url: String,
    pub(crate) domains: Vec<String>,
    pub(crate) contact_email: Option<String>,
    pub(crate) cache_dir: String,
    pub(crate) challenge: AcmeChallengeMode,
    pub(crate) ca_cert_pem: Option<Vec<u8>>,
}

#[derive(Debug, Default)]
pub(crate) struct AcmeChallengeStore {
    http_tokens: RwLock<HashMap<String, String>>,
}

impl AcmeChallengeStore {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn insert_http01(&self, token: String, key_auth: String) {
        self.http_tokens.write().unwrap().insert(token, key_auth);
    }

    pub(crate) fn remove_http01(&self, token: &str) {
        self.http_tokens.write().unwrap().remove(token);
    }

    pub(crate) fn get_http01(&self, token: &str) -> Option<String> {
        self.http_tokens.read().unwrap().get(token).cloned()
    }
}

struct AcmeAccountKey {
    pair: EcdsaKeyPair,
    jwk_json: String,
    thumbprint_b64: String,
}

impl AcmeAccountKey {
    fn load_or_create(path: &Path) -> Result<Self, String> {
        if path.exists() {
            let der = std::fs::read(path)
                .map_err(|e| format!("failed to read account key {}: {e}", path.display()))?;
            return Self::from_pkcs8(&der);
        }

        let rng = SystemRandom::new();
        let doc = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
            .map_err(|_| "failed to generate P-256 account key".to_string())?;
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(path, doc.as_ref())
            .map_err(|e| format!("failed to persist account key: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }

        Self::from_pkcs8(doc.as_ref())
    }

    fn from_pkcs8(pkcs8_der: &[u8]) -> Result<Self, String> {
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8_der)
            .map_err(|_| "invalid PKCS#8 P-256 account key".to_string())?;

        let pub_bytes = pair.public_key().as_ref();
        if pub_bytes.len() != 65 || pub_bytes[0] != 0x04 {
            return Err("unexpected P-256 public key format".into());
        }
        let x = URL_SAFE_NO_PAD.encode(&pub_bytes[1..33]);
        let y = URL_SAFE_NO_PAD.encode(&pub_bytes[33..65]);

        let jwk_json = format!(r#"{{"crv":"P-256","kty":"EC","x":"{x}","y":"{y}"}}"#);
        let hash = digest(&SHA256, jwk_json.as_bytes());
        let thumbprint_b64 = URL_SAFE_NO_PAD.encode(hash.as_ref());

        Ok(Self {
            pair,
            jwk_json,
            thumbprint_b64,
        })
    }

    fn key_authorization(&self, token: &str) -> String {
        format!("{token}.{}", self.thumbprint_b64)
    }

    fn sign_jws(&self, url: &str, nonce: &str, kid: Option<&str>, payload_json: &str) -> String {
        let protected = match kid {
            Some(k) => {
                let mut s = String::with_capacity(64 + k.len() + nonce.len() + url.len());
                s.push_str(r#"{"alg":"ES256","kid":""#);
                s.push_str(k);
                s.push_str(r#"","nonce":""#);
                s.push_str(nonce);
                s.push_str(r#"","url":""#);
                s.push_str(url);
                s.push_str(r#""}"#);
                s
            }
            None => format!(
                r#"{{"alg":"ES256","jwk":{},"nonce":"{nonce}","url":"{url}"}}"#,
                self.jwk_json
            ),
        };

        let protected_b64 = URL_SAFE_NO_PAD.encode(protected.as_bytes());
        let payload_b64 = if payload_json.is_empty() {
            String::new()
        } else {
            URL_SAFE_NO_PAD.encode(payload_json.as_bytes())
        };

        let signing_input = format!("{protected_b64}.{payload_b64}");
        let rng = SystemRandom::new();
        let sig = self.pair.sign(&rng, signing_input.as_bytes()).unwrap();
        let sig_b64 = URL_SAFE_NO_PAD.encode(sig.as_ref());

        format!(
            r#"{{"protected":"{protected_b64}","payload":"{payload_b64}","signature":"{sig_b64}"}}"#
        )
    }
}

struct AcmeHttpResponse {
    status: u16,
    replay_nonce: Option<String>,
    location: Option<String>,
    body: Vec<u8>,
}

struct AcmeHttpClient {
    connector: TlsConnector,
}

impl AcmeHttpClient {
    fn new(ca_cert_pem: Option<&[u8]>) -> Result<Self, String> {
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

        if let Some(pem) = ca_cert_pem {
            let mut reader = std::io::BufReader::new(pem);
            for cert in rustls_pemfile::certs(&mut reader) {
                let cert = cert.map_err(|e| format!("invalid custom ACME CA PEM: {e}"))?;
                roots
                    .add(cert)
                    .map_err(|e| format!("failed to add custom ACME CA: {e}"))?;
            }
        }

        let mut cfg = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        cfg.alpn_protocols = vec![b"http/1.1".to_vec()];

        Ok(Self {
            connector: TlsConnector::from(Arc::new(cfg)),
        })
    }

    async fn request(
        &self,
        method: &str,
        url: &str,
        body: Option<&[u8]>,
        content_type: Option<&str>,
    ) -> Result<AcmeHttpResponse, String> {
        let (host, port, path) = parse_https_url(url)?;

        let tcp = TcpStream::connect((host, port))
            .await
            .map_err(|e| format!("ACME connect to {host}:{port} failed: {e}"))?;
        let server_name = ServerName::try_from(host.to_owned())
            .map_err(|_| format!("invalid ACME host '{host}'"))?;
        let mut tls = self
            .connector
            .connect(server_name, tcp)
            .await
            .map_err(|e| format!("ACME TLS handshake to {host} failed: {e}"))?;

        let mut req = String::with_capacity(256);
        req.push_str(method);
        req.push(' ');
        req.push_str(path);
        req.push_str(" HTTP/1.1\r\nHost: ");
        req.push_str(host);
        if port != 443 {
            use std::fmt::Write;
            let _ = write!(req, ":{port}");
        }
        req.push_str("\r\nUser-Agent: boomerang-acme/1.0\r\nAccept: application/json, application/pem-certificate-chain\r\nConnection: close\r\n");
        if let Some(ct) = content_type {
            req.push_str("Content-Type: ");
            req.push_str(ct);
            req.push_str("\r\n");
        }
        if let Some(b) = body {
            use std::fmt::Write;
            let _ = write!(req, "Content-Length: {}\r\n", b.len());
        }
        req.push_str("\r\n");

        tls.write_all(req.as_bytes())
            .await
            .map_err(|e| format!("ACME request write failed: {e}"))?;
        if let Some(b) = body {
            tls.write_all(b)
                .await
                .map_err(|e| format!("ACME body write failed: {e}"))?;
        }
        tls.flush()
            .await
            .map_err(|e| format!("ACME flush failed: {e}"))?;

        let mut raw = Vec::with_capacity(4096);
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            tls.read_to_end(&mut raw),
        )
        .await
        .map_err(|_| "ACME HTTP read timed out".to_string())?
        .map_err(|e| format!("ACME HTTP read failed: {e}"))?;

        parse_http_response(&raw)
    }
}

fn parse_https_url(url: &str) -> Result<(&str, u16, &str), String> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| format!("ACME URL must use https:// (got {url})"))?;
    let (authority, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };
    if let Some((host, port_str)) = authority.rsplit_once(':') {
        if !host.contains(']') || authority.ends_with(']') {
            let port = port_str
                .parse::<u16>()
                .map_err(|_| format!("invalid port in URL: {url}"))?;
            return Ok((host.trim_matches(|c| c == '[' || c == ']'), port, path));
        }
    }
    Ok((authority.trim_matches(|c| c == '[' || c == ']'), 443, path))
}

fn parse_http_response(raw: &[u8]) -> Result<AcmeHttpResponse, String> {
    let hdr_end = memchr::memmem::find(raw, b"\r\n\r\n")
        .ok_or_else(|| "ACME response missing header terminator".to_string())?;
    let hdr_str = std::str::from_utf8(&raw[..hdr_end])
        .map_err(|_| "ACME response headers not UTF-8".to_string())?;
    let mut lines = hdr_str.split("\r\n");
    let status_line = lines.next().ok_or("missing status line")?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| format!("invalid ACME status line: {status_line}"))?;

    let mut replay_nonce = None;
    let mut location = None;
    let mut is_chunked = false;
    let mut content_length: Option<usize> = None;

    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim();
            let val = v.trim();
            if key.eq_ignore_ascii_case("replay-nonce") {
                replay_nonce = Some(val.to_owned());
            } else if key.eq_ignore_ascii_case("location") {
                location = Some(val.to_owned());
            } else if key.eq_ignore_ascii_case("transfer-encoding")
                && val.to_ascii_lowercase().contains("chunked")
            {
                is_chunked = true;
            } else if key.eq_ignore_ascii_case("content-length") {
                content_length = val.parse::<usize>().ok();
            }
        }
    }

    let raw_body = &raw[hdr_end + 4..];
    let body = if is_chunked {
        decode_chunked(raw_body)?
    } else if let Some(cl) = content_length {
        raw_body[..cl.min(raw_body.len())].to_vec()
    } else {
        raw_body.to_vec()
    };

    Ok(AcmeHttpResponse {
        status,
        replay_nonce,
        location,
        body,
    })
}

fn decode_chunked(mut data: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let pos = memchr::memmem::find(data, b"\r\n")
            .ok_or_else(|| "malformed chunked ACME response".to_string())?;
        let size_line = std::str::from_utf8(&data[..pos]).map_err(|_| "invalid chunk size")?;
        let hex = size_line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(hex, 16).map_err(|_| "invalid hex chunk size")?;
        data = &data[pos + 2..];
        if size == 0 {
            break;
        }
        if data.len() < size + 2 {
            return Err("truncated chunked ACME response".into());
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size + 2..];
    }
    Ok(out)
}

fn read_der_len(buf: &[u8], pos: &mut usize) -> Option<usize> {
    let b = *buf.get(*pos)?;
    *pos += 1;
    if b < 0x80 {
        return Some(b as usize);
    }
    let count = (b & 0x7F) as usize;
    if count == 0 || count > 4 || *pos + count > buf.len() {
        return None;
    }
    let mut len = 0usize;
    for &byte in &buf[*pos..*pos + count] {
        len = (len << 8) | (byte as usize);
    }
    *pos += count;
    Some(len)
}

fn skip_tlv(buf: &[u8], pos: &mut usize) -> Option<()> {
    let _tag = *buf.get(*pos)?;
    *pos += 1;
    let len = read_der_len(buf, pos)?;
    if *pos + len > buf.len() {
        return None;
    }
    *pos += len;
    Some(())
}

fn cert_not_after_unix(cert_der: &[u8]) -> Option<i64> {
    let mut pos = 0usize;
    if *cert_der.get(pos)? != 0x30 {
        return None;
    }
    pos += 1;
    let _cert_len = read_der_len(cert_der, &mut pos)?;

    // tbsCertificate SEQUENCE
    if *cert_der.get(pos)? != 0x30 {
        return None;
    }
    pos += 1;
    let _tbs_len = read_der_len(cert_der, &mut pos)?;

    // optional [0] EXPLICIT version
    if *cert_der.get(pos)? == 0xA0 {
        skip_tlv(cert_der, &mut pos)?;
    }
    skip_tlv(cert_der, &mut pos)?; // serialNumber
    skip_tlv(cert_der, &mut pos)?; // signature
    skip_tlv(cert_der, &mut pos)?; // issuer

    // validity SEQUENCE
    if *cert_der.get(pos)? != 0x30 {
        return None;
    }
    pos += 1;
    let _val_len = read_der_len(cert_der, &mut pos)?;

    skip_tlv(cert_der, &mut pos)?; // notBefore

    // notAfter: UTCTime (0x17) or GeneralizedTime (0x18)
    let tag = *cert_der.get(pos)?;
    pos += 1;
    let len = read_der_len(cert_der, &mut pos)?;
    let s = std::str::from_utf8(cert_der.get(pos..pos + len)?).ok()?;

    let (year, rest) = match tag {
        0x17 if s.len() >= 13 => {
            let yy: i64 = s[0..2].parse().ok()?;
            let y = if yy >= 50 { 1900 + yy } else { 2000 + yy };
            (y, &s[2..])
        }
        0x18 if s.len() >= 15 => (s[0..4].parse().ok()?, &s[4..]),
        _ => return None,
    };

    let month: i64 = rest[0..2].parse().ok()?;
    let day: i64 = rest[2..4].parse().ok()?;
    let hour: i64 = rest[4..6].parse().ok()?;
    let min: i64 = rest[6..8].parse().ok()?;
    let sec: i64 = rest[8..10].parse().ok()?;

    Some(unix_secs_from_utc(year, month, day, hour, min, sec))
}

fn unix_secs_from_utc(year: i64, month: i64, day: i64, hour: i64, min: i64, sec: i64) -> i64 {
    let y = year - if month <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    days * 86_400 + hour * 3600 + min * 60 + sec
}

fn cert_needs_renewal(cert_der: &[u8]) -> bool {
    let Some(not_after) = cert_not_after_unix(cert_der) else {
        return true;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    not_after - now <= RENEW_BEFORE_SECS
}

pub(crate) fn try_load_cached_cert(cache_dir: &str) -> Option<Arc<CertifiedKey>> {
    let cert_path = format!("{cache_dir}/fullchain.pem");
    let key_path = format!("{cache_dir}/privkey.pem");
    let (certs, key) = load_from_files(&cert_path, &key_path).ok()?;
    let ck = build_certified_key(certs, key).ok()?;
    Some(Arc::new(ck))
}

struct AcmeClient {
    http: AcmeHttpClient,
    account_key: AcmeAccountKey,
    kid: Option<String>,
    nonce: Option<String>,
    new_nonce_url: String,
    new_account_url: String,
    new_order_url: String,
}

impl AcmeClient {
    async fn init(config: &AcmeConfig) -> Result<Self, String> {
        let http = AcmeHttpClient::new(config.ca_cert_pem.as_deref())?;
        let acct_path = Path::new(&config.cache_dir).join("account.key.der");
        let account_key = AcmeAccountKey::load_or_create(&acct_path)?;

        let dir_resp = http
            .request("GET", &config.directory_url, None, None)
            .await?;
        if dir_resp.status != 200 {
            return Err(format!("ACME directory HTTP {}", dir_resp.status));
        }
        let dir = JsonValue::parse(&dir_resp.body)
            .map_err(|e| format!("invalid ACME directory JSON: {e:?}"))?;

        let new_nonce_url = dir["newNonce"]
            .as_str()
            .ok_or("missing newNonce in directory")?
            .to_owned();
        let new_account_url = dir["newAccount"]
            .as_str()
            .ok_or("missing newAccount in directory")?
            .to_owned();
        let new_order_url = dir["newOrder"]
            .as_str()
            .ok_or("missing newOrder in directory")?
            .to_owned();

        let mut client = Self {
            http,
            account_key,
            kid: None,
            nonce: dir_resp.replay_nonce,
            new_nonce_url,
            new_account_url,
            new_order_url,
        };

        client
            .register_account(config.contact_email.as_deref())
            .await?;
        Ok(client)
    }

    async fn ensure_nonce(&mut self) -> Result<String, String> {
        if let Some(n) = self.nonce.take() {
            return Ok(n);
        }
        let resp = self
            .http
            .request("HEAD", &self.new_nonce_url, None, None)
            .await?;
        resp.replay_nonce
            .ok_or_else(|| "ACME newNonce response missing Replay-Nonce header".to_string())
    }

    async fn post_signed(
        &mut self,
        url: &str,
        payload_json: &str,
        use_kid: bool,
    ) -> Result<AcmeHttpResponse, String> {
        for attempt in 0..2 {
            let nonce = self.ensure_nonce().await?;
            let kid = if use_kid { self.kid.as_deref() } else { None };
            let jws = self.account_key.sign_jws(url, &nonce, kid, payload_json);
            let resp = self
                .http
                .request(
                    "POST",
                    url,
                    Some(jws.as_bytes()),
                    Some("application/jose+json"),
                )
                .await?;
            if let Some(next_nonce) = resp.replay_nonce.clone() {
                self.nonce = Some(next_nonce);
            }
            if resp.status == 400 && attempt == 0 {
                if let Ok(err_json) = JsonValue::parse(&resp.body) {
                    if err_json["type"]
                        .as_str()
                        .is_some_and(|t| t.ends_with(":badNonce"))
                    {
                        continue;
                    }
                }
            }
            return Ok(resp);
        }
        unreachable!()
    }

    async fn register_account(&mut self, email: Option<&str>) -> Result<(), String> {
        let mut payload = String::from(r#"{"termsOfServiceAgreed":true"#);
        if let Some(addr) = email {
            let mut mailto = Vec::with_capacity(addr.len() + 16);
            mailto.extend_from_slice(br#","contact":["mailto:"#);
            let mut quoted = Vec::new();
            write_json_str(addr, &mut quoted);
            let inner = std::str::from_utf8(&quoted[1..quoted.len() - 1]).unwrap_or(addr);
            mailto.extend_from_slice(inner.as_bytes());
            mailto.extend_from_slice(br#""]"#);
            payload.push_str(std::str::from_utf8(&mailto).unwrap());
        }
        payload.push('}');

        let url = self.new_account_url.clone();
        let resp = self.post_signed(&url, &payload, false).await?;
        if resp.status != 200 && resp.status != 201 {
            return Err(format!(
                "ACME newAccount failed ({}): {}",
                resp.status,
                String::from_utf8_lossy(&resp.body)
            ));
        }
        self.kid = resp.location;
        if self.kid.is_none() {
            return Err("ACME newAccount missing Location (kid) header".into());
        }
        Ok(())
    }

    async fn issue_cert(
        &mut self,
        config: &AcmeConfig,
        resolver: &DynamicCertResolver,
        http_store: &AcmeChallengeStore,
    ) -> Result<Arc<CertifiedKey>, String> {
        let mut ids_buf = Vec::from(&b"{\"identifiers\":["[..]);
        for (idx, d) in config.domains.iter().enumerate() {
            if idx > 0 {
                ids_buf.push(b',');
            }
            ids_buf.extend_from_slice(br#"{"type":"dns","value":"#);
            write_json_str(d, &mut ids_buf);
            ids_buf.push(b'}');
        }
        ids_buf.extend_from_slice(b"]}");
        let order_payload = String::from_utf8(ids_buf).unwrap();

        let new_order_url = self.new_order_url.clone();
        let order_resp = self
            .post_signed(&new_order_url, &order_payload, true)
            .await?;
        if order_resp.status != 201 {
            return Err(format!(
                "ACME newOrder failed ({}): {}",
                order_resp.status,
                String::from_utf8_lossy(&order_resp.body)
            ));
        }
        let order_url = order_resp
            .location
            .ok_or("ACME newOrder missing Location")?;
        let order_json =
            JsonValue::parse(&order_resp.body).map_err(|_| "invalid newOrder JSON".to_string())?;

        let finalize_url = order_json["finalize"]
            .as_str()
            .ok_or("missing finalize URL")?
            .to_owned();

        let authzs = order_json["authorizations"]
            .as_array()
            .ok_or("missing authorizations array")?;

        let mut active_http_tokens: Vec<String> = Vec::new();
        let mut active_alpn_domains: Vec<String> = Vec::new();

        for authz_val in authzs {
            let authz_url = authz_val.as_str().ok_or("invalid authz URL")?;
            let authz_resp = self.post_signed(authz_url, "", true).await?;
            let authz_json =
                JsonValue::parse(&authz_resp.body).map_err(|_| "invalid authz JSON".to_string())?;

            if authz_json["status"].as_str() == Some("valid") {
                continue;
            }

            let domain = authz_json["identifier"]["value"]
                .as_str()
                .ok_or("authz missing identifier.value")?
                .to_owned();

            let challenges = authz_json["challenges"]
                .as_array()
                .ok_or("authz missing challenges")?;

            let chosen = pick_challenge(challenges, config.challenge)
                .ok_or_else(|| format!("no matching ACME challenge offered for {domain}"))?;
            let ch_type = chosen["type"].as_str().unwrap_or("");
            let ch_url = chosen["url"]
                .as_str()
                .ok_or("challenge missing url")?
                .to_owned();
            let token = chosen["token"].as_str().ok_or("challenge missing token")?;
            let key_auth = self.account_key.key_authorization(token);

            if ch_type == "tls-alpn-01" {
                let auth_digest = digest(&SHA256, key_auth.as_bytes());
                let alpn_ck = build_tls_alpn_cert(&domain, auth_digest.as_ref())?;
                resolver.set_alpn_challenge(domain.clone(), alpn_ck);
                active_alpn_domains.push(domain.clone());
            } else {
                http_store.insert_http01(token.to_owned(), key_auth);
                active_http_tokens.push(token.to_owned());
            }

            let trigger_resp = self.post_signed(&ch_url, "{}", true).await?;
            if trigger_resp.status >= 400 {
                cleanup_challenges(
                    resolver,
                    http_store,
                    &active_http_tokens,
                    &active_alpn_domains,
                );
                return Err(format!(
                    "ACME challenge trigger failed ({}): {}",
                    trigger_resp.status,
                    String::from_utf8_lossy(&trigger_resp.body)
                ));
            }

            let mut valid = false;
            for _ in 0..30 {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                let poll = self.post_signed(authz_url, "", true).await?;
                let pj = JsonValue::parse(&poll.body)
                    .map_err(|_| "invalid authz poll JSON".to_string())?;
                match pj["status"].as_str() {
                    Some("valid") => {
                        valid = true;
                        break;
                    }
                    Some("invalid") => {
                        cleanup_challenges(
                            resolver,
                            http_store,
                            &active_http_tokens,
                            &active_alpn_domains,
                        );
                        return Err(format!(
                            "ACME authorization invalid for {domain}: {}",
                            String::from_utf8_lossy(&poll.body)
                        ));
                    }
                    _ => {}
                }
            }

            if !valid {
                cleanup_challenges(
                    resolver,
                    http_store,
                    &active_http_tokens,
                    &active_alpn_domains,
                );
                return Err(format!("ACME authorization timed out for {domain}"));
            }
        }

        cleanup_challenges(
            resolver,
            http_store,
            &active_http_tokens,
            &active_alpn_domains,
        );

        let leaf_key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
            .map_err(|e| format!("failed to generate leaf key: {e}"))?;
        let mut csr_params = rcgen::CertificateParams::new(config.domains.clone())
            .map_err(|e| format!("failed to build CSR params: {e}"))?;
        let mut dn = rcgen::DistinguishedName::new();
        dn.push(rcgen::DnType::CommonName, config.domains[0].clone());
        csr_params.distinguished_name = dn;
        let csr = csr_params
            .serialize_request(&leaf_key)
            .map_err(|e| format!("CSR serialization failed: {e}"))?;

        let csr_b64 = URL_SAFE_NO_PAD.encode(csr.der());
        let finalize_payload = format!(r#"{{"csr":"{csr_b64}"}}"#);
        let fin_resp = self
            .post_signed(&finalize_url, &finalize_payload, true)
            .await?;
        if fin_resp.status >= 400 {
            return Err(format!(
                "ACME finalize failed ({}): {}",
                fin_resp.status,
                String::from_utf8_lossy(&fin_resp.body)
            ));
        }

        let mut cert_url: Option<String> = JsonValue::parse(&fin_resp.body)
            .ok()
            .and_then(|j| j["certificate"].as_str().map(str::to_owned));

        if cert_url.is_none() {
            for _ in 0..30 {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                let poll = self.post_signed(&order_url, "", true).await?;
                let pj = JsonValue::parse(&poll.body)
                    .map_err(|_| "invalid order poll JSON".to_string())?;
                if let Some(cu) = pj["certificate"].as_str() {
                    cert_url = Some(cu.to_owned());
                    break;
                }
                if pj["status"].as_str() == Some("invalid") {
                    return Err(format!(
                        "ACME order became invalid: {}",
                        String::from_utf8_lossy(&poll.body)
                    ));
                }
            }
        }

        let cert_url = cert_url.ok_or("ACME order timed out waiting for certificate")?;
        let cert_resp = self.post_signed(&cert_url, "", true).await?;
        if cert_resp.status != 200 {
            return Err(format!("ACME cert download HTTP {}", cert_resp.status));
        }

        let _ = std::fs::create_dir_all(&config.cache_dir);
        let fullchain_path = format!("{}/fullchain.pem", config.cache_dir);
        let privkey_path = format!("{}/privkey.pem", config.cache_dir);
        let leaf_pem = leaf_key.serialize_pem();
        std::fs::write(&fullchain_path, &cert_resp.body)
            .map_err(|e| format!("failed to write {fullchain_path}: {e}"))?;
        std::fs::write(&privkey_path, leaf_pem.as_bytes())
            .map_err(|e| format!("failed to write {privkey_path}: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&privkey_path, std::fs::Permissions::from_mode(0o600));
        }

        let mut reader = std::io::BufReader::new(cert_resp.body.as_slice());
        let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut reader)
            .collect::<Result<_, _>>()
            .map_err(|e| format!("failed to parse issued PEM chain: {e}"))?;
        let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der()));
        let ck = build_certified_key(certs, key_der)
            .map_err(|e| format!("failed to build CertifiedKey: {e}"))?;

        Ok(Arc::new(ck))
    }
}

fn pick_challenge<'a>(
    challenges: &'a [JsonValue],
    mode: AcmeChallengeMode,
) -> Option<&'a JsonValue> {
    let find_type = |t: &str| challenges.iter().find(|c| c["type"].as_str() == Some(t));
    match mode {
        AcmeChallengeMode::TlsAlpn01 => find_type("tls-alpn-01"),
        AcmeChallengeMode::Http01 => find_type("http-01"),
        AcmeChallengeMode::Auto => find_type("tls-alpn-01").or_else(|| find_type("http-01")),
    }
}

fn cleanup_challenges(
    resolver: &DynamicCertResolver,
    http_store: &AcmeChallengeStore,
    tokens: &[String],
    domains: &[String],
) {
    for t in tokens {
        http_store.remove_http01(t);
    }
    for d in domains {
        resolver.clear_alpn_challenge(d);
    }
}

fn build_tls_alpn_cert(domain: &str, sha_digest: &[u8]) -> Result<Arc<CertifiedKey>, String> {
    let mut params = rcgen::CertificateParams::new(vec![domain.to_owned()])
        .map_err(|e| format!("TLS-ALPN params error: {e}"))?;
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    params.custom_extensions = vec![rcgen::CustomExtension::new_acme_identifier(sha_digest)];

    let key_pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
        .map_err(|e| format!("TLS-ALPN keygen error: {e}"))?;
    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| format!("TLS-ALPN self-sign error: {e}"))?;

    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));
    let ck = build_certified_key(vec![cert.der().clone()], key_der)
        .map_err(|e| format!("TLS-ALPN CertifiedKey error: {e}"))?;
    Ok(Arc::new(ck))
}

pub(crate) async fn run_acme_loop(
    config: AcmeConfig,
    resolver: Arc<DynamicCertResolver>,
    http_store: Arc<AcmeChallengeStore>,
) {
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    loop {
        let cert_path = format!("{}/fullchain.pem", config.cache_dir);
        let key_path = format!("{}/privkey.pem", config.cache_dir);

        let needs_issue = match load_from_files(&cert_path, &key_path) {
            Ok((certs, _)) => certs
                .first()
                .map(|leaf| cert_needs_renewal(leaf.as_ref()))
                .unwrap_or(true),
            Err(_) => true,
        };

        if needs_issue {
            match AcmeClient::init(&config).await {
                Ok(mut client) => match client.issue_cert(&config, &resolver, &http_store).await {
                    Ok(ck) => {
                        resolver.update(ck);
                        println!("ACME certificate issued/renewed for {:?}.", config.domains);
                    }
                    Err(e) => {
                        println!("ACME issuance error: {e}. Retrying in 60s...");
                        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                        continue;
                    }
                },
                Err(e) => {
                    println!("ACME client init error: {e}. Retrying in 60s...");
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    continue;
                }
            }
        }

        tokio::time::sleep(std::time::Duration::from_secs(RENEW_CHECK_INTERVAL_SECS)).await;
    }
}
