//! Unix-domain control channel and background job API.
//!
//! The same Axum router that serves the TCP listener is served on a Unix
//! socket. Requests authenticated with the control token are mapped to an
//! admin principal, so the CLI reuses the existing admin handlers. The TCP
//! listener never sees the token-principal marker and therefore cannot be
//! authenticated with the control token.
//!
//! Filesystem and peer rules:
//! - parent directory `0700` when this process creates it
//! - socket, `token`, `runtime.json`, and `control.lock` are `0600`
//! - on Linux, `SO_PEERCRED` must report the service UID or root
//! - a stale socket is unlinked only after a failed connect and while holding
//!   an exclusive `flock` on `control.lock`
//! - graceful shutdown removes the socket, token, and runtime metadata
//!
//! `sun_path` is limited to roughly 108 bytes on Linux; callers must keep the
//! configured path short. `docker exec -u <uid>` must use the service UID or
//! root because any other UID is rejected by peer-credential checking.

use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use axum::extract::connect_info::Connected;
use axum::extract::{ConnectInfo, Extension, Path as AxumPath, Query, State};
use axum::http::{Request, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex as AsyncMutex, oneshot, watch};

use crate::app::AppState;
use crate::auth::ControlPrincipal;
use crate::models::AppKind;
use crate::rename::{self, DownloaderKind};
use crate::sync;

/// Maximum retained log events per job. Older events are dropped and counted.
const MAX_JOB_EVENTS: usize = 200;
/// Tokens are 32 random bytes, base64url without padding.
const TOKEN_BYTES: usize = 32;
/// Linux `sockaddr_un.sun_path` is 108 bytes including the trailing NUL.
const MAX_SOCKET_PATH_BYTES: usize = 107;

#[derive(Debug, Clone, Copy, Default)]
pub struct ControlPeer {
    pub uid: Option<u32>,
}

impl Connected<axum::serve::IncomingStream<'_, UnixListener>> for ControlPeer {
    fn connect_info(stream: axum::serve::IncomingStream<'_, UnixListener>) -> Self {
        Self {
            uid: peer_uid(stream.io()).ok(),
        }
    }
}

#[cfg(target_os = "linux")]
fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    let mut credentials = std::mem::MaybeUninit::<libc::ucred>::uninit();
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `getsockopt` writes a `ucred` into the provided buffer and sets
    // `length`; the fd is valid for the lifetime of `stream`.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            credentials.as_mut_ptr().cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `getsockopt` succeeded and initialized the structure.
    Ok(unsafe { credentials.assume_init() }.uid)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: both pointers are valid for the duration of the call.
    let result = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(uid)
}

/// Runtime metadata shared with control handlers. It never contains a hash or
/// a credential beyond the control token itself.
pub struct ControlInfo {
    pub socket_path: PathBuf,
    pub runtime_file: PathBuf,
    pub token_file: PathBuf,
    pub token: String,
    pub started_at: Instant,
    pub started_at_unix: u64,
    pub pid: u32,
    pub service_uid: u32,
    pub jobs: JobManager,
}

impl ControlInfo {
    fn peer_allowed(&self, uid: u32) -> bool {
        uid == self.service_uid || uid == 0
    }
}

/// A prepared control socket. The lock is held until this value is dropped.
pub struct PreparedControl {
    pub info: Arc<ControlInfo>,
    listener: Option<UnixListener>,
    socket_path: PathBuf,
    runtime_file: PathBuf,
    token_file: PathBuf,
    _lock: File,
    cleaned: bool,
}

impl PreparedControl {
    /// Take the bound listener once it is time to serve requests.
    pub fn take_listener(&mut self) -> Result<UnixListener> {
        self.listener
            .take()
            .context("control listener was already taken")
    }

    /// Remove socket and runtime metadata. Safe to call more than once.
    pub fn cleanup(&mut self) {
        if self.cleaned {
            return;
        }
        self.cleaned = true;
        let _ = fs::remove_file(&self.socket_path);
        let _ = fs::remove_file(&self.runtime_file);
        let _ = fs::remove_file(&self.token_file);
    }
}

