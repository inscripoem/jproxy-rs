//! Agent and operator subcommands.
//!
//! Online commands are thin HTTP clients for the Unix-domain control channel;
//! they never touch the database behind a running server. `admin` and `config`
//! retain an explicit offline mode for a stopped service (recovery and database
//! inspection), which is announced on stderr.
//!
//! Results go to stdout, progress and logs go to stderr. Credentials are read
//! from stdin or a file and are never accepted in argv, printed, or logged.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sqlx::SqlitePool;

use crate::admin_db;
use crate::auth;
use crate::config::{self, Config, ConfigStore, DEFAULT_CONTROL_SOCKET};
use crate::db;
use crate::models::AppKind;
use crate::{probe, sync};

pub const EXIT_USAGE: i32 = 2;
pub const EXIT_CONNECT: i32 = 3;
pub const EXIT_AUTH: i32 = 4;
pub const EXIT_SERVER: i32 = 5;
pub const EXIT_VERSION: i32 = 6;

#[derive(Debug)]
pub struct CliFailure {
    code: i32,
    message: String,
}

impl std::fmt::Display for CliFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CliFailure {}

fn failure(code: i32, message: impl Into<String>) -> anyhow::Error {
    CliFailure {
        code,
        message: message.into(),
    }
    .into()
}

fn usage(message: impl Into<String>) -> anyhow::Error {
    failure(EXIT_USAGE, message)
}

/// Exit code for a CLI error. Errors without a typed failure use `1`.
pub fn exit_code(error: &anyhow::Error) -> i32 {
    for cause in error.chain() {
        if let Some(failure) = cause.downcast_ref::<CliFailure>() {
            return failure.code;
        }
    }
    1
}

#[derive(Debug, Clone, Default)]
struct Global {
    socket: Option<PathBuf>,
    token_file: Option<PathBuf>,
    runtime_file: Option<PathBuf>,
    json: bool,
    timeout: Duration,
    offline: bool,
}

impl Global {
    fn parse(args: &[String]) -> Result<(Self, Vec<String>)> {
        let mut global = Self {
            timeout: Duration::from_secs(30),
            ..Self::default()
        };
        let mut rest = Vec::new();
        let mut index = 0;
        while index < args.len() {
            match args[index].as_str() {
                "--socket" => {
                    global.socket = Some(PathBuf::from(
                        args.get(index + 1)
                            .ok_or_else(|| usage("--socket requires a path"))?,
                    ));
                    index += 2;
                }
                "--token-file" => {
                    global.token_file = Some(PathBuf::from(
                        args.get(index + 1)
                            .ok_or_else(|| usage("--token-file requires a path"))?,
                    ));
                    index += 2;
                }
                "--runtime-file" => {
                    global.runtime_file = Some(PathBuf::from(
                        args.get(index + 1)
                            .ok_or_else(|| usage("--runtime-file requires a path"))?,
                    ));
                    index += 2;
                }
                "--timeout" => {
                    let value = args
                        .get(index + 1)
                        .ok_or_else(|| usage("--timeout requires seconds"))?;
                    let seconds: u64 = value
                        .parse()
                        .with_context(|| format!("--timeout must be an integer, got {value}"))?;
                    if seconds == 0 {
                        bail!("--timeout must be greater than zero");
                    }
                    global.timeout = Duration::from_secs(seconds);
                    index += 2;
                }
                "--json" => {
                    global.json = true;
                    index += 1;
                }
                "--no-color" => {
                    index += 1;
                }
                "--offline" => {
                    global.offline = true;
                    index += 1;
                }
                other => {
                    rest.push(other.to_owned());
                    index += 1;
                }
            }
        }
        Ok((global, rest))
    }
}

pub async fn run(args: &[String]) -> Result<()> {
    let (global, rest) = Global::parse(args)?;
    let Some(command) = rest.first().map(String::as_str) else {
        return Err(usage("missing subcommand"));
    };
    let tail = &rest[1..];
    match command {
        "status" => status_command(&global, tail).await,
        "healthcheck" => healthcheck_command(&global, tail).await,
        "version" => version_command(&global, tail),
        "config" => config_command(&global, tail).await,
        "rules" => rules_command(&global, tail).await,
        "titles" => titles_command(&global, tail).await,
        "examples" => examples_command(&global, tail).await,
        "tmdb" => tmdb_command(&global, tail).await,
        "cache" => cache_command(&global, tail).await,
        "sync" => sync_command(&global, tail).await,
        "rename" => rename_command(&global, tail).await,
        "login" => login_command(&global, tail).await,
        "admin" => admin_command(&global, tail).await,
        "jobs" => jobs_command(&global, tail).await,
        "doctor" => doctor_command(&global, tail).await,
        other => Err(usage(format!("unknown subcommand: {other}"))),
    }
}

