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
mod active_set;
mod builder;
mod call;
mod connection;
mod per_core;
mod request_info;
mod transport;

use crate::cors::Cors;
use crate::protocol::{
    HttpMethod, HttpRequestError, HttpResponse, HttpStatus, TrustedProxies, Uninit,
};
use crate::router::Router;
use crate::server::per_core::set_current_worker_id;
use crate::tls::{AcmeChallengeStore, TlsBackgroundTask, TlsConfigError};
use active_set::ActiveSet;
use builder::HttpServerBuilder;
use bytes::Bytes;
pub use call::HttpCall;
pub(crate) use connection::{Connection, WorkerScratch};
pub use per_core::{CachePadded, PerCore, worker_id};
use request_info::RequestInfo;
use std::cell::Cell;
use std::net::{SocketAddr, TcpListener};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::task::Poll;
use tokio::sync::Notify;
use tokio_rustls::TlsAcceptor;
use transport::Transport;

const ACCEPT_BATCH: usize = 32;

thread_local! {
    static WORKER_DRAIN_STATE: (Cell<usize>, Rc<Notify>) =
        (Cell::new(0), Rc::new(Notify::new()));
}

#[derive(thiserror::Error, Debug)]
pub enum HttpServerError {
    #[error("address 0.0.0.0:{0} is already in use")]
    AddrInUse(u16),
    #[error("{0}")]
    Tls(#[from] TlsConfigError),
}

pub struct RestartContext {
    pub active_requests: Vec<RequestInfo>,
}

type RestartHook =
    Box<dyn FnOnce(RestartContext) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send>;

pub(crate) struct RestartRequest {
    pub(crate) force: bool,
    pub(crate) pre_hook: RestartHook,
}

#[derive(Clone)]
pub struct RestartHandle {
    restart_tx: tokio::sync::mpsc::UnboundedSender<RestartRequest>,
}

impl RestartHandle {
    pub fn restart<F, Fut>(&self, force: bool, pre_hook: F)
    where
        F: FnOnce(RestartContext) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let _ = self.restart_tx.send(RestartRequest {
            force,
            pre_hook: Box::new(move |ctx| Box::pin(pre_hook(ctx))),
        });
    }
}

#[derive(Clone)]
struct ThreadDrainBarrier {
    tx: Arc<tokio::sync::watch::Sender<usize>>,
}

impl ThreadDrainBarrier {
    fn new(thread_count: usize) -> Self {
        let (tx, _) = tokio::sync::watch::channel(thread_count);
        Self { tx: Arc::new(tx) }
    }

    fn thread_done(&self) {
        self.tx.send_modify(|remaining| {
            *remaining = remaining.saturating_sub(1);
        });
    }

    async fn wait_all_done(&self) {
        let mut rx = self.tx.subscribe();
        let _ = rx.wait_for(|&remaining| remaining == 0).await;
    }
}

struct LocalTaskGuard;

impl LocalTaskGuard {
    #[inline]
    fn new() -> Self {
        WORKER_DRAIN_STATE.with(|(count, _)| {
            count.set(count.get() + 1);
        });
        Self
    }

    #[inline]
    async fn wait_for_zero() {
        let notify = WORKER_DRAIN_STATE.with(|(_, n)| n.clone());
        loop {
            let notified = notify.notified();
            tokio::pin!(notified);
            if WORKER_DRAIN_STATE.with(|(c, _)| c.get()) == 0 {
                break;
            }
            notified.await;
        }
    }
}

impl Drop for LocalTaskGuard {
    #[inline]
    fn drop(&mut self) {
        WORKER_DRAIN_STATE.with(|(count, notify)| {
            let next = count.get().saturating_sub(1);
            count.set(next);
            if next == 0 {
                notify.notify_one();
            }
        });
    }
}

thread_local! {
    static WORKER_REQ_ID: Cell<(u64, u64)> = const { Cell::new((0, 0)) };
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestId(u128);

impl RequestId {
    #[inline]
    pub const fn as_u128(self) -> u128 {
        self.0
    }

    #[inline]
    pub fn encode<'a>(self, buf: &'a mut [u8; 33]) -> &'a str {
        const HEX: &[u8; 16] = b"0123456789abcdef";

        let prefix = (self.0 >> 64) as u64;
        for i in 0..16 {
            let nibble = ((prefix >> (60 - i * 4)) & 0xF) as usize;
            buf[i] = HEX[nibble];
        }
        buf[16] = b'-';

        let seq = self.0 as u64;
        if seq == 0 {
            buf[17] = b'0';
            return unsafe { std::str::from_utf8_unchecked(&buf[..18]) };
        }

        let mut pos = 33usize;
        let mut v = seq;
        while v > 0 {
            pos -= 1;
            buf[pos] = HEX[(v & 0xF) as usize];
            v >>= 4;
        }
        let seq_len = 33 - pos;
        buf.copy_within(pos..33, 17);
        unsafe { std::str::from_utf8_unchecked(&buf[..17 + seq_len]) }
    }
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut buf = [0u8; 33];
        f.write_str(self.encode(&mut buf))
    }
}

impl std::fmt::Debug for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