impl Drop for PreparedControl {
    fn drop(&mut self) {
        self.cleanup();
    }
}

pub async fn prepare(config: &crate::config::Config) -> Result<PreparedControl> {
    let socket_path = config.control_socket.clone();
    validate_socket_path(&socket_path)?;
    let directory = socket_path.parent().unwrap_or_else(|| Path::new("."));
    let directory_created = !directory.exists();
    fs::create_dir_all(directory)
        .with_context(|| format!("create control directory {}", directory.display()))?;
    if directory_created || directory == Path::new("/tmp/jproxy") {
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("secure control directory {}", directory.display()))?;
    }

    let lock_path = directory.join("control.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)
        .with_context(|| format!("open control lock {}", lock_path.display()))?;
    // SAFETY: `flock` only reads the valid file descriptor and does not retain
    // the pointer. The lock is released when `lock` closes.
    let locked = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if locked != 0 {
        bail!(
            "control socket is locked by another process: {}",
            socket_path.display()
        );
    }

    remove_stale_socket(&socket_path)?;

    let token = generate_token()?;
    let token_file = directory.join("token");
    write_private(&token_file, token.as_bytes())?;

    let runtime_file = directory.join("runtime.json");
    let started_at_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let runtime = json!({
        "socket": socket_path,
        "pid": std::process::id(),
        "version": env!("CARGO_PKG_VERSION"),
        "token_file": token_file,
        "started_at": started_at_unix,
        "peer_policy": "same-uid-or-root",
    });
    write_private(
        &runtime_file,
        serde_json::to_vec_pretty(&runtime)
            .context("serialize control runtime metadata")?
            .as_slice(),
    )?;

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("bind control socket {}", socket_path.display()))?;
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("secure control socket {}", socket_path.display()))?;

    let info = Arc::new(ControlInfo {
        socket_path: socket_path.clone(),
        runtime_file: runtime_file.clone(),
        token_file: token_file.clone(),
        token,
        started_at: Instant::now(),
        started_at_unix,
        pid: std::process::id(),
        service_uid: effective_uid(),
        jobs: JobManager::default(),
    });

    Ok(PreparedControl {
        info,
        listener: Some(listener),
        socket_path,
        runtime_file,
        token_file,
        _lock: lock,
        cleaned: false,
    })
}

fn effective_uid() -> u32 {
    // SAFETY: `geteuid` has no preconditions.
    unsafe { libc::geteuid() }
}

fn validate_socket_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        bail!("CONTROL_SOCKET must not be empty");
    }
    let length = path.as_os_str().as_encoded_bytes().len();
    if length >= MAX_SOCKET_PATH_BYTES {
        bail!(
            "CONTROL_SOCKET is {length} bytes; Unix socket paths must be shorter than \
             {MAX_SOCKET_PATH_BYTES} bytes"
        );
    }
    Ok(())
}

fn remove_stale_socket(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("inspect {}", path.display()));
        }
    };
    if !metadata.file_type().is_socket() {
        bail!(
            "control path exists and is not a Unix socket: {}",
            path.display()
        );
    }
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(_) => bail!(
            "another control server is already listening on {}",
            path.display()
        ),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
            ) =>
        {
            fs::remove_file(path)
                .with_context(|| format!("remove stale control socket {}", path.display()))?;
            Ok(())
        }
        Err(error) => Err(error).with_context(|| {
            format!(
                "probe existing control socket {}; remove it manually if the service is stopped",
                path.display()
            )
        }),
    }
}