// ---------------------------------------------------------------------------
// Control target and HTTP client
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Target {
    socket: PathBuf,
    token_file: PathBuf,
    version: Option<String>,
    token: String,
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

fn default_runtime_file() -> PathBuf {
    Path::new(DEFAULT_CONTROL_SOCKET)
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("runtime.json")
}

fn resolve_target(global: &Global) -> Result<Option<Target>> {
    if let Some(socket) = global.socket.clone().or_else(|| env_path("CONTROL_SOCKET")) {
        let runtime_file = global
            .runtime_file
            .clone()
            .or_else(|| socket.parent().map(|dir| dir.join("runtime.json")))
            .unwrap_or_else(default_runtime_file);
        let version = read_runtime(&runtime_file).ok().and_then(|runtime| {
            runtime
                .get("version")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
        let token_file = global.token_file.clone().unwrap_or_else(|| {
            socket
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("token")
        });
        if !token_file.exists() && !socket.exists() {
            return Ok(None);
        }
        let token = read_token(&token_file)?;
        return Ok(Some(Target {
            socket,
            token_file,
            version,
            token,
        }));
    }

    let runtime_file = global
        .runtime_file
        .clone()
        .or_else(|| env_path("CONTROL_RUNTIME_FILE"))
        .unwrap_or_else(default_runtime_file);
    if !runtime_file.exists() {
        return Ok(None);
    }
    let runtime = read_runtime(&runtime_file)?;
    let socket = runtime
        .get("socket")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .context("runtime.json has no socket field")?;
    let version = runtime
        .get("version")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let token_file = global
        .token_file
        .clone()
        .or_else(|| {
            runtime
                .get("token_file")
                .and_then(Value::as_str)
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| {
            socket
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("token")
        });
    let token = read_token(&token_file)?;
    Ok(Some(Target {
        socket,
        token_file,
        version,
        token,
    }))
}

fn read_runtime(path: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read control runtime metadata {}", path.display()))?;
    serde_json::from_str(&text)
        .with_context(|| format!("parse control runtime metadata {}", path.display()))
}

fn read_token(path: &Path) -> Result<String> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => {
            return Err(failure(
                EXIT_AUTH,
                format!("cannot read control token file {}: {error}", path.display()),
            ));
        }
    };
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(failure(
            EXIT_AUTH,
            format!(
                "control token file {} is accessible to other users; expected mode 0600",
                path.display()
            ),
        ));
    }
    let token = match std::fs::read_to_string(path) {
        Ok(token) => token,
        Err(error) => {
            return Err(failure(
                EXIT_AUTH,
                format!("cannot read control token file {}: {error}", path.display()),
            ));
        }
    };
    let token = token.trim().to_owned();
    if token.is_empty() {
        return Err(failure(
            EXIT_AUTH,
            format!("control token file {} is empty", path.display()),
        ));
    }
    Ok(token)
}

fn ensure_online(global: &Global) -> Result<Target> {
    let Some(target) = resolve_target(global)? else {
        return Err(failure(
            EXIT_CONNECT,
            format!(
                "control socket not found ({}); start the service or pass --socket/--runtime-file",
                default_runtime_file().display()
            ),
        ));
    };
    if !global.offline
        && let Some(version) = target.version.as_deref()
        && version != env!("CARGO_PKG_VERSION")
    {
        return Err(failure(
            EXIT_VERSION,
            format!(
                "control server version {version} does not match CLI {}",
                env!("CARGO_PKG_VERSION")
            ),
        ));
    }
    Ok(target)
}

fn online_or_offline(global: &Global) -> Result<Option<Target>> {
    if global.offline {
        return Ok(None);
    }
    match resolve_target(global)? {
        Some(target) => Ok(Some(target)),
        None => {
            if let Some(socket) = live_socket(global)
                && std::os::unix::net::UnixStream::connect(&socket).is_ok()
            {
                return Err(failure(
                    EXIT_CONNECT,
                    format!(
                        "control socket {} is live but its token/runtime metadata is \
                         unavailable; pass --runtime-file or --token-file, or --offline \
                         to edit the database explicitly",
                        socket.display()
                    ),
                ));
            }
            eprintln!(
                "control socket not found; using offline database mode (stderr notice, \
                 set CONTROL_SOCKET or pass --socket to use the running service)"
            );
            Ok(None)
        }
    }
}

fn live_socket(global: &Global) -> Option<PathBuf> {
    if let Some(socket) = global.socket.clone().or_else(|| env_path("CONTROL_SOCKET")) {
        return Some(socket);
    }
    let runtime_file = global
        .runtime_file
        .clone()
        .or_else(|| env_path("CONTROL_RUNTIME_FILE"))
        .unwrap_or_else(default_runtime_file);
    if let Some(socket) = read_runtime(&runtime_file).ok().and_then(|runtime| {
        runtime
            .get("socket")
            .and_then(Value::as_str)
            .map(PathBuf::from)
    }) {
        return Some(socket);
    }
    let default_socket = PathBuf::from(DEFAULT_CONTROL_SOCKET);
    default_socket.exists().then_some(default_socket)
}

struct ControlClient {
    http: reqwest::Client,
    socket: PathBuf,
    token: String,
}

impl ControlClient {
    fn new(target: &Target, global: &Global) -> Result<Self> {
        let http = reqwest::Client::builder()
            .unix_socket(target.socket.clone())
            .timeout(global.timeout)
            .build()
            .context("build control HTTP client")?;
        Ok(Self {
            http,
            socket: target.socket.clone(),
            token: target.token.clone(),
        })
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("http://localhost{path}"))
            .header("x-control-token", &self.token)
    }

    async fn get(&self, path: &str) -> Result<reqwest::Response> {
        self.finish(self.request(reqwest::Method::GET, path).send().await)
            .await
    }

    async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let response = self.get(path).await?;
        response.json::<T>().await.map_err(|error| {
            failure(
                EXIT_SERVER,
                format!("parse control response: {}", error.without_url()),
            )
        })
    }

    async fn post_json(&self, path: &str, body: &Value) -> Result<reqwest::Response> {
        self.finish(
            self.request(reqwest::Method::POST, path)
                .json(body)
                .send()
                .await,
        )
        .await
    }

    async fn post_multipart(
        &self,
        path: &str,
        file_name: &str,
        bytes: Vec<u8>,
    ) -> Result<reqwest::Response> {
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(file_name.to_owned())
            .mime_str("application/json")
            .context("build multipart part")?;
        let form = reqwest::multipart::Form::new().part("file", part);
        self.finish(
            self.request(reqwest::Method::POST, path)
                .multipart(form)
                .send()
                .await,
        )
        .await
    }

    async fn finish(
        &self,
        result: std::result::Result<reqwest::Response, reqwest::Error>,
    ) -> Result<reqwest::Response> {
        let response = result.map_err(|error| {
            if error.is_connect() {
                failure(
                    EXIT_CONNECT,
                    format!(
                        "cannot connect to control socket {}: {}",
                        self.socket.display(),
                        error.without_url()
                    ),
                )
            } else if error.is_timeout() {
                failure(EXIT_SERVER, "control request timed out")
            } else {
                failure(
                    EXIT_CONNECT,
                    format!("control request failed: {}", error.without_url()),
                )
            }
        })?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let body = response.text().await.unwrap_or_default();
        let detail = body.trim();
        let message = if detail.is_empty() {
            format!("control request returned HTTP {}", status.as_u16())
        } else {
            format!(
                "control request returned HTTP {}: {detail}",
                status.as_u16()
            )
        };
        let code = match status.as_u16() {
            401 | 403 => EXIT_AUTH,
            400 | 405 | 422 => EXIT_USAGE,
            _ => EXIT_SERVER,
        };
        Err(failure(code, message))
    }

    async fn create_job(&self, path: &str) -> Result<Value> {
        let response = self.post_json(path, &json!({})).await?;
        response.json::<Value>().await.map_err(|error| {
            failure(
                EXIT_SERVER,
                format!("parse job response: {}", error.without_url()),
            )
        })
    }

    /// Stream one job's SSE progress. Returns the final job view.
    async fn watch_job(&self, id: &str, quiet: bool) -> Result<Value> {
        let http = reqwest::Client::builder()
            .unix_socket(self.socket.clone())
            .build()
            .context("build control streaming client")?;
        let response = http
            .get(format!("http://localhost/api/control/jobs/{id}/events"))
            .header("x-control-token", &self.token)
            .send()
            .await
            .map_err(|error| {
                failure(
                    EXIT_CONNECT,
                    format!("watch control job: {}", error.without_url()),
                )
            })?;
        let response = self.finish(Ok(response)).await?;

        let mut stream = response.bytes_stream();
        let mut buffer = String::new();
        let mut final_view = None;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| {
                failure(
                    EXIT_CONNECT,
                    format!("read job events: {}", error.without_url()),
                )
            })?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(index) = buffer.find("\n\n") {
                let frame = buffer[..index].to_owned();
                buffer.drain(..index + 2);
                let (event, data) = parse_sse_frame(&frame);
                let Ok(payload) = serde_json::from_str::<Value>(&data) else {
                    continue;
                };
                match event.as_str() {
                    "log" => {
                        if !quiet {
                            let level = payload
                                .get("level")
                                .and_then(Value::as_str)
                                .unwrap_or("info");
                            let message = payload
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            eprintln!("[{level}] {message}");
                        }
                    }
                    "done" => {
                        final_view = Some(payload);
                        break;
                    }
                    _ => {}
                }
            }
            if final_view.is_some() {
                break;
            }
        }
        if let Some(view) = final_view {
            return Ok(view);
        }
        self.get_json(&format!("/api/control/jobs/{id}")).await
    }
}