#[inline]
fn init_worker_request_id_gen(worker_id: usize) {
    let stack_addr = (&worker_id as *const usize) as u64;
    let code_addr = (init_worker_request_id_gen as *const ()) as u64;
    let time_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);

    let mut x = time_ns ^ stack_addr.rotate_left(17) ^ code_addr.rotate_left(33);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^= x >> 31;

    let prefix = ((x & !0xFFFu64) | ((worker_id as u64) & 0xFFFu64)).max(1);
    WORKER_REQ_ID.set((prefix, 0));
}

#[inline(always)]
fn next_request_id() -> RequestId {
    WORKER_REQ_ID.with(|cell| {
        let (mut prefix, seq) = cell.get();
        if prefix == 0 {
            init_worker_request_id_gen(0xFFF);
            prefix = cell.get().0;
        }
        let next_seq = seq.wrapping_add(1);
        cell.set((prefix, next_seq));
        RequestId(((prefix as u128) << 64) | (next_seq as u128))
    })
}

#[inline]
fn is_transient_accept_error(e: &std::io::Error) -> bool {
    if matches!(
        e.kind(),
        std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::Interrupted
    ) {
        return true;
    }
    #[cfg(target_os = "linux")]
    if let Some(raw) = e.raw_os_error() {
        return matches!(
            raw,
            libc::EAGAIN
                | libc::ENETDOWN
                | libc::EPROTO
                | libc::ENOPROTOOPT
                | libc::EHOSTDOWN
                | libc::ENONET
                | libc::EHOSTUNREACH
                | libc::EOPNOTSUPP
                | libc::ENETUNREACH
        );
    }
    false
}

#[inline]
fn build_worker_local_runtime() -> tokio::runtime::LocalRuntime {
    let mut builder = tokio::runtime::Builder::new_current_thread();
    builder
        .enable_all()
        .event_interval(31)
        .global_queue_interval(61)
        .max_io_events_per_tick(256)
        .max_io_events_per_busy_tick(32);

    builder
        .build_local(tokio::runtime::LocalOptions::default())
        .expect("failed to build worker LocalRuntime")
}

#[cfg(target_os = "linux")]
fn allowed_linux_cpus() -> Vec<usize> {
    unsafe {
        let mut cpuset: libc::cpu_set_t = std::mem::zeroed();
        if libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut cpuset) == 0 {
            let mut cpus = Vec::new();
            for cpu in 0..libc::CPU_SETSIZE as usize {
                if libc::CPU_ISSET(cpu, &cpuset) {
                    cpus.push(cpu);
                }
            }
            if !cpus.is_empty() {
                return cpus;
            }
        }
    }
    Vec::new()
}

fn pin_current_thread_to_cpu(cpu_id: Option<usize>) {
    #[cfg(target_os = "linux")]
    if let Some(cpu) = cpu_id {
        unsafe {
            let mut cpuset: libc::cpu_set_t = std::mem::zeroed();
            libc::CPU_ZERO(&mut cpuset);
            libc::CPU_SET(cpu, &mut cpuset);
            libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &cpuset);
            libc::prctl(libc::PR_SET_TIMERSLACK, 1, 0, 0, 0);
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = cpu_id;

    #[cfg(target_os = "macos")]
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INITIATED, 0);
    }
}

#[cfg(target_os = "linux")]
fn prewarm_linux_fd_table() {
    use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};

    unsafe {
        let mut rlim: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim) != 0 {
            return;
        }
        let soft_limit = rlim.rlim_cur as u64;
        if soft_limit <= 512 {
            return;
        }
        let target = (soft_limit - 256).min(65_536) as libc::c_int;
        if let Ok(dev_null) = std::fs::File::open("/dev/null") {
            let dup_fd = libc::fcntl(dev_null.as_raw_fd(), libc::F_DUPFD_CLOEXEC, target);
            if dup_fd >= 0 {
                let _owned = OwnedFd::from_raw_fd(dup_fd);
            }
        }
    }
}

pub(crate) struct WorkerLocalState {
    pub(crate) server: Arc<HttpServer>,
    pub(crate) keep_alive_header: Rc<[u8]>,
    pub(crate) trusted_proxies: Rc<TrustedProxies>,
    pub(crate) restart_handle: Rc<RestartHandle>,
}

impl WorkerLocalState {
    fn new(server: Arc<HttpServer>, restart_handle: RestartHandle) -> Self {
        let keep_alive_header = Rc::from(&*server.keep_alive_header);
        let trusted_proxies = Rc::new((*server.trusted_proxies).clone());
        let restart_handle = Rc::new(restart_handle);
        Self {
            server,
            keep_alive_header,
            trusted_proxies,
            restart_handle,
        }
    }
}

pub type ActiveRequest = ActiveSet<RequestId, RequestInfo>;

type OnRequestComplete =
    Arc<dyn Fn(RequestInfo) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