fn generate_token() -> Result<String> {
    let mut bytes = [0_u8; TOKEN_BYTES];
    getrandom::getrandom(&mut bytes)
        .map_err(|error| anyhow::anyhow!("read control token: {error}"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    file.write_all(contents)
        .with_context(|| format!("write {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("sync {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("secure {}", path.display()))?;
    Ok(())
}

fn token_matches(presented: &str, expected: &str) -> bool {
    let presented = presented.as_bytes();
    let expected = expected.as_bytes();
    if presented.len() != expected.len() {
        return false;
    }
    let mut difference = 0_u8;
    for (left, right) in presented.iter().zip(expected) {
        difference |= left ^ right;
    }
    difference == 0
}

/// Layer applied only to the Unix-domain listener. It verifies the peer UID,
/// maps a valid control token to [`ControlPrincipal`], and strips the
/// Authorization/control headers before the request reaches a handler.
pub async fn authenticate_control(
    State(info): State<Arc<ControlInfo>>,
    ConnectInfo(peer): ConnectInfo<ControlPeer>,
    mut request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let Some(uid) = peer.uid else {
        return forbidden("control peer credentials unavailable");
    };
    if !info.peer_allowed(uid) {
        return forbidden("control peer is not the service user or root");
    }

    let mut control_authenticated = false;
    let header_token = request
        .headers()
        .get("x-control-token")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    if let Some(presented) = header_token {
        if !token_matches(&presented, &info.token) {
            return forbidden("invalid control token");
        }
        control_authenticated = true;
    } else if let Some(value) = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        && token_matches(value, &info.token)
    {
        control_authenticated = true;
    }

    if control_authenticated {
        request
            .extensions_mut()
            .insert(ControlPrincipal { uid: Some(uid) });
        request.headers_mut().remove(header::AUTHORIZATION);
    }
    request.headers_mut().remove("x-control-token");
    request.extensions_mut().insert(info);
    next.run(request).await
}

fn forbidden(message: &'static str) -> Response {
    (StatusCode::FORBIDDEN, message).into_response()
}

/// Control endpoints are reachable only through the UDS listener. A valid Web
/// UI JWT on the TCP listener cannot call them because it never carries
/// [`ControlPrincipal`].
pub async fn require_control(request: Request<axum::body::Body>, next: Next) -> Response {
    if request.extensions().get::<ControlPrincipal>().is_some() {
        next.run(request).await
    } else {
        forbidden("control channel required")
    }
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/control/status", get(status))
        .route("/api/control/reload", post(reload))
        .route("/api/control/sync", post(start_sync))
        .route("/api/control/rename", post(start_rename))
        .route("/api/control/login", post(start_login))
        .route("/api/control/jobs", post(start_job).get(list_jobs))
        .route("/api/control/jobs/{id}", get(show_job))
        .route("/api/control/jobs/{id}/events", get(job_events))
        .route("/api/control/jobs/{id}/cancel", post(cancel_job))
        .route_layer(middleware::from_fn(require_control))
}

/// The full application router plus the UDS-only authentication layer. Serve
/// it with `into_make_service_with_connect_info::<ControlPeer>()`.
pub fn control_router(state: AppState, info: Arc<ControlInfo>) -> Router {
    crate::app::router(state).layer(middleware::from_fn_with_state(info, authenticate_control))
}

// ---------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct JobEvent {
    pub seq: u64,
    pub at: String,
    pub level: &'static str,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobView {
    pub id: String,
    pub kind: String,
    pub target: String,
    pub status: String,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub cancel_requested: bool,
    pub events_dropped: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub events: Option<Vec<JobEvent>>,
}

struct JobInner {
    view: JobView,
    events: VecDeque<JobEvent>,
    next_seq: u64,
    dropped: u64,
    cancel: Option<oneshot::Sender<()>>,
    revision: u64,
}

pub struct Job {
    id: String,
    inner: AsyncMutex<JobInner>,
    notify: watch::Sender<u64>,
}

impl Job {
    fn new(id: String, kind: String, target: String) -> (Self, oneshot::Receiver<()>) {
        let (cancel, cancel_rx) = oneshot::channel();
        let (notify, _) = watch::channel(0_u64);
        (
            Self {
                id: id.clone(),
                inner: AsyncMutex::new(JobInner {
                    view: JobView {
                        id,
                        kind,
                        target,
                        status: "queued".to_owned(),
                        created_at: now_rfc3339(),
                        started_at: None,
                        finished_at: None,
                        cancel_requested: false,
                        events_dropped: 0,
                        result: None,
                        error: None,
                        events: None,
                    },
                    events: VecDeque::new(),
                    next_seq: 0,
                    dropped: 0,
                    cancel: Some(cancel),
                    revision: 0,
                }),
                notify,
            },
            cancel_rx,
        )
    }

    fn id(&self) -> String {
        self.id.clone()
    }

    async fn snapshot(&self, include_events: bool) -> JobView {
        let inner = self.inner.lock().await;
        let mut view = inner.view.clone();
        view.events_dropped = inner.dropped;
        if include_events {
            view.events = Some(inner.events.iter().cloned().collect());
        }
        view
    }

    async fn start(&self) {
        let mut inner = self.inner.lock().await;
        inner.view.status = "running".to_owned();
        inner.view.started_at = Some(now_rfc3339());
        inner.revision += 1;
        let revision = inner.revision;
        drop(inner);
        let _ = self.notify.send(revision);
    }

    async fn log(&self, level: &'static str, message: impl Into<String>) {
        let mut inner = self.inner.lock().await;
        let seq = inner.next_seq;
        inner.next_seq += 1;
        inner.events.push_back(JobEvent {
            seq,
            at: now_rfc3339(),
            level,
            message: message.into(),
        });
        while inner.events.len() > MAX_JOB_EVENTS {
            inner.events.pop_front();
            inner.dropped += 1;
        }
        inner.view.events_dropped = inner.dropped;
        inner.revision += 1;
        let revision = inner.revision;
        drop(inner);
        let _ = self.notify.send(revision);
    }

    async fn finish(&self, status: &str, result: Option<Value>, error: Option<String>) {
        let mut inner = self.inner.lock().await;
        inner.view.status = status.to_owned();
        inner.view.finished_at = Some(now_rfc3339());
        inner.view.result = result;
        inner.view.error = error.map(|message| truncate_message(&message));
        inner.cancel = None;
        inner.revision += 1;
        let revision = inner.revision;
        drop(inner);
        let _ = self.notify.send(revision);
    }

    async fn request_cancel(&self) -> bool {
        let mut inner = self.inner.lock().await;
        if matches!(
            inner.view.status.as_str(),
            "succeeded" | "failed" | "cancelled"
        ) {
            return false;
        }
        inner.view.cancel_requested = true;
        if let Some(sender) = inner.cancel.take() {
            let _ = sender.send(());
        }
        inner.revision += 1;
        let revision = inner.revision;
        drop(inner);
        let _ = self.notify.send(revision);
        true
    }

    async fn events_after(&self, after: u64) -> (Vec<JobEvent>, JobView, bool) {
        let inner = self.inner.lock().await;
        let events = inner
            .events
            .iter()
            .filter(|event| event.seq >= after)
            .cloned()
            .collect();
        let mut view = inner.view.clone();
        view.events_dropped = inner.dropped;
        let done = matches!(
            inner.view.status.as_str(),
            "succeeded" | "failed" | "cancelled"
        );
        (events, view, done)
    }

    fn subscribe(&self) -> watch::Receiver<u64> {
        self.notify.subscribe()
    }
}

impl Drop for Job {
    fn drop(&mut self) {}
}

#[derive(Default)]
pub struct JobManager {
    jobs: Mutex<HashMap<String, Arc<Job>>>,
}

impl JobManager {
    fn create(&self, kind: &str, target: &str) -> (Arc<Job>, oneshot::Receiver<()>) {
        let id = random_id();
        let (job, cancel) = Job::new(id.clone(), kind.to_owned(), target.to_owned());
        let job = Arc::new(job);
        self.jobs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id, job.clone());
        (job, cancel)
    }

    fn get(&self, id: &str) -> Option<Arc<Job>> {
        self.jobs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(id)
            .cloned()
    }

    async fn views(&self, limit: usize) -> Vec<JobView> {
        let jobs: Vec<Arc<Job>> = {
            let jobs = self
                .jobs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            jobs.values().cloned().collect()
        };
        let mut views = Vec::with_capacity(jobs.len());
        for job in jobs {
            let inner = job.inner.lock().await;
            views.push(inner.view.clone());
        }
        views.sort_by(|left, right| right.created_at.cmp(&left.created_at));
        views.truncate(limit);
        views
    }

    async fn summary(&self) -> (usize, usize) {
        let jobs: Vec<Arc<Job>> = {
            let jobs = self
                .jobs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            jobs.values().cloned().collect()
        };
        let mut running = 0;
        for job in &jobs {
            if job.inner.lock().await.view.status == "running" {
                running += 1;
            }
        }
        (jobs.len(), running)
    }
}

fn random_id() -> String {
    let mut bytes = [0_u8; 16];
    // A failure here is extraordinarily unlikely; a time-derived id keeps job
    // creation available instead of failing the request.
    if getrandom::getrandom(&mut bytes).is_err() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        return format!("fallback-{nanos}");
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn truncate_message(message: &str) -> String {
    const LIMIT: usize = 2000;
    if message.chars().count() <= LIMIT {
        return message.to_owned();
    }
    let mut truncated: String = message.chars().take(LIMIT).collect();
    truncated.push('…');
    truncated
}

#[derive(Debug, Clone, Copy)]
enum Command {
    Sync(SyncTarget),
    Rename { kind: AppKind, dry_run: bool },
    Login(DownloaderKind),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncTarget {
    Sonarr,
    Radarr,
    Tmdb,
    Rules,
    All,
}

impl SyncTarget {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "sonarr" => Some(Self::Sonarr),
            "radarr" => Some(Self::Radarr),
            "tmdb" => Some(Self::Tmdb),
            "rules" => Some(Self::Rules),
            "all" => Some(Self::All),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Sonarr => "sonarr",
            Self::Radarr => "radarr",
            Self::Tmdb => "tmdb",
            Self::Rules => "rules",
            Self::All => "all",
        }
    }
}

#[derive(Debug, Serialize)]
struct JobAccepted {
    id: String,
    kind: String,
    target: String,
    status: &'static str,
}

fn spawn_command(
    state: &AppState,
    info: &Arc<ControlInfo>,
    command: Command,
    kind: &str,
    target: &str,
) -> Response {
    let (job, cancel) = info.jobs.create(kind, target);
    let accepted = JobAccepted {
        id: job.id(),
        kind: kind.to_owned(),
        target: target.to_owned(),
        status: "queued",
    };
    let state = state.clone();
    tokio::spawn(async move {
        run_job(state, job, command, cancel).await;
    });
    (StatusCode::ACCEPTED, Json(accepted)).into_response()
}

async fn run_job(state: AppState, job: Arc<Job>, command: Command, cancel: oneshot::Receiver<()>) {
    job.start().await;
    let outcome = tokio::select! {
        _ = cancel => {
            job.log("warn", "cancellation requested; the in-flight step was abandoned").await;
            job.finish("cancelled", None, None).await;
            return;
        }
        result = execute(&state, &job, command) => result,
    };
    match outcome {
        Ok(result) => {
            job.log("info", "completed").await;
            job.finish("succeeded", Some(result), None).await;
        }
        Err(error) => {
            let message = format!("{error:#}");
            tracing::warn!(%message, "control job failed");
            job.log("error", message.clone()).await;
            job.finish("failed", None, Some(message)).await;
        }
    }
}

async fn execute(state: &AppState, job: &Job, command: Command) -> Result<Value> {
    match command {
        Command::Sync(target) => execute_sync(state, job, target).await,
        Command::Rename { kind, dry_run } => {
            job.log(
                "info",
                format!(
                    "rename pass for {kind}{}",
                    if dry_run { " (dry run)" } else { "" }
                ),
            )
            .await;
            let report = rename::rename_once_with_options(state, kind, dry_run).await?;
            job.log(
                "info",
                format!(
                    "history={} planned_torrents={} planned_files={} renamed_torrents={} renamed_files={} errors={} dry_run={}",
                    report.history_items,
                    report.planned_torrents,
                    report.planned_files,
                    report.renamed_torrents,
                    report.renamed_files,
                    report.errors,
                    report.dry_run
                ),
            )
            .await;
            serde_json::to_value(report).context("serialize rename report")
        }
        Command::Login(kind) => {
            job.log("info", format!("login {}", kind.as_str())).await;
            let logged_in = rename::login_client(state, kind).await;
            job.log(
                if logged_in { "info" } else { "warn" },
                if logged_in {
                    format!("{} login succeeded", kind.as_str())
                } else {
                    format!("{} login failed or is not configured", kind.as_str())
                },
            )
            .await;
            Ok(json!({ "client": kind.as_str(), "logged_in": logged_in }))
        }
    }
}

async fn execute_sync(state: &AppState, job: &Job, target: SyncTarget) -> Result<Value> {
    let mut results = Vec::new();
    match target {
        SyncTarget::Sonarr => {
            results.push(sync_kind(state, job, AppKind::Sonarr).await?);
        }
        SyncTarget::Radarr => {
            results.push(sync_kind(state, job, AppKind::Radarr).await?);
        }
        SyncTarget::Tmdb => {
            job.log("info", "sync tmdb").await;
            let count = sync::sync_tmdb_once(state).await?;
            log_sync_count(job, "tmdb", count).await;
            results.push(json!({ "kind": "tmdb", "count": count }));
        }
        SyncTarget::Rules => {
            for kind in [AppKind::Sonarr, AppKind::Radarr] {
                job.log("info", format!("sync {kind} rules")).await;
                let count = sync::sync_rules_once(state, kind).await?;
                log_sync_count(job, &format!("{kind} rules"), count).await;
                results.push(json!({ "kind": format!("{kind}-rules"), "count": count }));
            }
        }
        SyncTarget::All => {
            for kind in [AppKind::Sonarr, AppKind::Radarr] {
                results.push(sync_kind(state, job, kind).await?);
            }
            job.log("info", "sync tmdb").await;
            let count = sync::sync_tmdb_once(state).await?;
            log_sync_count(job, "tmdb", count).await;
            results.push(json!({ "kind": "tmdb", "count": count }));
            for kind in [AppKind::Sonarr, AppKind::Radarr] {
                job.log("info", format!("sync {kind} rules")).await;
                let count = sync::sync_rules_once(state, kind).await?;
                log_sync_count(job, &format!("{kind} rules"), count).await;
                results.push(json!({ "kind": format!("{kind}-rules"), "count": count }));
            }
        }
    }
    Ok(json!({ "results": results }))
}

async fn sync_kind(state: &AppState, job: &Job, kind: AppKind) -> Result<Value> {
    job.log("info", format!("sync {kind} titles")).await;
    let count = sync::sync_titles_once(state, kind).await?;
    log_sync_count(job, kind.as_str(), count).await;
    Ok(json!({ "kind": kind.as_str(), "count": count }))
}

async fn log_sync_count(job: &Job, label: &str, count: Option<usize>) {
    match count {
        Some(count) => job.log("info", format!("{label}: {count}")).await,
        None => {
            job.log(
                "warn",
                format!("{label}: skipped (sync disabled or not configured)"),
            )
            .await;
        }
    }
}

// ---------------------------------------------------------------------------
// Control handlers
// ---------------------------------------------------------------------------

async fn status(
    State(state): State<AppState>,
    Extension(info): Extension<Arc<ControlInfo>>,
) -> Response {
    let snapshot = state.snapshot();
    let rows = crate::db::load_system_config_rows(&state.pool)
        .await
        .unwrap_or_default();
    let mut valid = 0_usize;
    let mut invalid_keys = Vec::new();
    let mut unconfigured_keys = Vec::new();
    for row in &rows {
        let configured = row
            .value
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty());
        if row.valid_status == 1 {
            valid += 1;
        } else if configured {
            invalid_keys.push(row.key.clone());
        } else {
            unconfigured_keys.push(row.key.clone());
        }
    }
    invalid_keys.sort();
    unconfigured_keys.sort();

    let (job_total, job_running) = info.jobs.summary().await;
    let qbittorrent_logged_in = state.downloaders.qbittorrent.is_login().await;
    let transmission_logged_in = state.downloaders.transmission.is_login().await;
    let sync_interval = snapshot.sync_interval;
    let sonarr_gate = state
        .sync
        .title_gate_remaining(AppKind::Sonarr, sync_interval)
        .await;
    let radarr_gate = state
        .sync
        .title_gate_remaining(AppKind::Radarr, sync_interval)
        .await;
    let tmdb_gate = state.sync.tmdb_gate_remaining(sync_interval).await;

    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "pid": info.pid,
        "uptime_seconds": info.started_at.elapsed().as_secs(),
        "started_at": info.started_at_unix,
        "control": {
            "socket": info.socket_path,
            "runtime_file": info.runtime_file,
            "token_file": info.token_file,
            "peer_policy": "same-uid-or-root",
            "service_uid": info.service_uid,
            "peer_credentials": cfg!(target_os = "linux"),
        },
        "config": {
            "rows": rows.len(),
            "valid": valid,
            "invalid_keys": invalid_keys,
            "unconfigured_keys": unconfigured_keys,
            "sonarr": snapshot.title_sync_configured(AppKind::Sonarr),
            "radarr": snapshot.title_sync_configured(AppKind::Radarr),
            "prowlarr": snapshot.prowlarr_url.is_some(),
            "jackett": snapshot.jackett_url.is_some(),
            "tmdb": snapshot.tmdb_api_key.is_some(),
            "qbittorrent": snapshot.qbittorrent_configured(),
            "transmission": snapshot.transmission_configured(),
            "login_enabled": snapshot.login_enabled,
        },
        "sync": {
            "enabled": snapshot.sync_enabled,
            "interval_seconds": sync_interval.as_secs(),
            "sonarr_title_gate_remaining_seconds": sonarr_gate,
            "radarr_title_gate_remaining_seconds": radarr_gate,
            "tmdb_gate_remaining_seconds": tmdb_gate,
        },
        "downloaders": {
            "qbittorrent": {
                "configured": snapshot.qbittorrent_configured(),
                "logged_in": qbittorrent_logged_in,
            },
            "transmission": {
                "configured": snapshot.transmission_configured(),
                "logged_in": transmission_logged_in,
            },
        },
        "caches": {
            "result_entries": state.result_cache.len().await,
            "result_max_entries": snapshot.result_cache_max_entries,
            "formatting_entries": state.formatting_cache.len().await,
            "title_entries": state.title_cache.len().await,
            "title_max_entries": snapshot.cache_max_entries,
        },
        "jobs": {
            "total": job_total,
            "running": job_running,
        },
    }))
    .into_response()
}