fn parse_sse_frame(frame: &str) -> (String, String) {
    let mut event = String::from("message");
    let mut data = String::new();
    for line in frame.lines() {
        if let Some(value) = line.strip_prefix("event:") {
            event = value.trim().to_owned();
        } else if let Some(value) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.trim_start());
        }
    }
    (event, data)
}

fn job_id(value: &Value) -> Result<&str> {
    value
        .get("id")
        .and_then(Value::as_str)
        .context("job response has no id")
}

fn job_status(value: &Value) -> &str {
    value
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
}

fn print_job_result(global: &Global, view: &Value, action: &str) -> Result<()> {
    let status = job_status(view);
    if status != "succeeded" {
        let message = view.get("error").and_then(Value::as_str).unwrap_or(status);
        return Err(failure(
            EXIT_SERVER,
            format!("{action} job {status}: {message}"),
        ));
    }
    if global.json {
        println!("{}", serde_json::to_string_pretty(view)?);
        return Ok(());
    }
    if let Some(result) = view.get("result") {
        if let Some(results) = result.get("results").and_then(Value::as_array) {
            for entry in results {
                let kind = entry.get("kind").and_then(Value::as_str).unwrap_or("?");
                match entry.get("count") {
                    Some(Value::Number(count)) => println!("{kind}: {count}"),
                    _ => println!("{kind}: skipped"),
                }
            }
        } else {
            println!("{}", serde_json::to_string_pretty(result)?);
        }
    } else {
        println!("{action}: {status}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Simple commands
// ---------------------------------------------------------------------------

fn ensure_no_extra(args: &[String]) -> Result<()> {
    if let Some(extra) = args.first() {
        return Err(usage(format!("unexpected argument: {extra}")));
    }
    Ok(())
}

async fn status_command(global: &Global, args: &[String]) -> Result<()> {
    ensure_no_extra(args)?;
    let target = ensure_online(global)?;
    let client = ControlClient::new(&target, global)?;
    let value: Value = client.get_json("/api/control/status").await?;
    if global.json {
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    let field = |path: &[&str]| -> String {
        let mut current = &value;
        for key in path {
            current = current.get(*key).unwrap_or(&Value::Null);
        }
        match current {
            Value::String(text) => text.clone(),
            Value::Null => "-".to_owned(),
            other => other.to_string(),
        }
    };
    println!("version: {}", field(&["version"]));
    println!("pid: {}", field(&["pid"]));
    println!("uptime: {}s", field(&["uptime_seconds"]));
    println!("control socket: {}", field(&["control", "socket"]));
    println!(
        "control peer policy: {}",
        field(&["control", "peer_policy"])
    );
    println!("config rows: {}", field(&["config", "rows"]));
    println!("config valid: {}", field(&["config", "valid"]));
    println!("config invalid: {}", field(&["config", "invalid_keys"]));
    println!("sync enabled: {}", field(&["sync", "enabled"]));
    println!(
        "sonarr title gate: {}s",
        field(&["sync", "sonarr_title_gate_remaining_seconds"])
    );
    println!(
        "radarr title gate: {}s",
        field(&["sync", "radarr_title_gate_remaining_seconds"])
    );
    println!(
        "qbittorrent: configured={} logged_in={}",
        field(&["downloaders", "qbittorrent", "configured"]),
        field(&["downloaders", "qbittorrent", "logged_in"])
    );
    println!(
        "transmission: configured={} logged_in={}",
        field(&["downloaders", "transmission", "configured"]),
        field(&["downloaders", "transmission", "logged_in"])
    );
    println!("result cache: {}", field(&["caches", "result_entries"]));
    println!("title cache: {}", field(&["caches", "title_entries"]));
    println!(
        "jobs: total={} running={}",
        field(&["jobs", "total"]),
        field(&["jobs", "running"])
    );
    Ok(())
}

async fn healthcheck_command(global: &Global, args: &[String]) -> Result<()> {
    ensure_no_extra(args)?;
    let target = ensure_online(global)?;
    let client = ControlClient::new(&target, global)?;
    let response = client.get("/health").await?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(failure(
            EXIT_SERVER,
            format!("health check returned HTTP {}: {body}", status.as_u16()),
        ));
    }
    if global.json {
        let value: Value = serde_json::from_str(&body).unwrap_or_else(|_| json!({ "raw": body }));
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!("{body}");
    }
    Ok(())
}

fn version_command(global: &Global, args: &[String]) -> Result<()> {
    ensure_no_extra(args)?;
    if global.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "version": env!("CARGO_PKG_VERSION"),
                "name": "jproxy-rs",
            }))?
        );
    } else {
        println!("jproxy-rs {}", env!("CARGO_PKG_VERSION"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Argument helper
// ---------------------------------------------------------------------------

struct Parsed {
    flags: HashSet<String>,
    values: HashMap<String, String>,
    positional: Vec<String>,
}

impl Parsed {
    fn parse(args: &[String], value_flags: &[&str]) -> Result<Self> {
        let mut flags = HashSet::new();
        let mut values = HashMap::new();
        let mut positional = Vec::new();
        let mut index = 0;
        while index < args.len() {
            let argument = &args[index];
            if argument.starts_with("--") {
                if value_flags.contains(&argument.as_str()) {
                    let value = args
                        .get(index + 1)
                        .ok_or_else(|| usage(format!("{argument} requires a value")))?
                        .clone();
                    values.insert(argument.clone(), value);
                    index += 2;
                } else {
                    flags.insert(argument.clone());
                    index += 1;
                }
            } else {
                positional.push(argument.clone());
                index += 1;
            }
        }
        Ok(Self {
            flags,
            values,
            positional,
        })
    }

    fn flag(&self, name: &str) -> bool {
        self.flags.contains(name)
    }

    fn value(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    fn ensure_only(&self, flags: &[&str], values: &[&str]) -> Result<()> {
        for flag in &self.flags {
            if !flags.contains(&flag.as_str()) {
                return Err(usage(format!("unexpected argument: {flag}")));
            }
        }
        for value in self.values.keys() {
            if !values.contains(&value.as_str()) {
                return Err(usage(format!("unexpected argument: {value}")));
            }
        }
        Ok(())
    }

    fn require_yes(&self, operation: &str) -> Result<()> {
        if self.flag("--yes") {
            Ok(())
        } else {
            Err(usage(format!(
                "{operation} is destructive; pass --yes to confirm"
            )))
        }
    }
}

fn require_value_file(parsed: &Parsed, label: &str) -> Result<String> {
    let stdin = parsed.flag("--stdin");
    let file = parsed.value("--file");
    if stdin == file.is_some() {
        return Err(usage(format!(
            "{label} requires exactly one of --stdin or --file <path>"
        )));
    }
    match file {
        Some(path) => read_file_value(Path::new(path)),
        None => read_stdin_value(),
    }
}

// ---------------------------------------------------------------------------
// Offline helpers
// ---------------------------------------------------------------------------

async fn open_pool() -> Result<SqlitePool> {
    let path = config::database_path_from_env();
    db::connect(&path)
        .await
        .with_context(|| format!("open database {}", path.display()))
}

fn read_stdin_value() -> Result<String> {
    let mut buffer = String::new();
    std::io::stdin()
        .read_to_string(&mut buffer)
        .context("read credential from stdin")?;
    let value = buffer.trim_end_matches(['\r', '\n']).to_owned();
    if value.is_empty() {
        bail!("stdin is empty");
    }
    Ok(value)
}

fn read_file_value(path: &Path) -> Result<String> {
    let buffer = std::fs::read_to_string(path)
        .with_context(|| format!("read value from {}", path.display()))?;
    let value = buffer.trim_end_matches(['\r', '\n']).to_owned();
    if value.is_empty() {
        bail!("{} is empty", path.display());
    }
    Ok(value)
}

fn masked(value: &str) -> &str {
    if value.is_empty() { "" } else { "********" }
}

// ---------------------------------------------------------------------------
// admin
// ---------------------------------------------------------------------------

async fn admin_command(global: &Global, args: &[String]) -> Result<()> {
    if !global.offline
        && let Some(socket) = live_socket(global)
        && std::os::unix::net::UnixStream::connect(&socket).is_ok()
    {
        return Err(usage(format!(
            "control socket {} is live; stop the service or pass --offline to edit \
             the database directly",
            socket.display()
        )));
    }
    if !global.offline {
        eprintln!("offline database mode: admin recovery commands edit DATABASE_PATH directly");
    }
    match args.first().map(String::as_str) {
        Some("set-password") => admin_set_password(&args[1..]).await,
        Some("reset-password") => admin_reset_password(&args[1..]).await,
        Some(other) => Err(usage(format!("unknown admin command: {other}"))),
        None => Err(usage(
            "usage: jproxy-rs admin set-password|reset-password --stdin",
        )),
    }
}

async fn admin_set_password(args: &[String]) -> Result<()> {
    let mut username = None;
    let mut stdin = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--username" => {
                username = Some(
                    args.get(index + 1)
                        .ok_or_else(|| usage("--username requires a value"))?
                        .clone(),
                );
                index += 2;
            }
            "--stdin" => {
                stdin = true;
                index += 1;
            }
            other => return Err(usage(format!("unexpected argument: {other}"))),
        }
    }
    if !stdin {
        return Err(usage("admin set-password requires --stdin"));
    }
    let explicit_username = username
        .or_else(|| std::env::var("ADMIN_USERNAME").ok())
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let password = read_stdin_value()?;
    let hash = auth::stored_password_hash(&password);
    let pool = open_pool().await?;
    // With no explicit target, follow the account that already exists. This
    // keeps a migrated `inscripoem` account authoritative instead of adding a
    // second `jproxy` administrator.
    let username = match explicit_username {
        Some(username) => username,
        None => match admin_db::list_usernames(&pool).await?.as_slice() {
            [username] => username.clone(),
            _ => auth::DEFAULT_USERNAME.to_owned(),
        },
    };
    admin_db::set_user_password(&pool, &username, &hash).await?;
    println!("admin user {username}: password updated");
    Ok(())
}

async fn admin_reset_password(args: &[String]) -> Result<()> {
    let mut stdin = false;
    for argument in args {
        match argument.as_str() {
            "--stdin" => stdin = true,
            other => return Err(usage(format!("unexpected argument: {other}"))),
        }
    }
    if !stdin {
        return Err(usage("admin reset-password requires --stdin"));
    }
    let password = read_stdin_value()?;
    let hash = auth::stored_password_hash(&password);
    let pool = open_pool().await?;
    let username = admin_db::reset_single_user_password(&pool, &hash).await?;
    println!("admin user {username}: password reset");
    Ok(())
}

// ---------------------------------------------------------------------------
// config
// ---------------------------------------------------------------------------

async fn config_command(global: &Global, args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("get") => config_get(global, &args[1..]).await,
        Some("set") => config_set(global, &args[1..]).await,
        Some("list") => config_list(global, &args[1..]).await,
        Some(other) => Err(usage(format!("unknown config command: {other}"))),
        None => Err(usage("usage: jproxy-rs config get|set|list ...")),
    }
}

