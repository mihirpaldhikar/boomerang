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
use crate::cors::Cors;
use crate::router::Router;
use crate::server::request_info::RequestInfo;
use crate::server::{ActiveSet, HttpServer, HttpServerError, OnRequestComplete};
use crate::tls::{AcmeChallengeStore, TlsConfig};
use socket2::Socket;
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;

pub struct HttpServerBuilder {
    port: u16,
    cors: Option<Cors>,
    tls_config: Option<TlsConfig>,
    request_timeout: std::time::Duration,
    keep_alive_timeout: std::time::Duration,
    max_body_size: usize,
    on_request_complete: Option<OnRequestComplete>,
    restart_stability_window: std::time::Duration,
    restart_pre_hook_timeout: std::time::Duration,
    acme_webroot: Option<String>,
    acme_http_port: Option<u16>,
}

impl HttpServerBuilder {
    pub fn new(port: u16) -> Self {
        Self {
            port,
            cors: None,
            tls_config: None,
            request_timeout: std::time::Duration::from_secs(60),
            keep_alive_timeout: std::time::Duration::from_secs(75),
            max_body_size: 10 * 1024 * 1024,
            on_request_complete: None,
            restart_stability_window: std::time::Duration::from_secs(5),
            restart_pre_hook_timeout: std::time::Duration::from_secs(30),
            acme_webroot: None,
            acme_http_port: None,
        }
    }

    pub fn cors(mut self, cors: Cors) -> Self {
        self.cors = Some(cors);
        self
    }

    pub fn tls(mut self, tls_config: TlsConfig) -> Self {
        self.tls_config = Some(tls_config);
        self
    }

    pub fn acme_webroot<S: Into<String>>(mut self, dir: S) -> Self {
        self.acme_webroot = Some(dir.into());
        self
    }

    pub fn acme_http_port(mut self, port: u16) -> Self {
        self.acme_http_port = Some(port);
        self
    }

    pub fn request_timeout(mut self, duration: std::time::Duration) -> Self {
        self.request_timeout = duration;
        self
    }

    pub fn keep_alive_timeout(mut self, duration: std::time::Duration) -> Self {
        self.keep_alive_timeout = duration;
        self
    }

    pub fn max_body_size(mut self, bytes: usize) -> Self {
        self.max_body_size = bytes;
        self
    }

    pub fn on_request_complete<F, Fut>(mut self, f: F) -> Self
    where
        F: Fn(RequestInfo) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.on_request_complete = Some(Arc::new(move |meta| Box::pin(f(meta))));
        self
    }

    pub fn restart_stability_window(mut self, duration: std::time::Duration) -> Self {
        self.restart_stability_window = duration;
        self
    }

    pub fn restart_pre_hook_timeout(mut self, duration: std::time::Duration) -> Self {
        self.restart_pre_hook_timeout = duration;
        self
    }

    pub async fn build(self) -> Result<HttpServer, HttpServerError> {
        #[cfg(target_os = "linux")]
        prewarm_linux_fd_table();

        let addr = SocketAddr::from(([0, 0, 0, 0], self.port));
        let domain = if addr.is_ipv6() {
            socket2::Domain::IPV6
        } else {
            socket2::Domain::IPV4
        };

        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);

        #[cfg(target_os = "linux")]
        let listener_count = cores;
        #[cfg(not(target_os = "linux"))]
        let listener_count = 1;

        let mut listeners = Vec::with_capacity(listener_count);

        for _ in 0..listener_count {
            let socket = Socket::new(domain, socket2::Type::STREAM, Some(socket2::Protocol::TCP))
                .map_err(|_| HttpServerError::AddrInUse(self.port))?;

            #[cfg(not(windows))]
            socket.set_reuse_address(true).unwrap_or(());
            #[cfg(target_os = "linux")]
            socket.set_reuse_port(true).unwrap_or(());

            socket
                .bind(&addr.into())
                .map_err(|_| HttpServerError::AddrInUse(self.port))?;
            socket
                .listen(4096)
                .map_err(|_| HttpServerError::AddrInUse(self.port))?;
            socket.set_nonblocking(true).unwrap_or(());
            socket.set_tcp_nodelay(true).unwrap_or(());
            socket
                .set_tcp_keepalive(
                    &socket2::TcpKeepalive::new()
                        .with_time(std::time::Duration::from_secs(60))
                        .with_interval(std::time::Duration::from_secs(10)),
                )
                .unwrap_or(());

            #[cfg(target_os = "linux")]
            unsafe {
                use std::os::unix::io::AsRawFd;
                let defer_secs: libc::c_int = 1;
                libc::setsockopt(
                    socket.as_raw_fd(),
                    libc::IPPROTO_TCP,
                    libc::TCP_DEFER_ACCEPT,
                    &defer_secs as *const libc::c_int as *const libc::c_void,
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                );
            }

            let std_listener: TcpListener = socket.into();
            listeners.push(std_listener);
        }

        let keep_alive_header: Arc<[u8]> = Arc::from(
            format!(
                "connection: keep-alive\r\nkeep-alive: timeout={}\r\n",
                self.keep_alive_timeout.as_secs()
            )
            .into_bytes(),
        );

        let (tls, acme_challenges, tls_background) = match self.tls_config {
            Some(tls_config) => {
                let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
                let runtime = tls_config.build()?;
                (
                    Some(runtime.acceptor),
                    runtime.http_challenges,
                    runtime.background,
                )
            }
            None => (None, Arc::new(AcmeChallengeStore::new()), None),
        };

        let acme_http_listener = match self.acme_http_port {
            Some(port) => {
                let http_addr = SocketAddr::from(([0, 0, 0, 0], port));
                let sock = Socket::new(
                    socket2::Domain::IPV4,
                    socket2::Type::STREAM,
                    Some(socket2::Protocol::TCP),
                )
                .map_err(|_| HttpServerError::AddrInUse(port))?;
                #[cfg(not(windows))]
                sock.set_reuse_address(true).unwrap_or(());
                #[cfg(target_os = "linux")]
                sock.set_reuse_port(true).unwrap_or(());
                sock.bind(&http_addr.into())
                    .map_err(|_| HttpServerError::AddrInUse(port))?;
                sock.listen(1024)
                    .map_err(|_| HttpServerError::AddrInUse(port))?;
                sock.set_nonblocking(true).unwrap_or(());
                Some(sock.into())
            }
            None => None,
        };

        Ok(HttpServer {
            router: Router::build(),
            listeners,
            worker_count: cores,
            active_requests: ActiveSet::new_with_telemetry(self.on_request_complete.is_some()),
            keep_alive_header,
            on_request_complete: self.on_request_complete,
            cors: self.cors,
            tls,
            request_timeout: self.request_timeout,
            keep_alive_timeout: self.keep_alive_timeout,
            max_body_size: self.max_body_size,
            restart_stability_window: self.restart_stability_window,
            restart_pre_hook_timeout: self.restart_pre_hook_timeout,
            acme_http_listener,
            tls_background,
            acme_challenges,
            acme_webroot: self.acme_webroot,
        })
    }
}