async fn reload(State(state): State<AppState>) -> Response {
    match state.refresh_config().await {
        Ok(()) => Json(json!({ "status": "ok", "reloaded": true })).into_response(),
        Err(error) => {
            tracing::error!(%error, "control reload failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "status": "error", "error": "configuration reload failed" })),
            )
                .into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
struct SyncQuery {
    kind: String,
}

async fn start_sync(
    State(state): State<AppState>,
    Extension(info): Extension<Arc<ControlInfo>>,
    Query(query): Query<SyncQuery>,
) -> Response {
    let Some(target) = SyncTarget::parse(&query.kind) else {
        return bad_request("kind must be sonarr, radarr, tmdb, rules, or all");
    };
    spawn_command(
        &state,
        &info,
        Command::Sync(target),
        "sync",
        target.as_str(),
    )
}

#[derive(Debug, Deserialize)]
struct RenameQuery {
    kind: String,
    #[serde(default)]
    dry_run: Option<String>,
}

async fn start_rename(
    State(state): State<AppState>,
    Extension(info): Extension<Arc<ControlInfo>>,
    Query(query): Query<RenameQuery>,
) -> Response {
    let Some(kind) = parse_app_kind(&query.kind) else {
        return bad_request("kind must be sonarr or radarr");
    };
    let dry_run = parse_bool(query.dry_run.as_deref()).unwrap_or(false);
    spawn_command(
        &state,
        &info,
        Command::Rename { kind, dry_run },
        "rename",
        &format!("{}{}", kind.as_str(), if dry_run { ":dry-run" } else { "" }),
    )
}

#[derive(Debug, Deserialize)]
struct LoginQuery {
    client: String,
}

async fn start_login(
    State(state): State<AppState>,
    Extension(info): Extension<Arc<ControlInfo>>,
    Query(query): Query<LoginQuery>,
) -> Response {
    let Some(kind) = DownloaderKind::parse(&query.client) else {
        return bad_request("client must be qbittorrent or transmission");
    };
    spawn_command(&state, &info, Command::Login(kind), "login", kind.as_str())
}

#[derive(Debug, Deserialize)]
struct JobCreateRequest {
    action: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    dry_run: Option<bool>,
    #[serde(default)]
    client: Option<String>,
}

async fn start_job(
    State(state): State<AppState>,
    Extension(info): Extension<Arc<ControlInfo>>,
    Json(request): Json<JobCreateRequest>,
) -> Response {
    match request.action.as_str() {
        "sync" => {
            let Some(target) = request.kind.as_deref().and_then(SyncTarget::parse) else {
                return bad_request("kind must be sonarr, radarr, tmdb, rules, or all");
            };
            spawn_command(
                &state,
                &info,
                Command::Sync(target),
                "sync",
                target.as_str(),
            )
        }
        "rename" => {
            let Some(kind) = request.kind.as_deref().and_then(parse_app_kind) else {
                return bad_request("kind must be sonarr or radarr");
            };
            let dry_run = request.dry_run.unwrap_or(false);
            spawn_command(
                &state,
                &info,
                Command::Rename { kind, dry_run },
                "rename",
                &format!("{}{}", kind.as_str(), if dry_run { ":dry-run" } else { "" }),
            )
        }
        "login" => {
            let Some(kind) = request.client.as_deref().and_then(DownloaderKind::parse) else {
                return bad_request("client must be qbittorrent or transmission");
            };
            spawn_command(&state, &info, Command::Login(kind), "login", kind.as_str())
        }
        _ => bad_request("action must be sync, rename, or login"),
    }
}

#[derive(Debug, Deserialize)]
struct JobsQuery {
    #[serde(default)]
    limit: Option<usize>,
}

async fn list_jobs(
    Extension(info): Extension<Arc<ControlInfo>>,
    Query(query): Query<JobsQuery>,
) -> Response {
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    Json(info.jobs.views(limit).await).into_response()
}

async fn show_job(
    Extension(info): Extension<Arc<ControlInfo>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let Some(job) = info.jobs.get(&id) else {
        return not_found();
    };
    Json(job.snapshot(true).await).into_response()
}

async fn cancel_job(
    Extension(info): Extension<Arc<ControlInfo>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let Some(job) = info.jobs.get(&id) else {
        return not_found();
    };
    let accepted = job.request_cancel().await;
    Json(json!({
        "id": id,
        "cancel_requested": accepted,
        "status": job.snapshot(false).await.status,
    }))
    .into_response()
}

async fn job_events(
    Extension(info): Extension<Arc<ControlInfo>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let Some(job) = info.jobs.get(&id) else {
        return not_found();
    };
    let mut revision = job.subscribe();
    let stream = async_stream::stream! {
        let mut after = 0_u64;
        loop {
            let (events, view, done) = job.events_after(after).await;
            for event in events {
                after = event.seq + 1;
                yield Ok::<Event, std::convert::Infallible>(
                    Event::default()
                        .event("log")
                        .id(event.seq.to_string())
                        .data(serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_owned())),
                );
            }
            if done {
                yield Ok(
                    Event::default()
                        .event("done")
                        .data(serde_json::to_string(&view).unwrap_or_else(|_| "{}".to_owned())),
                );
                break;
            }
            if revision.changed().await.is_err() {
                break;
            }
        }
    };
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

fn parse_app_kind(value: &str) -> Option<AppKind> {
    match value {
        "sonarr" => Some(AppKind::Sonarr),
        "radarr" => Some(AppKind::Radarr),
        _ => None,
    }
}

fn parse_bool(value: Option<&str>) -> Option<bool> {
    match value?.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn bad_request(message: &'static str) -> Response {
    (StatusCode::BAD_REQUEST, message).into_response()
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "job not found").into_response()
}