async fn config_get(global: &Global, args: &[String]) -> Result<()> {
    let parsed = Parsed::parse(args, &[])?;
    parsed.ensure_only(&["--reveal"], &[])?;
    let key = parsed
        .positional
        .first()
        .ok_or_else(|| usage("config get requires a key"))?
        .clone();
    if parsed.positional.len() > 1 {
        return Err(usage(format!(
            "unexpected argument: {}",
            parsed.positional[1]
        )));
    }
    if let Some(target) = online_or_offline(global)? {
        let client = ControlClient::new(&target, global)?;
        let rows: Vec<Value> = client.get_json("/api/system/config/query").await?;
        let row = rows
            .iter()
            .find(|row| row.get("key").and_then(Value::as_str) == Some(key.as_str()))
            .with_context(|| format!("system_config key not found: {key}"))?;
        let value = row.get("value").and_then(Value::as_str).unwrap_or_default();
        if !parsed.flag("--reveal") && config::is_secret_system_config_key(&key) {
            println!("{}", masked(value));
        } else {
            println!("{value}");
        }
        return Ok(());
    }

    let pool = open_pool().await?;
    let row = admin_db::get_system_config(&pool, &key)
        .await?
        .with_context(|| format!("system_config key not found: {key}"))?;
    let value = row.value.unwrap_or_default();
    if !parsed.flag("--reveal") && config::is_secret_system_config_key(&key) {
        println!("{}", masked(&value));
    } else {
        println!("{value}");
    }
    Ok(())
}

async fn config_set(global: &Global, args: &[String]) -> Result<()> {
    let parsed = Parsed::parse(args, &["--file"])?;
    parsed.ensure_only(&["--stdin"], &["--file"])?;
    let key = parsed
        .positional
        .first()
        .ok_or_else(|| usage("config set requires a key"))?
        .clone();
    if parsed.positional.len() > 1 {
        return Err(usage(format!(
            "unexpected argument: {}",
            parsed.positional[1]
        )));
    }
    if !Config::system_config_keys().contains(&key.as_str()) {
        return Err(usage(format!("unknown system_config key: {key}")));
    }
    let value = require_value_file(&parsed, "config set")?;

    if let Some(target) = online_or_offline(global)? {
        let client = ControlClient::new(&target, global)?;
        let mut rows: Vec<Value> = client.get_json("/api/system/config/query").await?;
        let row = rows
            .iter_mut()
            .find(|row| row.get("key").and_then(Value::as_str) == Some(key.as_str()))
            .with_context(|| format!("system_config key not found: {key}"))?;
        row["value"] = Value::String(value);
        row["validStatus"] = Value::from(1);
        client
            .post_json("/api/system/config/update", &Value::Array(rows))
            .await?;
        println!("system_config {key}: updated");
        return Ok(());
    }

    let pool = open_pool().await?;
    admin_db::set_system_config_value(&pool, &key, &value).await?;
    println!("system_config {key}: updated");
    Ok(())
}