pub struct HttpServer {
    router: Router,
    listeners: Vec<TcpListener>,
    acme_http_listener: Option<TcpListener>,
    worker_count: usize,
    keep_alive_header: Arc<[u8]>,
    active_requests: ActiveRequest,
    on_request_complete: Option<OnRequestComplete>,
    cors: Option<Cors>,
    tls: Option<TlsAcceptor>,
    tls_background: Option<TlsBackgroundTask>,
    acme_challenges: Arc<AcmeChallengeStore>,
    acme_webroot: Option<String>,
    request_timeout: std::time::Duration,
    keep_alive_timeout: std::time::Duration,
    restart_stability_window: std::time::Duration,
    restart_pre_hook_timeout: std::time::Duration,
    max_body_size: usize,
    exe_path: Option<std::path::PathBuf>,
    trusted_proxies: Arc<TrustedProxies>,
}

impl HttpServer {
    pub fn builder(port: u16) -> HttpServerBuilder {
        HttpServerBuilder::new(port)
    }

    pub fn listen(mut self) -> (RestartHandle, impl Future<Output = ()>) {
        let listeners = std::mem::take(&mut self.listeners);
        let acme_http_listener = self.acme_http_listener.take();
        let tls_background = self.tls_background.take();
        let worker_count = self.worker_count.max(1);
        let server = Arc::new(self);

        let (restart_tx, mut restart_rx) = tokio::sync::mpsc::unbounded_channel::<RestartRequest>();
        let (result_tx, mut result_rx) =
            tokio::sync::mpsc::unbounded_channel::<Result<RestartRequest, ()>>();

        let handle = RestartHandle {
            restart_tx: restart_tx.clone(),
        };

        let fut = async move {
            let mut restart_in_progress = false;

            let child_pid: Arc<std::sync::Mutex<Option<u32>>> =
                Arc::new(std::sync::Mutex::new(None));

            let connection_handle = RestartHandle {
                restart_tx: restart_tx.clone(),
            };

            #[cfg(unix)]
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();

            let (stop_accept_tx, stop_accept_rx) = tokio::sync::watch::channel(false);

            let drain_barrier = Self::spawn_workers(
                listeners,
                worker_count,
                server.clone(),
                connection_handle,
                stop_accept_rx.clone(),
            );

            let acme_http_handle = acme_http_listener.map(|std_http| {
                let srv = server.clone();
                let mut stop_rx = stop_accept_rx.clone();
                tokio::spawn(async move {
                    std_http.set_nonblocking(true).unwrap();
                    let Ok(listener) = tokio::net::TcpListener::from_std(std_http) else {
                        return;
                    };
                    loop {
                        tokio::select! {
                            biased;
                            res = stop_rx.changed() => {
                                if res.is_err() || *stop_rx.borrow() {
                                    break;
                                }
                            }
                            accepted = listener.accept() => {
                                if let Ok((stream, _)) = accepted {
                                    let _ = stream.set_nodelay(true);
                                    let s = srv.clone();
                                    tokio::spawn(async move {
                                        s.handle_acme_http_conn(stream).await;
                                    });
                                }
                            }
                        }
                    }
                })
            });

            let tls_bg_handle = tls_background.map(|bg| {
                tokio::spawn(async move {
                    match bg {
                        TlsBackgroundTask::AutoReload {
                            cert_path,
                            key_path,
                            interval,
                            resolver,
                        } => loop {
                            tokio::time::sleep(interval).await;
                            if let Ok((certs, key)) =
                                crate::tls::load_from_files(&cert_path, &key_path)
                            {
                                if let Ok(ck) = crate::tls::build_certified_key(certs, key) {
                                    resolver.update(Arc::new(ck));
                                }
                            }
                        },
                        TlsBackgroundTask::Acme {
                            config,
                            resolver,
                            http_challenges,
                        } => {
                            crate::tls::run_acme_loop(config, resolver, http_challenges).await;
                        }
                    }
                })
            });

            let force: bool = loop {
                tokio::select! {
                    Some(restart_req) = restart_rx.recv() => {
                        if restart_in_progress {
                            println!("Restart already in progress, ignoring duplicate.");
                            continue;
                        }

                        restart_in_progress = true;
                        println!("Restart triggered. Force: {}", restart_req.force);

                        let exe_path = match &server.exe_path {
                            Some(p) if p.is_absolute() && p.exists() => p.clone(),
                            _ => match std::env::current_exe() {
                                Ok(p) if p.is_absolute() && p.exists() => p,
                                Ok(p) => {
                                    println!(
                                        "Restart aborted: executable path '{}' is unavailable.",
                                        p.display()
                                    );
                                    restart_in_progress = false;
                                    continue;
                                }
                                Err(e) => {
                                    println!("Failed to get executable path: {}", e);
                                    restart_in_progress = false;
                                    continue;
                                }
                            },
                        };

                        let mut child = match std::process::Command::new(&exe_path)
                            .args(std::env::args().skip(1))
                            .spawn()
                        {
                            Ok(c) => c,
                            Err(e) => {
                                println!("Failed to spawn new process: {}", e);
                                restart_in_progress = false;
                                continue;
                            }
                        };

                        *child_pid.lock().unwrap() = Some(child.id());

                        println!(
                            "New process spawned (PID {}). Verifying stability for {}s...",
                            child.id(),
                            server.restart_stability_window.as_secs(),
                        );

                        let result_tx = result_tx.clone();
                        let stability_window = server.restart_stability_window;

                        tokio::spawn(async move {
                            tokio::time::sleep(stability_window).await;

                            match child.try_wait() {
                                Ok(Some(status)) => {
                                    println!(
                                        "New instance crashed with status: {}",
                                        status
                                    );
                                    let _ = result_tx.send(Err(()));
                                }
                                Ok(None) => {
                                    println!("New instance appears stable.");
                                    let _ = result_tx.send(Ok(restart_req));
                                }
                                Err(e) => {
                                    println!(
                                        "Failed to check new instance status: {}. Aborting.",
                                        e
                                    );
                                    let _ = result_tx.send(Err(()));
                                }
                            }
                        });
                    }

                    result = result_rx.recv() => {
                        match result {
                            Some(Ok(restart_req)) => {
                                println!(
                                    "New instance stable. Stopping accept loops..."
                                );

                                let _ = stop_accept_tx.send(true);

                                let ctx = RestartContext {
                                    active_requests: server.active_requests
                                        .snapshot()
                                        .into_values()
                                        .collect(),
                                };

                                println!(
                                    "Running pre-hook with {} active requests...",
                                    ctx.active_requests.len()
                                );

                                match tokio::time::timeout(
                                    server.restart_pre_hook_timeout,
                                    (restart_req.pre_hook)(ctx),
                                ).await {
                                    Ok(_) => {
                                        println!("Pre-hook complete. Handing over.");
                                    }
                                    Err(_) => {
                                        println!(
                                            "Pre-hook timed out after {}s. Proceeding.",
                                            server.restart_pre_hook_timeout.as_secs()
                                        );
                                    }
                                }

                                break restart_req.force;
                            }
                            Some(Err(_)) => {
                                println!(
                                    "Restart failed. Unlocking for future attempts."
                                );
                                *child_pid.lock().unwrap() = None;
                                restart_in_progress = false;
                            }
                            None => {
                                println!(
                                    "Result channel closed unexpectedly. Unlocking."
                                );
                                *child_pid.lock().unwrap() = None;
                                restart_in_progress = false;
                            }
                        }
                    }

                    _ = tokio::signal::ctrl_c() => {
                        if restart_in_progress {
                            if let Some(pid) = *child_pid.lock().unwrap() {
                                println!(
                                    "SIGINT during restart. Killing child PID {}.",
                                    pid
                                );
                                #[cfg(unix)]
                                unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL); }
                                #[cfg(not(unix))]
                                {
                                    let _ = std::process::Command::new("taskkill")
                                        .args(["/PID", &pid.to_string(), "/F"])
                                        .spawn();
                                }
                            }
                        }
                        println!("SIGINT received. Graceful shutdown.");
                        let _ = stop_accept_tx.send(true);
                        break false;
                    }

                    _ = async {
                        #[cfg(unix)]
                        sigterm.recv().await;
                        #[cfg(not(unix))]
                        std::future::pending::<()>().await;
                    } => {
                        if restart_in_progress {
                            if let Some(pid) = *child_pid.lock().unwrap() {
                                println!(
                                    "SIGTERM during restart. Killing child PID {}.",
                                    pid
                                );
                                #[cfg(unix)]
                                unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL); }
                                #[cfg(not(unix))]
                                {
                                    let _ = std::process::Command::new("taskkill")
                                        .args(["/PID", &pid.to_string(), "/F"])
                                        .spawn();
                                }
                            }
                        }
                        println!("SIGTERM received. Graceful shutdown.");
                        let _ = stop_accept_tx.send(true);
                        break false;
                    }
                }
            };

            if let Some(h) = tls_bg_handle {
                h.abort();
            }
            if let Some(h) = acme_http_handle {
                h.abort();
            }

            if force {
                println!("Force shutdown. Exiting immediately.");
                std::process::exit(0);
            } else {
                println!("Graceful shutdown. Waiting for active requests...");
                server.active_requests.wait_for_zero().await;
                println!("All requests complete. Waiting for threads to drain...");
                drain_barrier.wait_all_done().await;
                println!("All threads drained. Exiting.");
            }
        };

        (handle, fut)
    }

    fn spawn_workers(
        listeners: Vec<TcpListener>,
        worker_count: usize,
        server: Arc<Self>,
        connection_handle: RestartHandle,
        stop_accept_rx: tokio::sync::watch::Receiver<bool>,
    ) -> ThreadDrainBarrier {
        #[cfg(target_os = "linux")]
        let allowed_cpus = allowed_linux_cpus();
        #[cfg(not(target_os = "linux"))]
        let allowed_cpus: Vec<usize> = Vec::new();

        #[cfg(target_os = "linux")]
        {
            let drain_barrier = ThreadDrainBarrier::new(listeners.len());

            for (idx, std_listener) in listeners.into_iter().enumerate() {
                let server_clone = server.clone();
                let restart_handle = connection_handle.clone();
                let mut stop_rx = stop_accept_rx.clone();
                let barrier = drain_barrier.clone();
                let cpu_id = if allowed_cpus.is_empty() {
                    Some(idx)
                } else {
                    Some(allowed_cpus[idx % allowed_cpus.len()])
                };

                std::thread::Builder::new()
                    .name(format!("http-worker-{idx}"))
                    .spawn(move || {
                        pin_current_thread_to_cpu(cpu_id);
                        init_worker_request_id_gen(idx);
                        set_current_worker_id(idx);

                        let local_state =
                            std::rc::Rc::new(WorkerLocalState::new(server_clone, restart_handle));

                        let rt = build_worker_local_runtime();
                        rt.block_on(async move {
                            std_listener.set_nonblocking(true).unwrap();
                            let listener = tokio::net::TcpListener::from_std(std_listener).unwrap();

                            loop {
                                if *stop_rx.borrow() {
                                    break;
                                }

                                let outcome = tokio::select! {
                                    biased;
                                    res = stop_rx.changed() => {
                                        if res.is_err() || *stop_rx.borrow() {
                                            break;
                                        }
                                        continue;
                                    }
                                    res = std::future::poll_fn(|cx| {
                                        let mut accepted = 0usize;
                                        while accepted < ACCEPT_BATCH {
                                            match listener.poll_accept(cx) {
                                                Poll::Ready(Ok((stream, peer_addr))) => {
                                                    let _ = stream.set_nodelay(true);

                                                    let state = local_state.clone();

                                                    tokio::task::spawn_local(async move {
                                                        let _guard = LocalTaskGuard::new();

                                                        let transport = match &state.server.tls {
                                                            None => Transport::from_tcp(
                                                                stream, peer_addr,
                                                            ),
                                                            Some(acceptor) => {
                                                                match acceptor.accept(stream).await
                                                                {
                                                                    Ok(tls_stream) => {
                                                                        Transport::from_tls(
                                                                            tls_stream, peer_addr,
                                                                        )
                                                                    }
                                                                    Err(_) => return,
                                                                }
                                                            }
                                                        };

                                                        HttpServer::handle_connection(
                                                            transport, state,
                                                        )
                                                        .await;
                                                    });

                                                    accepted += 1;
                                                }
                                                Poll::Ready(Err(e)) => {
                                                    if is_transient_accept_error(&e) {
                                                        continue;
                                                    }
                                                    return Poll::Ready(Err(e));
                                                }
                                                Poll::Pending => {
                                                    return Poll::Pending;
                                                }
                                            }
                                        }
                                        Poll::Ready(Ok(true))
                                    }) => res,
                                };

                                match outcome {
                                    Ok(true) => tokio::task::yield_now().await,
                                    Ok(false) => {}
                                    Err(_) => {
                                        tokio::time::sleep(std::time::Duration::from_millis(5))
                                            .await;
                                    }
                                }
                            }

                            drop(listener);
                            LocalTaskGuard::wait_for_zero().await;
                        });

                        barrier.thread_done();
                    })
                    .expect("failed to spawn http-worker thread");
            }

            drain_barrier
        }

        // Non-Linux (macOS, Windows, BSD without SO_REUSEPORT_LB)
        #[cfg(not(target_os = "linux"))]
        {
            let _ = allowed_cpus;
            let total_threads = worker_count + listeners.len();
            let drain_barrier = ThreadDrainBarrier::new(total_threads);

            let mut worker_txs = Vec::with_capacity(worker_count);
            for idx in 0..worker_count {
                let (tx, mut rx) =
                    tokio::sync::mpsc::channel::<(std::net::TcpStream, SocketAddr)>(1024);
                worker_txs.push(tx);

                let server_clone = server.clone();
                let restart_handle = connection_handle.clone();
                let mut stop_rx = stop_accept_rx.clone();
                let barrier = drain_barrier.clone();

                std::thread::Builder::new()
                    .name(format!("http-worker-{idx}"))
                    .spawn(move || {
                        pin_current_thread_to_cpu(None);
                        init_worker_request_id_gen(idx);
                        set_current_worker_id(idx);

                        let local_state =
                            Rc::new(WorkerLocalState::new(server_clone, restart_handle));

                        let rt = build_worker_local_runtime();
                        rt.block_on(async move {
                            loop {
                                tokio::select! {
                                    biased;
                                    res = stop_rx.changed() => {
                                        if res.is_err() || *stop_rx.borrow() {
                                            break;
                                        }
                                    }
                                    msg = rx.recv() => {
                                        let Some((std_stream, peer_addr)) = msg else {
                                            break;
                                        };
                                        let Ok(stream) = tokio::net::TcpStream::from_std(std_stream) else {
                                            continue;
                                        };

                                        let state = local_state.clone();

                                        tokio::task::spawn_local(async move {
                                            let _guard = LocalTaskGuard::new();

                                            let transport = match &state.server.tls {
                                                None => Transport::from_tcp(stream, peer_addr),
                                                Some(acceptor) => match acceptor.accept(stream).await {
                                                    Ok(tls_stream) => Transport::from_tls(tls_stream, peer_addr),
                                                    Err(_) => return,
                                                },
                                            };

                                            HttpServer::handle_connection(transport, state).await;
                                        });
                                    }
                                }
                            }

                            // Drain any sockets already queued before shutdown
                            rx.close();
                            while let Some((std_stream, peer_addr)) = rx.recv().await {
                                let Ok(stream) = tokio::net::TcpStream::from_std(std_stream) else {
                                    continue;
                                };

                                let state = local_state.clone();

                                tokio::task::spawn_local(async move {
                                    let _guard = LocalTaskGuard::new();

                                    let transport = match &state.server.tls {
                                        None => Transport::from_tcp(stream, peer_addr),
                                        Some(acceptor) => match acceptor.accept(stream).await {
                                            Ok(tls_stream) => Transport::from_tls(tls_stream, peer_addr),
                                            Err(_) => return,
                                        },
                                    };

                                    HttpServer::handle_connection(transport, state).await;
                                });
                            }

                            LocalTaskGuard::wait_for_zero().await;
                        });

                        barrier.thread_done();
                    })
                    .expect("failed to spawn http-worker thread");
            }

            for (idx, std_listener) in listeners.into_iter().enumerate() {
                let txs = worker_txs.clone();
                let mut stop_rx = stop_accept_rx.clone();
                let barrier = drain_barrier.clone();

                std::thread::Builder::new()
                    .name(format!("http-acceptor-{idx}"))
                    .spawn(move || {
                        pin_current_thread_to_cpu(None);

                        let rt = build_worker_local_runtime();
                        rt.block_on(async move {
                            std_listener.set_nonblocking(true).unwrap();

                            #[cfg(unix)]
                            let async_fd = unsafe {
                                // SAFETY: `std_listener` is an owned, valid `std::net::TcpListener`
                                // whose FD stays open and unchanged until `async_fd` is dropped.
                                tokio::io::unix::AsyncFd::register_with_interest(
                                    std_listener,
                                    tokio::io::Interest::READABLE,
                                )
                            }
                            .expect("failed to register listener with AsyncFd");

                            #[cfg(not(unix))]
                            let listener = tokio::net::TcpListener::from_std(std_listener).unwrap();

                            let mut batch: Vec<(std::net::TcpStream, SocketAddr)> =
                                Vec::with_capacity(ACCEPT_BATCH);
                            let mut next_worker = 0usize;

                            loop {
                                if *stop_rx.borrow() {
                                    break;
                                }

                                batch.clear();
                                let outcome = tokio::select! {
                                    biased;
                                    res = stop_rx.changed() => {
                                        if res.is_err() || *stop_rx.borrow() {
                                            break;
                                        }
                                        continue;
                                    }
                                    res = std::future::poll_fn(|cx| {
                                        #[cfg(unix)]
                                        {
                                            loop {
                                                let mut guard = match async_fd.poll_read_ready(cx) {
                                                    Poll::Ready(Ok(g)) => g,
                                                    Poll::Ready(Err(e)) => {
                                                        return Poll::Ready(Err(e));
                                                    }
                                                    Poll::Pending => {
                                                        if batch.is_empty() {
                                                            return Poll::Pending;
                                                        }
                                                        return Poll::Ready(Ok(false));
                                                    }
                                                };

                                                while batch.len() < ACCEPT_BATCH {
                                                    match guard
                                                        .try_io(|inner| inner.get_ref().accept())
                                                    {
                                                        Ok(Ok((std_stream, peer_addr))) => {
                                                            let _ =
                                                                std_stream.set_nonblocking(true);
                                                            let _ = std_stream.set_nodelay(true);
                                                            batch.push((std_stream, peer_addr));
                                                        }
                                                        Ok(Err(e)) => {
                                                            if is_transient_accept_error(&e) {
                                                                continue;
                                                            }
                                                            return Poll::Ready(Err(e));
                                                        }
                                                        Err(_would_block) => {
                                                            if !batch.is_empty() {
                                                                return Poll::Ready(Ok(false));
                                                            }
                                                            break;
                                                        }
                                                    }
                                                }

                                                if batch.len() >= ACCEPT_BATCH {
                                                    return Poll::Ready(Ok(true));
                                                }
                                            }
                                        }

                                        #[cfg(not(unix))]
                                        {
                                            while batch.len() < ACCEPT_BATCH {
                                                match listener.poll_accept(cx) {
                                                    Poll::Ready(Ok((stream, peer_addr))) => {
                                                        let _ = stream.set_nodelay(true);
                                                        if let Ok(std_stream) = stream.into_std() {
                                                            batch.push((std_stream, peer_addr));
                                                        }
                                                    }
                                                    Poll::Ready(Err(e)) => {
                                                        if is_transient_accept_error(&e) {
                                                            continue;
                                                        }
                                                        return Poll::Ready(Err(e));
                                                    }
                                                    Poll::Pending => {
                                                        if batch.is_empty() {
                                                            return Poll::Pending;
                                                        }
                                                        return Poll::Ready(Ok(false));
                                                    }
                                                }
                                            }
                                            Poll::Ready(Ok(true))
                                        }
                                    }) => res,
                                };

                                for conn in batch.drain(..) {
                                    let mut pending = Some(conn);
                                    for offset in 0..txs.len() {
                                        let target = (next_worker + offset) % txs.len();
                                        if let Some(item) = pending.take() {
                                            match txs[target].try_send(item) {
                                                Ok(()) => {
                                                    next_worker = (target + 1) % txs.len();
                                                    break;
                                                }
                                                Err(
                                                    tokio::sync::mpsc::error::TrySendError::Full(
                                                        returned,
                                                    ),
                                                ) => {
                                                    pending = Some(returned);
                                                }
                                                Err(
                                                    tokio::sync::mpsc::error::TrySendError::Closed(
                                                        _,
                                                    ),
                                                ) => {
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                    if let Some(item) = pending {
                                        let target = next_worker;
                                        next_worker = (next_worker + 1) % txs.len();
                                        let _ = txs[target].send(item).await;
                                    }
                                }

                                match outcome {
                                    Ok(true) => tokio::task::yield_now().await,
                                    Ok(false) => {}
                                    Err(_) => {
                                        tokio::time::sleep(std::time::Duration::from_millis(5))
                                            .await;
                                    }
                                }
                            }

                            #[cfg(unix)]
                            drop(async_fd);
                            #[cfg(not(unix))]
                            drop(listener);
                        });

                        barrier.thread_done();
                    })
                    .expect("failed to spawn http-acceptor thread");
            }

            drain_barrier
        }
    }

    async fn handle_connection(transport: Transport, state: std::rc::Rc<WorkerLocalState>) {
        let srv_state = state.clone();
        let idle_sleep = tokio::time::sleep(srv_state.server.keep_alive_timeout);
        tokio::pin!(idle_sleep);
        let mut last_timer_sec = connection::coarse_elapsed_secs();

        let mut connection = Connection::with_worker_state(transport, state);

        loop {
            let request_id = next_request_id();

            match srv_state
                .server
                .handle_single_request(
                    connection,
                    request_id,
                    idle_sleep.as_mut(),
                    &mut last_timer_sec,
                )
                .await
            {
                Some(returned) => connection = returned,
                None => break,
            }
        }
    }

    async fn handle_single_request(
        &self,
        connection: Connection,
        request_id: RequestId,
        idle_sleep: Pin<&mut tokio::time::Sleep>,
        last_timer_sec: &mut u64,
    ) -> Option<Connection> {
        let mut call: HttpCall = match HttpCall::parse(
            connection,
            request_id,
            self.max_body_size,
            self.keep_alive_timeout,
            self.request_timeout,
            idle_sleep,
            last_timer_sec,
        )
        .await
        {
            Ok(Some(call)) => call,
            Ok(None) => {
                return None;
            }
            Err((mut stream, err)) => {
                let status = match err {
                    HttpRequestError::PayloadTooLarge => HttpStatus::ContentTooLarge,
                    HttpRequestError::RequestLineTooLong => HttpStatus::UriTooLong,
                    HttpRequestError::HeadersTooLarge => HttpStatus::RequestHeaderFieldsTooLarge,
                    HttpRequestError::Timeout => HttpStatus::RequestTimeout,
                    _ => HttpStatus::BadRequest,
                };

                stream.set_keep_alive(false);

                let _ = stream
                    .write_response(
                        HttpResponse::new(Bytes::from_static(b"HTTP/1.1"), request_id)
                            .status_code(status)
                            .empty()
                            .into_parts(),
                    )
                    .await;
                let _ = stream.shutdown().await;
                return None;
            }
        };

        let keep_alive = call
            .header("connection")
            .map(|v| !v.eq_ignore_ascii_case("close"))
            .unwrap_or(true);

        call.connection.get_mut().set_keep_alive(keep_alive);

        if let Some(cors) = &self.cors {
            if *call.method() == HttpMethod::OPTIONS {
                cors.handle_preflight(&mut call).await;
                return self.finish(call, keep_alive).await;
            }
        }

        let _active_guard = if let Some(on_complete) = &self.on_request_complete {
            let guard = self.active_requests.insert(
                request_id,
                RequestInfo::new(
                    request_id,
                    Arc::from(call.request.route.as_str()),
                    call.request.method().clone(),
                ),
            );

            let g = guard.clone();
            let on_complete = on_complete.clone();
            call.set_response_hook(Arc::new(move |status_code| {
                g.update(|info| {
                    info.set_response_status(status_code.clone());
                    info.mark_as_end();
                });
                if let Some(mut info) = g.value() {
                    info.set_response_status(status_code);
                    let fut = on_complete(info);
                    let hg = LocalTaskGuard::new();
                    tokio::task::spawn_local(async move {
                        let _hg = hg;
                        fut.await;
                    });
                }
            }));

            Some(guard)
        } else {
            None
        };

        let is_head = *call.method() == HttpMethod::HEAD;
        if is_head {
            call.set_suppress_body();
        }

        let method_to_match = if is_head {
            HttpMethod::GET
        } else {
            call.request.method().clone()
        };

        let matched = match self.router.find_or_allowed_into(
            &method_to_match,
            &call.request.route,
            &mut call.request.path_params,
        ) {
            Ok(route) => route,
            Err(allowed_mask) => {
                if allowed_mask == 0
                    && matches!(method_to_match, HttpMethod::GET | HttpMethod::HEAD)
                    && let Some(key_auth) = self.resolve_acme_http01(&call.request.route)
                {
                    let _ = call
                        .response()
                        .status_code(HttpStatus::Ok)
                        .text(key_auth)
                        .send()
                        .await;
                    return self.finish(call, keep_alive).await;
                }

                let mut resp_fields = WorkerScratch::take_resp_fields();
                resp_fields.clear();

                let mut resp = self.apply_cors_to_uninit(
                    &call,
                    HttpResponse::new_with_fields(
                        call.request.http_version().clone(),
                        request_id,
                        resp_fields,
                    ),
                );

                let standard_mask = allowed_mask & 0b1_1111;
                if standard_mask == 0 {
                    resp = resp.status_code(HttpStatus::NotFound);
                } else {
                    const METHOD_NAMES: [&str; 5] = ["GET", "POST", "PUT", "DELETE", "PATCH"];
                    let mut allow = String::with_capacity(32);
                    for (idx, name) in METHOD_NAMES.iter().enumerate() {
                        if (standard_mask & (1 << idx)) != 0 {
                            if !allow.is_empty() {
                                allow.push_str(", ");
                            }
                            allow.push_str(name);
                            if idx == 0 {
                                allow.push_str(", HEAD");
                            }
                        }
                    }

                    resp = resp
                        .status_code(HttpStatus::MethodNotAllowed)
                        .add_header("allow", allow);
                }

                let _ = call
                    .connection
                    .get_mut()
                    .write_response(resp.empty().into_parts())
                    .await;
                return self.finish(call, keep_alive).await;
            }
        };

        for &middleware in matched.middleware {
            middleware(&mut call).await;
            if call.response_sent() {
                return self.finish(call, keep_alive).await;
            }
        }

        (matched.handler)(&mut call).await;

        self.finish(call, keep_alive).await
    }

    fn apply_cors_to_uninit<'a>(
        &self,
        call: &HttpCall,
        resp: HttpResponse<'a, Uninit>,
    ) -> HttpResponse<'a, Uninit> {
        match &self.cors {
            Some(cors) => cors.add_cors_headers(call, resp),
            None => resp,
        }
    }

    async fn finish(&self, mut call: HttpCall, mut keep_alive: bool) -> Option<Connection> {
        if keep_alive && call.discard_unread_body().await.is_err() {
            keep_alive = false;
        }

        if keep_alive {
            WorkerScratch::recycle_request(
                call.request.field_lines,
                call.request.route,
                call.request.path_params,
                call.extras.into_inner(),
            );
            let mut conn = call.connection.into_inner();
            conn.recycle_reader_if_empty();
            Some(conn)
        } else {
            let mut conn = call.connection.into_inner();
            let _ = conn.shutdown().await;
            None
        }
    }
}

impl HttpServer {
    #[inline]
    fn resolve_acme_http01(&self, route: &str) -> Option<String> {
        let token = route.strip_prefix("/.well-known/acme-challenge/")?;
        let token = token.split('?').next().unwrap_or("");
        if token.is_empty()
            || token.len() > 128
            || !token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return None;
        }
        self.acme_challenges.get_http01(token).or_else(|| {
            self.acme_webroot.as_deref().and_then(|root| {
                std::fs::read_to_string(format!("{root}/.well-known/acme-challenge/{token}")).ok()
            })
        })
    }

    async fn handle_acme_http_conn(&self, mut stream: tokio::net::TcpStream) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut buf = [0u8; 4096];
        let mut total = 0usize;
        let read_res = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let n = stream.read(&mut buf[total..]).await?;
                if n == 0 {
                    return Ok::<_, std::io::Error>(false);
                }
                total += n;
                if memchr::memmem::find(&buf[..total], b"\r\n\r\n").is_some() {
                    return Ok(true);
                }
                if total == buf.len() {
                    return Ok(false);
                }
            }
        })
        .await;

        let Ok(Ok(true)) = read_res else {
            return;
        };

        let Ok(req_str) = std::str::from_utf8(&buf[..total]) else {
            return;
        };
        let mut lines = req_str.split("\r\n");
        let Some(req_line) = lines.next() else {
            return;
        };
        let mut parts = req_line.split_whitespace();
        let (Some(method), Some(path)) = (parts.next(), parts.next()) else {
            return;
        };

        if (method == "GET" || method == "HEAD")
            && let Some(key_auth) = self.resolve_acme_http01(path)
        {
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                key_auth.len(),
                if method == "HEAD" { "" } else { &key_auth }
            );
            let _ = stream.write_all(resp.as_bytes()).await;
            let _ = stream.shutdown().await;
            return;
        }

        let host = lines.find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case("host").then(|| v.trim())
        });

        if let Some(h) = host
            && !h.is_empty()
            && !h.bytes().any(|b| b == b'\r' || b == b'\n')
            && path.starts_with('/')
            && !path.bytes().any(|b| b == b'\r' || b == b'\n')
        {
            let host_only = if h.starts_with('[') {
                h.find(']').map(|end| &h[..=end]).unwrap_or(h)
            } else {
                h.split(':').next().unwrap_or(h)
            };
            let resp = format!(
                "HTTP/1.1 301 Moved Permanently\r\nLocation: https://{host_only}{path}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = stream.write_all(resp.as_bytes()).await;
        } else {
            let _ = stream
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
        }
        let _ = stream.shutdown().await;
    }
}