async fn config_list(global: &Global, args: &[String]) -> Result<()> {
    let parsed = Parsed::parse(args, &[])?;
    parsed.ensure_only(&["--reveal"], &[])?;
    if let Some(target) = online_or_offline(global)? {
        let client = ControlClient::new(&target, global)?;
        let rows: Vec<Value> = client.get_json("/api/system/config/query").await?;
        if global.json {
            println!("{}", serde_json::to_string_pretty(&rows)?);
            return Ok(());
        }
        for row in rows {
            let key = row.get("key").and_then(Value::as_str).unwrap_or_default();
            let value = row.get("value").and_then(Value::as_str).unwrap_or_default();
            if !parsed.flag("--reveal") && config::is_secret_system_config_key(key) {
                println!("{key}={}", masked(value));
            } else {
                println!("{key}={value}");
            }
        }
        return Ok(());
    }

    let pool = open_pool().await?;
    let rows = admin_db::query_system_config(&pool).await?;
    if global.json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    for row in rows {
        let key = row.key.unwrap_or_default();
        let value = row.value.unwrap_or_default();
        if !parsed.flag("--reveal") && config::is_secret_system_config_key(&key) {
            println!("{key}={}", masked(&value));
        } else {
            println!("{key}={value}");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Rules, titles, examples, TMDB
// ---------------------------------------------------------------------------

fn app_kind(value: &str) -> Result<AppKind> {
    match value {
        "sonarr" => Ok(AppKind::Sonarr),
        "radarr" => Ok(AppKind::Radarr),
        other => Err(usage(format!("expected sonarr or radarr, got {other}"))),
    }
}

fn print_json(value: &Value, _global: &Global) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

async fn rules_command(global: &Global, args: &[String]) -> Result<()> {
    let kind = args
        .first()
        .map(String::as_str)
        .context("usage: jproxy-rs rules sonarr|radarr ...")?;
    let kind = app_kind(kind)?;
    let action = args
        .get(1)
        .map(String::as_str)
        .ok_or_else(|| usage("rules requires an action"))?;
    let base = format!("/api/{}/rule", kind.as_str());
    let client = ControlClient::new(&ensure_online(global)?, global)?;

    match action {
        "list" => {
            let parsed = Parsed::parse(&args[2..], &["--current", "--page-size"])?;
            parsed.ensure_only(&[], &["--current", "--page-size"])?;
            let current = parsed.value("--current").unwrap_or("1");
            let page_size = parsed.value("--page-size").unwrap_or("1000");
            let page: Value = client
                .get_json(&format!(
                    "{base}/query?current={current}&pageSize={page_size}"
                ))
                .await?;
            if global.json {
                println!("{}", serde_json::to_string_pretty(&page)?);
            } else {
                for rule in page
                    .get("list")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    println!(
                        "{}\t{}\t{}\t{}\t{} -> {}\t{}",
                        rule.get("id").and_then(Value::as_str).unwrap_or("-"),
                        rule.get("priority")
                            .and_then(Value::as_i64)
                            .unwrap_or_default(),
                        rule.get("validStatus")
                            .and_then(Value::as_i64)
                            .unwrap_or_default(),
                        rule.get("token").and_then(Value::as_str).unwrap_or(""),
                        rule.get("regex").and_then(Value::as_str).unwrap_or(""),
                        rule.get("replacement")
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                        rule.get("author").and_then(Value::as_str).unwrap_or(""),
                    );
                }
                println!(
                    "total: {}",
                    page.get("total")
                        .and_then(Value::as_i64)
                        .unwrap_or_default()
                );
            }
        }
        "get" => {
            let parsed = Parsed::parse(&args[2..], &[])?;
            parsed.ensure_only(&[], &[])?;
            let id = parsed
                .positional
                .first()
                .ok_or_else(|| usage("rules get requires an id"))?;
            let rules: Vec<Value> = serde_json::from_value(
                client
                    .json_body(&format!("{base}/export"), &json!([id]))
                    .await?,
            )
            .context("parse exported rule")?;
            let rule = rules
                .first()
                .with_context(|| format!("rule not found: {id}"))?;
            print_json(rule, global)?;
        }
        "add" | "update" => {
            let parsed = Parsed::parse(&args[2..], &["--file"])?;
            parsed.ensure_only(&["--stdin"], &["--file"])?;
            let content = require_value_file(&parsed, action)?;
            let mut rule: Value =
                serde_json::from_str(&content).context("rule input must be a JSON object")?;
            if !rule.is_object() {
                return Err(usage("rule input must be a JSON object"));
            }
            if action == "update" {
                let id = parsed
                    .positional
                    .first()
                    .ok_or_else(|| usage("rules update requires an id"))?;
                rule["id"] = Value::String(id.clone());
            } else if let Some(id) = parsed.positional.first() {
                rule["id"] = Value::String(id.clone());
            }
            client.post_json(&format!("{base}/save"), &rule).await?;
            println!("rule saved");
        }
        "remove" | "enable" | "disable" => {
            let parsed = Parsed::parse(&args[2..], &[])?;
            parsed.ensure_only(&["--yes"], &[])?;
            if action == "remove" {
                parsed.require_yes("rules remove")?;
            }
            if parsed.positional.is_empty() {
                return Err(usage(format!("rules {action} requires at least one id")));
            }
            client
                .post_json(&format!("{base}/{action}"), &json!(parsed.positional))
                .await?;
            println!("{}: {}", action, parsed.positional.join(", "));
        }
        "export" => {
            let parsed = Parsed::parse(&args[2..], &["--file"])?;
            parsed.ensure_only(&[], &["--file"])?;
            let rules = client
                .json_body(&format!("{base}/export"), &json!(parsed.positional))
                .await?;
            write_output(
                parsed.value("--file"),
                &serde_json::to_string_pretty(&rules)?,
            )?;
        }
        "import" => {
            let parsed = Parsed::parse(&args[2..], &["--file"])?;
            parsed.ensure_only(&[], &["--file"])?;
            let path = parsed
                .value("--file")
                .ok_or_else(|| usage("rules import requires --file <path>"))?;
            let bytes = std::fs::read(path).with_context(|| format!("read import file {path}"))?;
            let file_name = Path::new(path)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("rules.json");
            client
                .post_multipart(&format!("{base}/import"), file_name, bytes)
                .await?;
            println!("rules imported");
        }
        other => return Err(usage(format!("unknown rules command: {other}"))),
    }
    Ok(())
}

impl ControlClient {
    async fn json_body(&self, path: &str, body: &Value) -> Result<Value> {
        let response = self.post_json(path, body).await?;
        response.json::<Value>().await.map_err(|error| {
            failure(
                EXIT_SERVER,
                format!("parse control response: {}", error.without_url()),
            )
        })
    }
}

fn write_output(path: Option<&str>, contents: &str) -> Result<()> {
    match path {
        Some(path) => {
            std::fs::write(path, contents).with_context(|| format!("write {path}"))?;
            println!("wrote {path}");
        }
        None => println!("{contents}"),
    }
    Ok(())
}

async fn titles_command(global: &Global, args: &[String]) -> Result<()> {
    let kind = args
        .first()
        .map(String::as_str)
        .context("usage: jproxy-rs titles sonarr|radarr ...")?;
    let kind = app_kind(kind)?;
    let action = args
        .get(1)
        .map(String::as_str)
        .ok_or_else(|| usage("titles requires an action"))?;
    let base = format!("/api/{}/title", kind.as_str());
    let client = ControlClient::new(&ensure_online(global)?, global)?;
    match action {
        "list" => {
            let parsed = Parsed::parse(&args[2..], &["--current", "--page-size"])?;
            parsed.ensure_only(&[], &["--current", "--page-size"])?;
            let current = parsed.value("--current").unwrap_or("1");
            let page_size = parsed.value("--page-size").unwrap_or("1000");
            let page: Value = client
                .get_json(&format!(
                    "{base}/query?current={current}&pageSize={page_size}"
                ))
                .await?;
            if global.json {
                println!("{}", serde_json::to_string_pretty(&page)?);
            } else {
                for title in page
                    .get("list")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    println!(
                        "{}\t{}\t{}\t{}\t{}",
                        title.get("id").and_then(Value::as_i64).unwrap_or_default(),
                        title.get("title").and_then(Value::as_str).unwrap_or(""),
                        title
                            .get("seasonNumber")
                            .and_then(Value::as_i64)
                            .unwrap_or_default(),
                        title
                            .get("year")
                            .and_then(Value::as_i64)
                            .unwrap_or_default(),
                        title
                            .get("monitored")
                            .and_then(Value::as_bool)
                            .unwrap_or_default(),
                    );
                }
                println!(
                    "total: {}",
                    page.get("total")
                        .and_then(Value::as_i64)
                        .unwrap_or_default()
                );
            }
        }
        "remove" => {
            let parsed = Parsed::parse(&args[2..], &[])?;
            parsed.ensure_only(&["--yes"], &[])?;
            parsed.require_yes("titles remove")?;
            let ids: Vec<i64> = parsed
                .positional
                .iter()
                .map(|id| id.parse::<i64>())
                .collect::<std::result::Result<_, _>>()
                .map_err(|_| usage("titles remove requires numeric ids"))?;
            if ids.is_empty() {
                return Err(usage("titles remove requires at least one id"));
            }
            client
                .post_json(&format!("{base}/remove"), &json!(ids))
                .await?;
            println!("titles removed");
        }
        "sync" => {
            ensure_no_extra(&args[2..])?;
            start_and_wait(
                global,
                &client,
                &format!("/api/control/sync?kind={}", kind.as_str()),
                "sync",
                false,
            )
            .await?;
        }
        other => return Err(usage(format!("unknown titles command: {other}"))),
    }
    Ok(())
}

async fn examples_command(global: &Global, args: &[String]) -> Result<()> {
    let kind = args
        .first()
        .map(String::as_str)
        .context("usage: jproxy-rs examples sonarr|radarr ...")?;
    let kind = app_kind(kind)?;
    let action = args
        .get(1)
        .map(String::as_str)
        .ok_or_else(|| usage("examples requires an action"))?;
    let base = format!("/api/{}/example", kind.as_str());
    let client = ControlClient::new(&ensure_online(global)?, global)?;
    match action {
        "list" => {
            let parsed = Parsed::parse(&args[2..], &["--current", "--page-size"])?;
            parsed.ensure_only(&[], &["--current", "--page-size"])?;
            let current = parsed.value("--current").unwrap_or("1");
            let page_size = parsed.value("--page-size").unwrap_or("1000");
            let page: Value = client
                .get_json(&format!(
                    "{base}/query?current={current}&pageSize={page_size}"
                ))
                .await?;
            if global.json {
                println!("{}", serde_json::to_string_pretty(&page)?);
            } else {
                for example in page
                    .get("list")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    println!(
                        "{}\t{}\t{}",
                        example
                            .get("hash")
                            .or_else(|| example.get("id"))
                            .and_then(Value::as_str)
                            .unwrap_or("-"),
                        example
                            .get("originalText")
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                        example
                            .get("formatText")
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                    );
                }
                println!(
                    "total: {}",
                    page.get("total")
                        .and_then(Value::as_i64)
                        .unwrap_or_default()
                );
            }
        }
        "add" => {
            let parsed = Parsed::parse(&args[2..], &["--file"])?;
            parsed.ensure_only(&["--stdin"], &["--file"])?;
            let original_text = require_value_file(&parsed, "examples add")?;
            client
                .post_json(
                    &format!("{base}/save"),
                    &json!({ "originalText": original_text }),
                )
                .await?;
            println!("example saved");
        }
        "remove" => {
            let parsed = Parsed::parse(&args[2..], &[])?;
            parsed.ensure_only(&["--yes"], &[])?;
            parsed.require_yes("examples remove")?;
            if parsed.positional.is_empty() {
                return Err(usage("examples remove requires at least one hash"));
            }
            client
                .post_json(&format!("{base}/remove"), &json!(parsed.positional))
                .await?;
            println!("examples removed");
        }
        other => return Err(usage(format!("unknown examples command: {other}"))),
    }
    Ok(())
}

async fn tmdb_command(global: &Global, args: &[String]) -> Result<()> {
    let action = args
        .first()
        .map(String::as_str)
        .context("usage: jproxy-rs tmdb list|add|remove|sync ...")?;
    let client = ControlClient::new(&ensure_online(global)?, global)?;
    match action {
        "list" => {
            let parsed = Parsed::parse(&args[1..], &["--current", "--page-size"])?;
            parsed.ensure_only(&[], &["--current", "--page-size"])?;
            let current = parsed.value("--current").unwrap_or("1");
            let page_size = parsed.value("--page-size").unwrap_or("1000");
            let page: Value = client
                .get_json(&format!(
                    "/api/tmdb/title/query?current={current}&pageSize={page_size}"
                ))
                .await?;
            if global.json {
                println!("{}", serde_json::to_string_pretty(&page)?);
            } else {
                for title in page
                    .get("list")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    println!(
                        "{}\t{}\t{}",
                        title
                            .get("tmdbId")
                            .and_then(Value::as_i64)
                            .unwrap_or_default(),
                        title
                            .get("tvdbId")
                            .and_then(Value::as_i64)
                            .unwrap_or_default(),
                        title.get("title").and_then(Value::as_str).unwrap_or(""),
                    );
                }
                println!(
                    "total: {}",
                    page.get("total")
                        .and_then(Value::as_i64)
                        .unwrap_or_default()
                );
            }
        }
        "add" => {
            let parsed = Parsed::parse(&args[1..], &["--file"])?;
            parsed.ensure_only(&["--stdin"], &["--file"])?;
            let content = require_value_file(&parsed, "tmdb add")?;
            let title: Value =
                serde_json::from_str(&content).context("tmdb add input must be a JSON object")?;
            client.post_json("/api/tmdb/title/save", &title).await?;
            println!("tmdb title saved");
        }
        "remove" => {
            let parsed = Parsed::parse(&args[1..], &[])?;
            parsed.ensure_only(&["--yes"], &[])?;
            parsed.require_yes("tmdb remove")?;
            let ids: Vec<i64> = parsed
                .positional
                .iter()
                .map(|id| id.parse::<i64>())
                .collect::<std::result::Result<_, _>>()
                .map_err(|_| usage("tmdb remove requires numeric ids"))?;
            if ids.is_empty() {
                return Err(usage("tmdb remove requires at least one id"));
            }
            client
                .post_json("/api/tmdb/title/remove", &json!(ids))
                .await?;
            println!("tmdb titles removed");
        }
        "sync" => {
            ensure_no_extra(&args[1..])?;
            start_and_wait(
                global,
                &client,
                "/api/control/sync?kind=tmdb",
                "sync",
                false,
            )
            .await?;
        }
        other => return Err(usage(format!("unknown tmdb command: {other}"))),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Cache, jobs, and long-running commands
// ---------------------------------------------------------------------------

async fn cache_command(global: &Global, args: &[String]) -> Result<()> {
    let action = args
        .first()
        .map(String::as_str)
        .context("usage: jproxy-rs cache list|clear [name]")?;
    let client = ControlClient::new(&ensure_online(global)?, global)?;
    match action {
        "list" => {
            ensure_no_extra(&args[1..])?;
            let status: Value = client.get_json("/api/control/status").await?;
            let caches = status.get("caches").cloned().unwrap_or(Value::Null);
            if global.json {
                println!("{}", serde_json::to_string_pretty(&caches)?);
            } else {
                println!("{caches}");
            }
        }
        "clear" => {
            let parsed = Parsed::parse(&args[1..], &[])?;
            parsed.ensure_only(&["--yes"], &[])?;
            parsed.require_yes("cache clear")?;
            if parsed.positional.len() > 1 {
                return Err(usage("cache clear accepts at most one cache name"));
            }
            match parsed.positional.first() {
                Some(name) => {
                    client
                        .post_json("/api/system/cache/clear", &json!({ "cacheName": name }))
                        .await?;
                    println!("cache {name}: cleared");
                }
                None => {
                    client
                        .post_json("/api/system/cache/clearAll", &json!({}))
                        .await?;
                    println!("all caches cleared");
                }
            }
        }
        other => return Err(usage(format!("unknown cache command: {other}"))),
    }
    Ok(())
}

async fn sync_command(global: &Global, args: &[String]) -> Result<()> {
    let parsed = Parsed::parse(args, &["--kind"])?;
    parsed.ensure_only(&["--no-wait"], &["--kind"])?;
    let kind = parsed
        .value("--kind")
        .ok_or_else(|| usage("sync requires --kind sonarr|radarr|tmdb|rules|all"))?;
    if !matches!(kind, "sonarr" | "radarr" | "tmdb" | "rules" | "all") {
        return Err(usage(
            "--kind must be one of sonarr, radarr, tmdb, rules, all",
        ));
    }

    if let Some(target) = online_or_offline(global)? {
        let client = ControlClient::new(&target, global)?;
        start_and_wait(
            global,
            &client,
            &format!("/api/control/sync?kind={kind}"),
            "sync",
            parsed.flag("--no-wait"),
        )
        .await?;
        return Ok(());
    }
    offline_sync(kind).await
}

async fn rename_command(global: &Global, args: &[String]) -> Result<()> {
    let parsed = Parsed::parse(args, &["--kind"])?;
    parsed.ensure_only(&["--dry-run", "--no-wait"], &["--kind"])?;
    let kind = parsed
        .value("--kind")
        .ok_or_else(|| usage("rename requires --kind sonarr|radarr"))?;
    if !matches!(kind, "sonarr" | "radarr") {
        return Err(usage("--kind must be sonarr or radarr"));
    }
    let dry_run = parsed.flag("--dry-run");
    let target = ensure_online(global)?;
    let client = ControlClient::new(&target, global)?;
    start_and_wait(
        global,
        &client,
        &format!(
            "/api/control/rename?kind={kind}{}",
            if dry_run { "&dry_run=true" } else { "" }
        ),
        "rename",
        parsed.flag("--no-wait"),
    )
    .await
}

async fn login_command(global: &Global, args: &[String]) -> Result<()> {
    let parsed = Parsed::parse(args, &["--client"])?;
    parsed.ensure_only(&["--no-wait"], &["--client"])?;
    let client_kind = parsed
        .value("--client")
        .ok_or_else(|| usage("login requires --client qbittorrent|transmission"))?;
    if !matches!(client_kind, "qbittorrent" | "transmission") {
        return Err(usage("--client must be qbittorrent or transmission"));
    }
    let target = ensure_online(global)?;
    let client = ControlClient::new(&target, global)?;
    start_and_wait(
        global,
        &client,
        &format!("/api/control/login?client={client_kind}"),
        "login",
        parsed.flag("--no-wait"),
    )
    .await
}

async fn start_and_wait(
    global: &Global,
    client: &ControlClient,
    path: &str,
    action: &str,
    no_wait: bool,
) -> Result<()> {
    let accepted = client.create_job(path).await?;
    let id = job_id(&accepted)?.to_owned();
    if no_wait {
        if global.json {
            println!("{}", serde_json::to_string_pretty(&accepted)?);
        } else {
            println!("{action}: queued job {id}");
        }
        return Ok(());
    }
    let view = client.watch_job(&id, false).await?;
    print_job_result(global, &view, action)
}

async fn jobs_command(global: &Global, args: &[String]) -> Result<()> {
    let action = args
        .first()
        .map(String::as_str)
        .context("usage: jproxy-rs jobs list|show|watch|cancel ...")?;
    let client = ControlClient::new(&ensure_online(global)?, global)?;
    match action {
        "list" => {
            let parsed = Parsed::parse(&args[1..], &["--limit"])?;
            parsed.ensure_only(&[], &["--limit"])?;
            let limit = parsed.value("--limit").unwrap_or("50");
            let jobs: Value = client
                .get_json(&format!("/api/control/jobs?limit={limit}"))
                .await?;
            if global.json {
                println!("{}", serde_json::to_string_pretty(&jobs)?);
            } else {
                for job in jobs.as_array().into_iter().flatten() {
                    println!(
                        "{}\t{}\t{}\t{}\t{}",
                        job.get("id").and_then(Value::as_str).unwrap_or("-"),
                        job.get("kind").and_then(Value::as_str).unwrap_or("-"),
                        job.get("target").and_then(Value::as_str).unwrap_or("-"),
                        job.get("status").and_then(Value::as_str).unwrap_or("-"),
                        job.get("created_at").and_then(Value::as_str).unwrap_or("-"),
                    );
                }
                println!(
                    "total: {}",
                    jobs.as_array().map(Vec::len).unwrap_or_default()
                );
            }
        }
        "show" => {
            let id = args
                .get(1)
                .ok_or_else(|| usage("jobs show requires an id"))?;
            ensure_no_extra(&args[2..])?;
            let view: Value = client.get_json(&format!("/api/control/jobs/{id}")).await?;
            println!("{}", serde_json::to_string_pretty(&view)?);
        }
        "watch" => {
            let id = args
                .get(1)
                .ok_or_else(|| usage("jobs watch requires an id"))?;
            ensure_no_extra(&args[2..])?;
            let view = client.watch_job(id, false).await?;
            print_job_result(global, &view, "watch")?;
        }
        "cancel" => {
            let id = args
                .get(1)
                .ok_or_else(|| usage("jobs cancel requires an id"))?;
            ensure_no_extra(&args[2..])?;
            let result = client
                .post_json(&format!("/api/control/jobs/{id}/cancel"), &json!({}))
                .await?
                .json::<Value>()
                .await
                .context("parse cancel response")?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        other => return Err(usage(format!("unknown jobs command: {other}"))),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Offline sync and doctor
// ---------------------------------------------------------------------------

async fn offline_sync(kind: &str) -> Result<()> {
    let mut config = Config::from_env()?;
    // A manual pass is an explicit request; it should not be suppressed by
    // SYNC_ENABLED=false or a zero interval intended for the daemon.
    config.sync_enabled = true;
    if config.sync_interval.is_zero() {
        config.sync_interval = Duration::from_secs(60);
    }
    let state = crate::app::AppState::new(config).await?;

    let kinds: Vec<&str> = match kind {
        "all" => vec!["sonarr", "radarr", "tmdb", "rules"],
        other => vec![other],
    };
    for requested in kinds {
        match requested {
            "sonarr" => {
                let count = sync::sync_titles_once(&state, AppKind::Sonarr)
                    .await
                    .context("sync Sonarr titles")?;
                report_sync("sonarr", count);
            }
            "radarr" => {
                let count = sync::sync_titles_once(&state, AppKind::Radarr)
                    .await
                    .context("sync Radarr titles")?;
                report_sync("radarr", count);
            }
            "tmdb" => {
                let count = sync::sync_tmdb_once(&state)
                    .await
                    .context("sync TMDB titles")?;
                report_sync("tmdb", count);
            }
            "rules" => {
                for app in [AppKind::Sonarr, AppKind::Radarr] {
                    let count = sync::sync_rules_once(&state, app)
                        .await
                        .with_context(|| format!("sync {app} rules"))?;
                    report_sync(&format!("{app} rules"), count);
                }
            }
            _ => unreachable!("validated --kind"),
        }
    }
    Ok(())
}

fn report_sync(label: &str, count: Option<usize>) {
    match count {
        Some(count) => println!("{label}: {count}"),
        None => println!("{label}: skipped (sync disabled or not configured)"),
    }
}

async fn doctor_command(global: &Global, args: &[String]) -> Result<()> {
    ensure_no_extra(args)?;
    match online_or_offline(global)? {
        Some(target) => online_doctor(global, &target).await,
        None => offline_doctor().await,
    }
}

async fn online_doctor(global: &Global, target: &Target) -> Result<()> {
    let client = ControlClient::new(target, global)?;
    let status: Value = client.get_json("/api/control/status").await?;
    let health = client.get("/health").await?;
    let health_ok = health.status().is_success();
    let health_body: Value = health
        .json()
        .await
        .unwrap_or_else(|_| json!({ "status": "unknown" }));
    println!(
        "server: {}",
        status
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
    );
    println!(
        "database: {}",
        health_body
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
    );
    println!(
        "control: socket={} peer_policy={}",
        target.socket.display(),
        status
            .get("control")
            .and_then(|control| control.get("peer_policy"))
            .and_then(Value::as_str)
            .unwrap_or("unknown")
    );
    println!("token file: {} (mode 0600)", target.token_file.display());
    let invalid = status
        .get("config")
        .and_then(|config| config.get("invalid_keys"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if invalid.is_empty() {
        println!("config: ok");
    } else {
        println!(
            "config: invalid rows: {}",
            invalid
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if !health_ok {
        return Err(failure(EXIT_SERVER, "health check is degraded"));
    }
    if !invalid.is_empty() {
        return Err(failure(
            EXIT_SERVER,
            "doctor found invalid configuration rows",
        ));
    }
    Ok(())
}

async fn offline_doctor() -> Result<()> {
    let config = Config::from_env()?;
    let store = ConfigStore::new(config.clone())?;
    let path = config.database_path.clone();
    let pool = db::connect(&path)
        .await
        .with_context(|| format!("open database {}", path.display()))?;

    db::healthcheck(&pool).await?;
    println!("database: ok");

    let rows = db::load_system_config_rows(&pool).await?;
    let invalid: Vec<&str> = rows
        .iter()
        .filter(|row| row.valid_status != 1)
        .filter(|row| {
            row.value
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
        })
        .map(|row| row.key.as_str())
        .collect();
    store.apply_rows(&rows);
    let snapshot = store.snapshot();

    let mut problems = 0;
    if invalid.is_empty() {
        println!("config: ok");
    } else {
        println!("config: invalid rows: {}", invalid.join(", "));
        problems += 1;
    }
    for (name, ok) in [
        (
            "sonarrIndexerFormat",
            probe::check_sonarr_indexer_format(Some(&snapshot.sonarr_indexer_format)),
        ),
        (
            "radarrIndexerFormat",
            probe::check_radarr_indexer_format(Some(&snapshot.radarr_indexer_format)),
        ),
        (
            "cleanTitleRegex",
            snapshot
                .query_value_by_key("cleanTitleRegex")
                .is_ok_and(|value| probe::check_regex(Some(value))),
        ),
    ] {
        if ok {
            println!("config {name}: ok");
        } else {
            println!("config {name}: invalid");
            problems += 1;
        }
    }

    let prober =
        probe::Prober::with_timeouts(snapshot.probe_connect_timeout, snapshot.probe_read_timeout)?;
    let mut checks = 0;

    if let Some(url) = snapshot
        .prowlarr_url
        .as_deref()
        .filter(|url| !url.trim().is_empty())
    {
        checks += 1;
        if prober.check_url(Some(url)).await {
            println!("upstream prowlarr: ok");
        } else {
            println!("upstream prowlarr: unreachable");
            problems += 1;
        }
    }
    if let Some(url) = snapshot
        .jackett_url
        .as_deref()
        .filter(|url| !url.trim().is_empty())
    {
        checks += 1;
        if prober.check_url(Some(url)).await {
            println!("upstream jackett: ok");
        } else {
            println!("upstream jackett: unreachable");
            problems += 1;
        }
    }
    for (name, url, key) in [
        (
            "sonarr",
            snapshot.sonarr_url.as_deref(),
            snapshot.sonarr_api_key.as_deref(),
        ),
        (
            "radarr",
            snapshot.radarr_url.as_deref(),
            snapshot.radarr_api_key.as_deref(),
        ),
    ] {
        if let (Some(url), Some(key)) = (url, key) {
            checks += 1;
            if prober
                .check_sonarr_or_radarr(Some(url), Some(key))
                .await
                .url_valid()
            {
                println!("upstream {name}: ok");
            } else {
                println!("upstream {name}: unreachable or rejected");
                problems += 1;
            }
        }
    }
    if let Some(key) = snapshot.tmdb_api_key.as_deref() {
        checks += 1;
        if prober
            .check_tmdb(snapshot.tmdb_url.as_deref(), Some(key))
            .await
            .url_valid()
        {
            println!("upstream tmdb: ok");
        } else {
            println!("upstream tmdb: unreachable or rejected");
            problems += 1;
        }
    }
    if checks == 0 {
        println!("upstream: not configured");
    }

    if problems > 0 {
        bail!("doctor found {problems} problem(s)");
    }
    Ok(())
}
