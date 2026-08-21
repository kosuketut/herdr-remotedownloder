use std::collections::HashMap;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use flate2::write::GzEncoder;
use flate2::Compression;
use regex::Regex;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub const PLUGIN_ID: &str = "kosukeyano.remote-download";
pub const DEFAULT_PORT: u16 = 18_340;
pub const DEFAULT_MAX_BYTES: u64 = 512 * 1024 * 1024;
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 300;
pub const PREFLIGHT_TIMEOUT_SECONDS: u64 = 3;
pub const TRANSFER_PATH: &str = "/v1/files";
pub const CHOOSE_FILE_PATH: &str = "/v1/choose-file";
pub const LAUNCHD_LABEL: &str = "com.kosukeyano.herdr-remote-download";
pub const DOWNLOAD_ACTION: &str = "kosukeyano.remote-download.download";
pub const PICK_ACTION: &str = "kosukeyano.remote-download.pick";
pub const PICK_DESCRIPTION: &str = "pick a remote file to download";
pub const UPLOAD_ACTION: &str = "kosukeyano.remote-download.upload";
pub const UPLOAD_DESCRIPTION: &str = "upload files from the connected Mac";

const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const COPY_BUFFER_BYTES: usize = 1024 * 1024;
const BATCH_CONTENT_TYPE: &str = "application/x-herdr-file-batch";

#[derive(Clone, Debug)]
pub enum ReceiverEndpoint {
    Tcp { host: String, port: u16 },
    Unix(PathBuf),
}

impl ReceiverEndpoint {
    pub fn display(&self) -> String {
        match self {
            Self::Tcp { host, port } => format!("{host}:{port}"),
            Self::Unix(path) => path.display().to_string(),
        }
    }
}

trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

#[derive(Debug)]
struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

pub fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

pub fn default_config_dir() -> Result<PathBuf> {
    Ok(match env::var_os("XDG_CONFIG_HOME") {
        Some(root) => expand_home(&root.to_string_lossy())?.join("herdr-remote-download"),
        None => home_dir()?.join(".config/herdr-remote-download"),
    })
}

pub fn default_token_path() -> Result<PathBuf> {
    Ok(default_config_dir()?.join("token"))
}

pub fn default_data_dir() -> Result<PathBuf> {
    Ok(match env::var_os("XDG_DATA_HOME") {
        Some(root) => expand_home(&root.to_string_lossy())?.join("herdr-remote-download"),
        None => home_dir()?.join(".local/share/herdr-remote-download"),
    })
}

pub fn default_download_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join("Downloads"))
}

pub fn default_herdr_config_path() -> Result<PathBuf> {
    Ok(home_dir()?.join(".config/herdr/config.toml"))
}

pub fn default_remote_socket_path() -> Result<PathBuf> {
    let user = env::var("USER")
        .or_else(|_| env::var("LOGNAME"))
        .or_else(|_| {
            Command::new("id")
                .arg("-un")
                .output()
                .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        })
        .context("could not determine the remote user name")?;
    if user.is_empty() || user.contains('/') || user.contains('\0') {
        bail!("unsupported remote user name");
    }
    Ok(PathBuf::from(format!(
        "/tmp/herdr-remote-download-{user}.sock"
    )))
}

pub fn sender_token_path() -> Result<PathBuf> {
    if let Some(path) = env::var_os("HERDR_DOWNLOAD_TOKEN_FILE") {
        return expand_home(&path.to_string_lossy());
    }
    if let Some(directory) = env::var_os("HERDR_PLUGIN_CONFIG_DIR") {
        let path = PathBuf::from(directory).join("token");
        if path.exists() {
            return Ok(path);
        }
    }
    default_token_path()
}

pub fn validate_token(value: &str) -> Result<String> {
    let token = value.trim();
    if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("token must contain exactly 64 hexadecimal characters");
    }
    Ok(token.to_ascii_lowercase())
}

pub fn read_token(path: &Path) -> Result<String> {
    let value = fs::read_to_string(path)
        .with_context(|| format!("cannot read token file {}", path.display()))?;
    validate_token(&value)
}

pub fn ensure_token(path: &Path) -> Result<String> {
    if path.exists() {
        return read_token(path);
    }

    let parent = path
        .parent()
        .context("token path does not have a parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("cannot create token directory {}", parent.display()))?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("cannot secure token directory {}", parent.display()))?;

    let mut random = [0_u8; 32];
    File::open("/dev/urandom")
        .context("cannot open /dev/urandom")?
        .read_exact(&mut random)
        .context("cannot read secure random bytes")?;
    let token = bytes_to_hex(&random);

    let mut file = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return read_token(path),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("cannot create token file {}", path.display()))
        }
    };
    if let Err(error) = writeln!(file, "{token}").and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(path);
        return Err(error).context("cannot write token file");
    }
    Ok(token)
}

pub fn encode_header_value(value: &str) -> String {
    URL_SAFE_NO_PAD.encode(value.as_bytes())
}

pub fn decode_header_value(value: &str) -> Result<String> {
    if value.is_empty() || value.len() > 1024 {
        bail!("invalid encoded filename");
    }
    let decoded = URL_SAFE_NO_PAD
        .decode(value)
        .context("invalid encoded filename")?;
    String::from_utf8(decoded).context("invalid encoded filename")
}

pub fn clean_filename(value: &str) -> Result<String> {
    let normalized = value.replace('\\', "/");
    let name = normalized.rsplit('/').next().unwrap_or("").trim();
    if name.is_empty()
        || matches!(name, "." | "..")
        || name.contains('\0')
        || name.chars().any(|character| character.is_control())
        || name.len() > 240
    {
        bail!("unsafe or unsupported filename");
    }
    Ok(name.to_string())
}

fn strip_path_markup(value: &str) -> &str {
    let mut candidate = value.trim();
    if candidate.starts_with('[') && candidate.ends_with(')') {
        if let Some(index) = candidate.find("](") {
            candidate = candidate[index + 2..candidate.len() - 1].trim();
        }
    }
    if candidate.len() >= 2 {
        let first = candidate.as_bytes()[0] as char;
        let last = candidate.as_bytes()[candidate.len() - 1] as char;
        if matches!(
            (first, last),
            ('`', '`') | ('\'', '\'') | ('"', '"') | ('<', '>')
        ) {
            candidate = candidate[1..candidate.len() - 1].trim();
        }
    }
    candidate
}

fn percent_decode(value: &str) -> Result<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                bail!("invalid percent encoding in file URL");
            }
            let high =
                hex_value(bytes[index + 1]).context("invalid percent encoding in file URL")?;
            let low =
                hex_value(bytes[index + 2]).context("invalid percent encoding in file URL")?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).context("file URL path is not valid UTF-8")
}

fn current_hostnames() -> Vec<String> {
    let mut names = vec![String::new(), "localhost".to_string()];
    if let Ok(value) = env::var("HOSTNAME") {
        names.push(value);
    }
    for argument in [None, Some("-f")] {
        let mut command = Command::new("hostname");
        if let Some(argument) = argument {
            command.arg(argument);
        }
        if let Ok(output) = command.output() {
            if output.status.success() {
                names.push(String::from_utf8_lossy(&output.stdout).trim().to_string());
            }
        }
    }
    names
}

fn path_from_file_url(value: &str) -> Result<String> {
    let Some(remainder) = value.strip_prefix("file://") else {
        return Ok(value.to_string());
    };
    let (host, path) = if remainder.starts_with('/') {
        ("", remainder)
    } else if let Some(index) = remainder.find('/') {
        (&remainder[..index], &remainder[index..])
    } else {
        (remainder, "")
    };
    if !current_hostnames()
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(host))
    {
        bail!("file URL belongs to another host: {host}");
    }
    percent_decode(path)
}

/// Deletes the temporary archive when the transfer ends, success or failure.
pub struct TemporaryArchive(PathBuf);

impl Drop for TemporaryArchive {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Create a temporary .tar.gz archive of a directory for transfer.
pub fn archive_directory(path: &Path) -> Result<TemporaryArchive> {
    let base = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let archive_path = env::temp_dir().join(format!(
        "herdr-archive-{}-{base}.tar.gz",
        std::process::id()
    ));
    let file = File::create(&archive_path)
        .with_context(|| format!("cannot create {}", archive_path.display()))?;
    let encoder = GzEncoder::new(file, Compression::default());
    let mut builder = tar::Builder::new(encoder);
    builder.follow_symlinks(false);
    // Archive the directory contents without the parent-name prefix; the
    // receiver recreates the name when extracting.
    builder
        .append_dir_all(".", path)
        .with_context(|| format!("cannot archive directory {}", path.display()))?;
    builder
        .into_inner()
        .and_then(|encoder| encoder.finish())
        .with_context(|| format!("cannot finish archiving {}", path.display()))?;
    Ok(TemporaryArchive(archive_path))
}

fn ensure_safe_entry_path(path: &Path) -> Result<()> {
    if path.is_absolute()
        || path
            .components()
            .any(|component| component == std::path::Component::ParentDir)
    {
        bail!("archive contains an unsafe path: {}", path.display());
    }
    Ok(())
}

/// Extract a verified .tar.gz into destination under a unique directory named
/// after the archive.
pub fn extract_archive(archive_path: &Path, destination: &Path, name: &str) -> Result<PathBuf> {
    let name = clean_filename(name)?;
    let mut index = 0_u32;
    let root = loop {
        let candidate = if index == 0 {
            destination.join(&name)
        } else {
            destination.join(format!("{name} ({index})"))
        };
        match fs::create_dir(&candidate) {
            Ok(()) => break candidate,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).context("cannot create extraction directory")
            }
        }
        index += 1;
        if index >= 10_000 {
            bail!("could not allocate a unique extraction directory");
        }
    };
    let file = File::open(archive_path)
        .with_context(|| format!("cannot reopen {}", archive_path.display()))?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    // ponytail: only regular files and directories are restored; symlinks and
    // special files are skipped. Add link handling if real trees need them.
    for entry in archive
        .entries()
        .context("cannot read archive entries")?
    {
        let mut entry = entry.context("cannot read archive entry")?;
        let entry_path = entry
            .path()?
            .to_path_buf();
        ensure_safe_entry_path(&entry_path)?;
        match entry.header().entry_type() {
            tar::EntryType::Regular | tar::EntryType::Directory => {
                entry
                    .unpack_in(&root)
                    .with_context(|| format!("cannot extract {}", entry_path.display()))?;
            }
            _ => {}
        }
    }
    Ok(root)
}

pub fn resolve_path_from_context(context: &Value) -> Result<PathBuf> {
    let raw = context
        .get("clicked_url")
        .or_else(|| context.get("selected_text"))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .context("select one remote file path or click a file:// link first")?;
    let lines = raw
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if lines.len() != 1 {
        bail!("select exactly one file path");
    }

    let candidate = path_from_file_url(strip_path_markup(lines[0]))?;
    if candidate.contains('\0') {
        bail!("file path contains a null byte");
    }
    let base = context
        .get("focused_pane_cwd")
        .or_else(|| context.get("workspace_cwd"))
        .and_then(Value::as_str)
        .map(expand_home)
        .transpose()?
        .unwrap_or(env::current_dir().context("cannot determine current directory")?);

    let mut candidates = vec![candidate.as_str()];
    if let Some(without_suffix) = strip_line_suffix(&candidate) {
        candidates.push(without_suffix);
    }
    let mut shown = None;
    for value in candidates {
        let expanded = expand_home(value)?;
        let path = if expanded.is_absolute() {
            expanded
        } else {
            base.join(expanded)
        };
        shown = Some(path.clone());
        if path.is_file() || path.is_dir() {
            return path
                .canonicalize()
                .with_context(|| format!("cannot resolve path {}", path.display()));
        }
    }
    bail!(
        "path does not exist or is not a regular file or directory: {}",
        shown
            .map(|path| path.display().to_string())
            .unwrap_or(candidate)
    )
}

fn strip_line_suffix(value: &str) -> Option<&str> {
    let (without_last, last) = value.rsplit_once(':')?;
    if !last.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if let Some((without_line, line)) = without_last.rsplit_once(':') {
        if line.bytes().all(|byte| byte.is_ascii_digit()) {
            return Some(without_line);
        }
    }
    Some(without_last)
}

pub fn context_from_environment() -> Result<Value> {
    let raw = env::var("HERDR_PLUGIN_CONTEXT_JSON").unwrap_or_else(|_| "{}".to_string());
    let mut context: Value =
        serde_json::from_str(&raw).context("HERDR_PLUGIN_CONTEXT_JSON is invalid")?;
    if !context.is_object() {
        bail!("HERDR_PLUGIN_CONTEXT_JSON must be an object");
    }
    if let Ok(clicked_url) = env::var("HERDR_PLUGIN_CLICKED_URL") {
        context["clicked_url"] = Value::String(clicked_url);
    }
    Ok(context)
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file =
        File::open(path).with_context(|| format!("cannot open file {}", path.display()))?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    loop {
        let count = file
            .read(&mut buffer)
            .with_context(|| format!("cannot read file {}", path.display()))?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(bytes_to_hex(&digest.finalize()))
}

fn connect_endpoint(endpoint: &ReceiverEndpoint, timeout: Duration) -> Result<Box<dyn ReadWrite>> {
    match endpoint {
        ReceiverEndpoint::Tcp { host, port } => {
            let stream = TcpStream::connect((host.as_str(), *port))
                .with_context(|| format!("cannot connect to {host}:{port}"))?;
            stream.set_read_timeout(Some(timeout))?;
            stream.set_write_timeout(Some(timeout))?;
            Ok(Box::new(stream))
        }
        ReceiverEndpoint::Unix(path) => {
            let stream = UnixStream::connect(path)
                .with_context(|| format!("cannot connect to {}", path.display()))?;
            stream.set_read_timeout(Some(timeout))?;
            stream.set_write_timeout(Some(timeout))?;
            Ok(Box::new(stream))
        }
    }
}

fn read_http_response(stream: &mut dyn ReadWrite, max_body: usize) -> Result<HttpResponse> {
    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    read_limited_line(&mut reader, &mut status_line, MAX_HEADER_BYTES)?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .context("receiver returned an invalid HTTP status line")?
        .parse::<u16>()
        .context("receiver returned an invalid HTTP status")?;
    let headers = read_headers(&mut reader)?;
    let length = headers
        .get("content-length")
        .context("receiver response did not include Content-Length")?
        .parse::<usize>()
        .context("receiver returned an invalid Content-Length")?;
    if length > max_body {
        bail!("receiver response is too large");
    }
    let mut body = vec![0_u8; length];
    reader
        .read_exact(&mut body)
        .context("receiver response ended early")?;
    Ok(HttpResponse { status, body })
}

pub fn check_receiver(endpoint: &ReceiverEndpoint, timeout_seconds: u64) -> Result<()> {
    if timeout_seconds == 0 {
        bail!("transfer timeout must be greater than zero");
    }
    let timeout = Duration::from_secs(timeout_seconds.min(PREFLIGHT_TIMEOUT_SECONDS));
    let display = endpoint.display();
    let mut stream = connect_endpoint(endpoint, timeout).map_err(|_| {
        anyhow!(
            "local receiver is unavailable through {display}; reconnect the Herdr remote session \
             and verify its SSH RemoteForward"
        )
    })?;
    stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .and_then(|_| stream.flush())
        .map_err(|_| {
            anyhow!(
                "local receiver is unavailable through {display}; reconnect the Herdr remote \
                 session and verify its SSH RemoteForward"
            )
        })?;
    let response = read_http_response(stream.as_mut(), 16 * 1024).map_err(|_| {
        anyhow!(
            "local receiver is unavailable through {display}; reconnect the Herdr remote session \
             and verify its SSH RemoteForward"
        )
    })?;
    let payload: Value = serde_json::from_slice(&response.body).unwrap_or(Value::Null);
    if response.status != 200
        || payload.get("service").and_then(Value::as_str) != Some("herdr-remote-download")
        || payload.get("status").and_then(Value::as_str) != Some("ok")
    {
        bail!(
            "unexpected service through {display}; verify the Herdr remote-download SSH \
             RemoteForward"
        );
    }
    Ok(())
}

pub fn upload_file(
    path: &Path,
    endpoint: &ReceiverEndpoint,
    token: &str,
    timeout_seconds: u64,
    max_bytes: u64,
) -> Result<Value> {
    let token = validate_token(token)?;
    let metadata =
        fs::metadata(path).with_context(|| format!("cannot inspect file {}", path.display()))?;
    // Directories are archived to a temporary .tar.gz and extracted by the
    // receiver; regular files are streamed as-is.
    let (source_path, filename, _archive_guard) = if metadata.is_dir() {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .context("directory name is not valid UTF-8")?;
        let archive = archive_directory(path)?;
        (
            archive.0.clone(),
            format!("{name}.tar.gz"),
            Some(archive),
        )
    } else if metadata.is_file() {
        let filename = path
            .file_name()
            .and_then(|name| name.to_str())
            .context("filename is not valid UTF-8")?;
        (path.to_path_buf(), filename.to_string(), None)
    } else {
        bail!("not a regular file or directory: {}", path.display());
    };
    let length = fs::metadata(&source_path)
        .with_context(|| format!("cannot inspect file {}", source_path.display()))?
        .len();
    if length > max_bytes {
        bail!("file is larger than the {max_bytes} byte transfer limit");
    }

    check_receiver(endpoint, timeout_seconds)?;
    let checksum = sha256_file(&source_path)?;
    let timeout = Duration::from_secs(timeout_seconds);
    let display = endpoint.display();
    let mut stream = connect_endpoint(endpoint, timeout)
        .map_err(|error| anyhow!("cannot reach the local receiver through {display}: {error:#}"))?;
    let mut header = format!(
        "POST {TRANSFER_PATH} HTTP/1.1\r\n\
         Host: localhost\r\n\
         Authorization: Bearer {token}\r\n\
         Content-Length: {length}\r\n\
         Content-Type: application/octet-stream\r\n\
         X-Herdr-Filename: {}\r\n",
        encode_header_value(&filename)
    );
    if _archive_guard.is_some() {
        header.push_str("X-Herdr-Extract: 1\r\n");
    }
    header.push_str(&format!(
        "X-Herdr-SHA256: {checksum}\r\n\
         Connection: close\r\n\r\n"
    ));
    stream
        .write_all(header.as_bytes())
        .with_context(|| format!("cannot reach the local receiver through {display}"))?;
    let mut source =
        File::open(&source_path).with_context(|| format!("cannot open file {}", source_path.display()))?;
    let sent = io::copy(&mut source, &mut stream)
        .with_context(|| format!("cannot send file through {display}"))?;
    if sent != length {
        bail!("file changed while it was being transferred");
    }
    stream
        .flush()
        .with_context(|| format!("cannot finish sending file through {display}"))?;

    let response = read_http_response(stream.as_mut(), MAX_RESPONSE_BYTES)
        .with_context(|| format!("cannot read receiver response through {display}"))?;
    let payload: Value = serde_json::from_slice(&response.body).unwrap_or(Value::Null);
    if response.status != 201 {
        let detail = payload
            .get("error")
            .and_then(Value::as_str)
            .map(|detail| format!(": {detail}"))
            .unwrap_or_default();
        bail!("receiver returned HTTP {}{detail}", response.status);
    }
    if !payload.is_object() {
        bail!("receiver returned an invalid response");
    }
    Ok(payload)
}

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub destination: PathBuf,
    pub token: String,
    pub max_bytes: u64,
    pub verbose: bool,
}

pub struct DownloadServer {
    listener: TcpListener,
    config: ServerConfig,
}

impl DownloadServer {
    pub fn bind(config: ServerConfig) -> Result<Self> {
        let token = validate_token(&config.token)?;
        let listener = TcpListener::bind((config.host.as_str(), config.port))
            .with_context(|| format!("cannot bind receiver to {}:{}", config.host, config.port))?;
        Ok(Self {
            listener,
            config: ServerConfig { token, ..config },
        })
    }

    pub fn local_port(&self) -> Result<u16> {
        Ok(self.listener.local_addr()?.port())
    }

    pub fn serve_forever(&self) -> Result<()> {
        for connection in self.listener.incoming() {
            match connection {
                Ok(stream) => self.handle_stream(stream),
                Err(error) if self.config.verbose => eprintln!("receiver accept failed: {error}"),
                Err(_) => {}
            }
        }
        Ok(())
    }

    pub fn serve_count(&self, count: usize) -> Result<()> {
        for _ in 0..count {
            let (stream, _) = self.listener.accept()?;
            self.handle_stream(stream);
        }
        Ok(())
    }

    fn handle_stream<S: Read + Write>(&self, stream: S) {
        self.handle_stream_with_picker(stream, choose_file_on_mac);
    }

    fn handle_stream_with_picker<S, F>(&self, mut stream: S, picker: F)
    where
        S: Read + Write,
        F: FnOnce() -> Result<Vec<PathBuf>>,
    {
        if let Err(error) = self.process_stream_with_picker(&mut stream, picker) {
            let _ = write_json_response(
                &mut stream,
                400,
                "Bad Request",
                &json!({"error": format!("{error:#}")}),
            );
        }
    }

    fn process_stream_with_picker<S, F>(&self, stream: &mut S, picker: F) -> Result<()>
    where
        S: Read + Write,
        F: FnOnce() -> Result<Vec<PathBuf>>,
    {
        let mut reader = BufReader::new(stream);
        let mut request_line = String::new();
        read_limited_line(&mut reader, &mut request_line, MAX_HEADER_BYTES)?;
        let parts = request_line.split_whitespace().collect::<Vec<_>>();
        if parts.len() != 3 {
            bail!("invalid HTTP request line");
        }
        let headers = read_headers(&mut reader)?;
        if self.config.verbose {
            eprintln!("{} {}", parts[0], parts[1]);
        }
        match (parts[0], parts[1]) {
            ("GET", "/health") => write_json_response(
                reader.get_mut(),
                200,
                "OK",
                &json!({
                    "service": "herdr-remote-download",
                    "status": "ok",
                    "destination": self.config.destination.to_string_lossy(),
                    "capabilities": ["download", "upload"],
                }),
            ),
            ("POST", TRANSFER_PATH) => self.receive_upload(&mut reader, &headers),
            ("POST", CHOOSE_FILE_PATH) => self.send_chosen_file(&mut reader, &headers, picker),
            _ => write_json_response(
                reader.get_mut(),
                404,
                "Not Found",
                &json!({"error": "not found"}),
            ),
        }
    }

    fn receive_upload<S: Read + Write>(
        &self,
        reader: &mut BufReader<&mut S>,
        headers: &HashMap<String, String>,
    ) -> Result<()> {
        let expected = format!("Bearer {}", self.config.token);
        let provided = headers
            .get("authorization")
            .map(String::as_str)
            .unwrap_or("");
        if !constant_time_eq(expected.as_bytes(), provided.as_bytes()) {
            discard_bounded_body(reader, headers, self.config.max_bytes);
            return write_json_response(
                reader.get_mut(),
                401,
                "Unauthorized",
                &json!({"error": "unauthorized"}),
            );
        }
        let length = match headers
            .get("content-length")
            .and_then(|value| value.parse::<u64>().ok())
        {
            Some(length) => length,
            None => {
                return write_json_response(
                    reader.get_mut(),
                    411,
                    "Length Required",
                    &json!({"error": "valid Content-Length required"}),
                )
            }
        };
        if length > self.config.max_bytes {
            return write_json_response(
                reader.get_mut(),
                413,
                "Content Too Large",
                &json!({"error": "file exceeds receiver limit"}),
            );
        }
        let filename = match headers
            .get("x-herdr-filename")
            .context("missing encoded filename")
            .and_then(|value| decode_header_value(value))
            .and_then(|value| clean_filename(&value))
        {
            Ok(filename) => filename,
            Err(error) => {
                return write_json_response(
                    reader.get_mut(),
                    400,
                    "Bad Request",
                    &json!({"error": format!("{error:#}")}),
                )
            }
        };
        let expected_checksum = headers
            .get("x-herdr-sha256")
            .map(String::as_str)
            .unwrap_or("");
        if expected_checksum.len() != 64
            || !expected_checksum
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return write_json_response(
                reader.get_mut(),
                400,
                "Bad Request",
                &json!({"error": "valid SHA-256 header required"}),
            );
        }

        let destination_existed = self.config.destination.exists();
        if let Err(error) = fs::create_dir_all(&self.config.destination) {
            return write_json_response(
                reader.get_mut(),
                500,
                "Internal Server Error",
                &json!({"error": format!("cannot create destination: {error}")}),
            );
        }
        if !destination_existed {
            let _ =
                fs::set_permissions(&self.config.destination, fs::Permissions::from_mode(0o700));
        }
        let (mut temporary, temporary_path) = match create_temporary_file(&self.config.destination)
        {
            Ok(result) => result,
            Err(error) => {
                return write_json_response(
                    reader.get_mut(),
                    500,
                    "Internal Server Error",
                    &json!({"error": format!("cannot create temporary file: {error:#}")}),
                )
            }
        };

        let transfer = (|| -> Result<String> {
            let mut digest = Sha256::new();
            let mut remaining = length;
            let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
            while remaining > 0 {
                let wanted = usize::try_from(remaining.min(COPY_BUFFER_BYTES as u64))?;
                let count = reader
                    .read(&mut buffer[..wanted])
                    .context("request body ended before Content-Length")?;
                if count == 0 {
                    bail!("request body ended before Content-Length");
                }
                temporary.write_all(&buffer[..count])?;
                digest.update(&buffer[..count]);
                remaining -= count as u64;
            }
            temporary.sync_all()?;
            Ok(bytes_to_hex(&digest.finalize()))
        })();

        let actual_checksum = match transfer {
            Ok(checksum) => checksum,
            Err(error) => {
                drop(temporary);
                let _ = fs::remove_file(&temporary_path);
                return write_json_response(
                    reader.get_mut(),
                    400,
                    "Bad Request",
                    &json!({"error": format!("{error:#}")}),
                );
            }
        };
        drop(temporary);
        if !constant_time_eq(actual_checksum.as_bytes(), expected_checksum.as_bytes()) {
            let _ = fs::remove_file(&temporary_path);
            return write_json_response(
                reader.get_mut(),
                422,
                "Unprocessable Content",
                &json!({"error": "SHA-256 mismatch"}),
            );
        }
        let final_path = if headers.contains_key("x-herdr-extract") {
            let stem = filename
                .strip_suffix(".tar.gz")
                .or_else(|| filename.strip_suffix(".tgz"))
                .unwrap_or(&filename)
                .to_string();
            match extract_archive(&temporary_path, &self.config.destination, &stem) {
                Ok(root) => {
                    let _ = fs::remove_file(&temporary_path);
                    return write_json_response(
                        reader.get_mut(),
                        201,
                        "Created",
                        &json!({
                            "path": root.to_string_lossy(),
                            "bytes": length,
                            "sha256": actual_checksum,
                            "extracted": true,
                        }),
                    );
                }
                Err(error) => {
                    let _ = fs::remove_file(&temporary_path);
                    return write_json_response(
                        reader.get_mut(),
                        500,
                        "Internal Server Error",
                        &json!({"error": format!("cannot extract archive: {error:#}")}),
                    );
                }
            }
        } else {
            match commit_without_overwrite(&temporary_path, &self.config.destination, &filename) {
                Ok(path) => path,
                Err(error) => {
                    let _ = fs::remove_file(&temporary_path);
                    return write_json_response(
                        reader.get_mut(),
                        500,
                        "Internal Server Error",
                        &json!({"error": format!("cannot save file: {error:#}")}),
                    );
                }
            }
        };
        write_json_response(
            reader.get_mut(),
            201,
            "Created",
            &json!({
                "path": final_path.to_string_lossy(),
                "bytes": length,
                "sha256": actual_checksum,
            }),
        )
    }

    fn send_chosen_file<S, F>(
        &self,
        reader: &mut BufReader<&mut S>,
        headers: &HashMap<String, String>,
        picker: F,
    ) -> Result<()>
    where
        S: Read + Write,
        F: FnOnce() -> Result<Vec<PathBuf>>,
    {
        let expected = format!("Bearer {}", self.config.token);
        let provided = headers
            .get("authorization")
            .map(String::as_str)
            .unwrap_or("");
        if !constant_time_eq(expected.as_bytes(), provided.as_bytes()) {
            return write_json_response(
                reader.get_mut(),
                401,
                "Unauthorized",
                &json!({"error": "unauthorized"}),
            );
        }
        if headers.get("content-length").map(String::as_str) != Some("0") {
            return write_json_response(
                reader.get_mut(),
                400,
                "Bad Request",
                &json!({"error": "Content-Length must be zero"}),
            );
        }
        let selected = picker()?;
        match selected.as_slice() {
            [] => bail!("select at least one file"),
            [path] => self.write_file_response(reader.get_mut(), path),
            paths => self.write_file_batch_response(reader.get_mut(), paths),
        }
    }

    fn write_file_batch_response<W: Write>(&self, stream: &mut W, paths: &[PathBuf]) -> Result<()> {
        let mut files = Vec::with_capacity(paths.len());
        let mut content_length = 0_u64;
        for path in paths {
            let file = self.prepare_file(path)?;
            let header = format!(
                "Content-Length: {}\r\n\
                 X-Herdr-Filename: {}\r\n\
                 X-Herdr-SHA256: {}\r\n\r\n",
                file.length,
                encode_header_value(&file.filename),
                file.checksum
            );
            content_length = content_length
                .checked_add(header.len() as u64)
                .and_then(|length| length.checked_add(file.length))
                .context("selected files are too large to transfer as one batch")?;
            files.push((file, header));
        }

        write!(
            stream,
            "HTTP/1.1 200 OK\r\n\
             Content-Type: {BATCH_CONTENT_TYPE}\r\n\
             Content-Length: {content_length}\r\n\
             X-Herdr-File-Count: {}\r\n\
             Connection: close\r\n\r\n",
            files.len()
        )?;
        for (mut file, header) in files {
            stream.write_all(header.as_bytes())?;
            self.write_file_body(stream, &mut file)?;
        }
        stream.flush()?;
        Ok(())
    }

    fn write_file_response<W: Write>(&self, stream: &mut W, path: &Path) -> Result<()> {
        let mut file = self.prepare_file(path)?;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\n\
             Content-Type: application/octet-stream\r\n\
             Content-Length: {}\r\n\
             X-Herdr-Filename: {}\r\n\
             X-Herdr-SHA256: {}\r\n\
             Connection: close\r\n\r\n",
            file.length,
            encode_header_value(&file.filename),
            file.checksum
        )?;
        self.write_file_body(stream, &mut file)?;
        stream.flush()?;
        Ok(())
    }

    fn prepare_file(&self, path: &Path) -> Result<PreparedFile> {
        let mut source =
            File::open(path).with_context(|| format!("cannot open file {}", path.display()))?;
        let metadata = source
            .metadata()
            .with_context(|| format!("cannot inspect file {}", path.display()))?;
        if !metadata.is_file() {
            bail!("selected path is not a regular file: {}", path.display());
        }
        if metadata.len() > self.config.max_bytes {
            bail!(
                "selected file is larger than the {} byte transfer limit",
                self.config.max_bytes
            );
        }
        let filename = path
            .file_name()
            .and_then(|name| name.to_str())
            .context("selected filename is not valid UTF-8")?;
        let filename = clean_filename(filename)?;

        let mut digest = Sha256::new();
        let mut measured = 0_u64;
        let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
        loop {
            let count = source.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            digest.update(&buffer[..count]);
            measured += count as u64;
        }
        if measured != metadata.len() {
            bail!("selected file changed while it was being read");
        }
        let checksum = bytes_to_hex(&digest.finalize());
        source.seek(SeekFrom::Start(0))?;
        Ok(PreparedFile {
            source,
            filename,
            length: metadata.len(),
            checksum,
        })
    }

    fn write_file_body<W: Write>(&self, stream: &mut W, file: &mut PreparedFile) -> Result<()> {
        let copied = io::copy(&mut file.source, stream)?;
        if copied != file.length {
            bail!("selected file changed while it was being transferred");
        }
        Ok(())
    }
}

struct PreparedFile {
    source: File,
    filename: String,
    length: u64,
    checksum: String,
}

fn choose_file_on_mac() -> Result<Vec<PathBuf>> {
    if env::consts::OS != "macos" {
        bail!("the connected file picker service must run on macOS");
    }
    let script = "set selectedFiles to choose file with prompt \
                  \"Select files to upload to the current Herdr pane\" \
                  with multiple selections allowed\n\
                  set selectedPaths to {}\n\
                  repeat with selectedFile in selectedFiles\n\
                      set end of selectedPaths to POSIX path of selectedFile\n\
                  end repeat\n\
                  set AppleScript's text item delimiters to ASCII character 0\n\
                  return selectedPaths as text";
    let output = Command::new("/usr/bin/osascript")
        .args(["-e", script])
        .output()
        .context("cannot open the macOS file picker")?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if detail.contains("User canceled") || detail.contains("-128") {
            bail!("file selection was cancelled");
        }
        bail!(
            "macOS file picker failed: {}",
            if detail.is_empty() {
                "unknown error"
            } else {
                &detail
            }
        );
    }
    let mut value = output.stdout;
    if value.last() == Some(&b'\n') {
        value.pop();
    }
    let value = String::from_utf8(value).context("file picker returned invalid UTF-8")?;
    value
        .split('\0')
        .map(PathBuf::from)
        .map(|path| {
            if !path.is_file() {
                bail!("selected path is not a regular file: {}", path.display());
            }
            Ok(path)
        })
        .collect()
}

pub fn run_server(config: ServerConfig) -> Result<()> {
    let server = DownloadServer::bind(config)?;
    println!(
        "herdr-remote-download listening on http://{}:{} and saving to {}",
        server.config.host,
        server.local_port()?,
        server.config.destination.display()
    );
    io::stdout().flush()?;
    server.serve_forever()
}

fn read_limited_line<R: BufRead>(reader: &mut R, line: &mut String, limit: usize) -> Result<usize> {
    line.clear();
    let count = reader.read_line(line)?;
    if count == 0 {
        bail!("connection ended before HTTP headers");
    }
    if count > limit {
        bail!("HTTP header line is too large");
    }
    Ok(count)
}

fn read_headers<R: BufRead>(reader: &mut R) -> Result<HashMap<String, String>> {
    let mut headers = HashMap::new();
    let mut total = 0;
    loop {
        let mut line = String::new();
        total += read_limited_line(reader, &mut line, MAX_HEADER_BYTES)?;
        if total > MAX_HEADER_BYTES {
            bail!("HTTP headers are too large");
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        let (name, value) = line.split_once(':').context("invalid HTTP header")?;
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
    }
    Ok(headers)
}

fn write_json_response<W: Write>(
    stream: &mut W,
    status: u16,
    reason: &str,
    payload: &Value,
) -> Result<()> {
    let body = serde_json::to_vec(payload)?;
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: application/json; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(&body)?;
    stream.flush()?;
    Ok(())
}

fn discard_bounded_body<R: Read>(
    reader: &mut R,
    headers: &HashMap<String, String>,
    max_bytes: u64,
) {
    let Some(length) = headers
        .get("content-length")
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|length| *length <= max_bytes)
    else {
        return;
    };
    let _ = io::copy(&mut reader.take(length), &mut io::sink());
}

fn create_temporary_file(directory: &Path) -> Result<(File, PathBuf)> {
    let base = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for attempt in 0..100_u32 {
        let path = directory.join(format!(
            ".herdr-download-{}-{base}-{attempt}.part",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => return Ok((file, path)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("cannot create temporary file"),
        }
    }
    bail!("could not allocate a temporary file")
}

fn commit_without_overwrite(
    temporary_path: &Path,
    directory: &Path,
    filename: &str,
) -> Result<PathBuf> {
    for candidate in destination_candidates(directory, filename) {
        match fs::hard_link(temporary_path, &candidate) {
            Ok(()) => {
                fs::remove_file(temporary_path)?;
                return Ok(candidate);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("cannot commit downloaded file"),
        }
    }
    bail!("could not allocate a unique destination filename")
}

fn destination_candidates(directory: &Path, filename: &str) -> Vec<PathBuf> {
    let path = Path::new(filename);
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or(filename);
    let extension = path.extension().and_then(|value| value.to_str());
    let mut candidates = Vec::with_capacity(10_000);
    candidates.push(directory.join(filename));
    for index in 1..10_000 {
        let name = match extension {
            Some(extension) => format!("{stem} ({index}).{extension}"),
            None => format!("{stem} ({index})"),
        };
        candidates.push(directory.join(name));
    }
    candidates
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0_u8;
    for (left, right) in left.iter().zip(right) {
        difference |= left ^ right;
    }
    difference == 0
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn expand_home(value: &str) -> Result<PathBuf> {
    if value == "~" {
        return home_dir();
    }
    if let Some(relative) = value.strip_prefix("~/") {
        return Ok(home_dir()?.join(relative));
    }
    Ok(PathBuf::from(value))
}

pub fn notify_herdr(title: &str, body: &str, sound: &str) {
    let Some(binary) = env::var_os("HERDR_BIN_PATH") else {
        return;
    };
    let _ = Command::new(binary)
        .args([
            "notification",
            "show",
            title,
            "--body",
            body,
            "--sound",
            sound,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

pub fn launchd_plist(
    binary: &Path,
    token_path: &Path,
    download_dir: &Path,
    port: u16,
    log_path: &Path,
) -> String {
    let arguments = [
        binary.display().to_string(),
        "serve".to_string(),
        "--host".to_string(),
        "127.0.0.1".to_string(),
        "--port".to_string(),
        port.to_string(),
        "--download-dir".to_string(),
        download_dir.display().to_string(),
        "--token-file".to_string(),
        token_path.display().to_string(),
    ];
    let argument_xml = arguments
        .iter()
        .map(|argument| format!("      <string>{}</string>", xml_escape(argument)))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LAUNCHD_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
{argument_xml}
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ProcessType</key>
  <string>Interactive</string>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#,
        log = xml_escape(&log_path.display().to_string())
    )
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn launchd_uid() -> Result<String> {
    let output = Command::new("/usr/bin/id")
        .arg("-u")
        .output()
        .context("cannot run id -u")?;
    if !output.status.success() {
        bail!("id -u failed");
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

pub fn install_service(
    binary_source: Option<&Path>,
    token_path: &Path,
    download_dir: &Path,
    port: u16,
) -> Result<PathBuf> {
    if env::consts::OS != "macos" && env::consts::OS != "linux" {
        bail!("service installation requires macOS or Linux");
    }
    ensure_token(token_path)?;
    let source = binary_source
        .map(PathBuf::from)
        .unwrap_or(env::current_exe().context("cannot determine current executable")?);
    if !source.is_file() {
        bail!("Rust executable not found: {}", source.display());
    }
    let installed_binary = install_binary(&source)?;

    fs::create_dir_all(download_dir)?;
    match env::consts::OS {
        "macos" => install_launchd_service(&installed_binary, token_path, download_dir, port),
        _ => install_systemd_service(&installed_binary, token_path, download_dir, port),
    }
}

fn install_binary(source: &Path) -> Result<PathBuf> {
    let install_dir = default_data_dir()?;
    fs::create_dir_all(&install_dir)?;
    fs::set_permissions(&install_dir, fs::Permissions::from_mode(0o700))?;
    let installed_binary = install_dir.join("herdr-remote-download");
    let temporary_binary = install_dir.join(format!(
        ".herdr-remote-download-install-{}",
        std::process::id()
    ));
    fs::copy(source, &temporary_binary).with_context(|| {
        format!(
            "cannot copy {} to {}",
            source.display(),
            temporary_binary.display()
        )
    })?;
    fs::set_permissions(&temporary_binary, fs::Permissions::from_mode(0o755))?;
    fs::rename(&temporary_binary, &installed_binary)?;
    Ok(installed_binary)
}

pub fn systemd_unit(
    binary: &Path,
    token_path: &Path,
    download_dir: &Path,
    port: u16,
) -> String {
    format!(
        r#"[Unit]
Description=Herdr remote download receiver

[Service]
ExecStart={binary} serve --host 127.0.0.1 --port {port} --download-dir {dir} --token-file {token}
Restart=always

[Install]
WantedBy=default.target
"#,
        binary = binary.display(),
        port = port,
        dir = download_dir.display(),
        token = token_path.display(),
    )
}

fn install_systemd_service(
    installed_binary: &Path,
    token_path: &Path,
    download_dir: &Path,
    port: u16,
) -> Result<PathBuf> {
    let unit_dir = home_dir()?.join(".config/systemd/user");
    fs::create_dir_all(&unit_dir).context("cannot create the systemd user unit directory")?;
    let unit_path = unit_dir.join(format!("{LAUNCHD_LABEL}.service"));
    let unit = systemd_unit(installed_binary, token_path, download_dir, port);
    let temporary_unit = unit_path.with_extension(format!("service.tmp-{}", std::process::id()));
    fs::write(&temporary_unit, unit)?;
    fs::rename(&temporary_unit, &unit_path)?;

    let reload = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .output()
        .context("cannot run systemctl --user daemon-reload")?;
    if !reload.status.success() {
        bail!(
            "systemctl daemon-reload failed: {}",
            String::from_utf8_lossy(&reload.stderr).trim()
        );
    }
    let enable = Command::new("systemctl")
        .args(["--user", "enable", "--now", unit_path.file_name().context("unit path has no file name")?.to_string_lossy().as_ref()])
        .output()
        .context("cannot run systemctl --user enable")?;
    if !enable.status.success() {
        bail!(
            "systemctl enable failed: {}",
            String::from_utf8_lossy(&enable.stderr).trim()
        );
    }
    Ok(unit_path)
}

fn install_launchd_service(
    installed_binary: &Path,
    token_path: &Path,
    download_dir: &Path,
    port: u16,
) -> Result<PathBuf> {
    let log_path = home_dir()?.join("Library/Logs/herdr-remote-download.log");
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let plist_path = home_dir()?.join(format!("Library/LaunchAgents/{LAUNCHD_LABEL}.plist"));
    if let Some(parent) = plist_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let plist = launchd_plist(installed_binary, token_path, download_dir, port, &log_path);
    let temporary_plist = plist_path.with_extension(format!("plist.tmp-{}", std::process::id()));
    fs::write(&temporary_plist, plist)?;
    fs::rename(&temporary_plist, &plist_path)?;

    let domain = format!("gui/{}", launchd_uid()?);
    let service = format!("{domain}/{LAUNCHD_LABEL}");
    let _ = Command::new("/bin/launchctl")
        .args(["bootout", &service])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let bootstrap = Command::new("/bin/launchctl")
        .arg("bootstrap")
        .arg(&domain)
        .arg(&plist_path)
        .output()
        .context("cannot run launchctl bootstrap")?;
    if !bootstrap.status.success() {
        let detail = if bootstrap.stderr.is_empty() {
            String::from_utf8_lossy(&bootstrap.stdout)
        } else {
            String::from_utf8_lossy(&bootstrap.stderr)
        };
        bail!("launchctl bootstrap failed: {}", detail.trim());
    }
    let _ = Command::new("/bin/launchctl")
        .args(["kickstart", "-k", &service])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    Ok(plist_path)
}

pub fn service_status() -> Result<bool> {
    match env::consts::OS {
        "macos" => {
            let service = format!("gui/{}/{LAUNCHD_LABEL}", launchd_uid()?);
            Ok(Command::new("/bin/launchctl")
                .args(["print", &service])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .context("cannot run launchctl print")?
                .success())
        }
        "linux" => Ok(Command::new("systemctl")
            .args(["--user", "is-active", "--quiet", &format!("{LAUNCHD_LABEL}.service")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("cannot run systemctl --user is-active")?
            .success()),
        _ => bail!("service status requires macOS or Linux"),
    }
}

pub fn add_keybinding_config(content: &str, key: &str) -> Result<(String, bool)> {
    if content.contains(PICK_ACTION) {
        let legacy_description = "description = \"download selected remote file\"";
        if content.contains(legacy_description) {
            return Ok((
                content.replacen(
                    legacy_description,
                    &format!("description = \"{PICK_DESCRIPTION}\""),
                    1,
                ),
                true,
            ));
        }
        return Ok((content.to_string(), false));
    }
    if !key
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'_' | b'-'))
    {
        bail!("keybinding contains unsupported characters");
    }
    let key_pattern = Regex::new(&format!(r#"(?m)^key\s*=\s*"{}"\s*$"#, regex::escape(key)))?;
    let legacy_command = format!("command = \"{DOWNLOAD_ACTION}\"");
    if key_pattern.is_match(content) && content.contains(&legacy_command) {
        let updated = content.replacen(&legacy_command, &format!("command = \"{PICK_ACTION}\""), 1);
        let updated = updated.replacen(
            "description = \"download selected remote file\"",
            &format!("description = \"{PICK_DESCRIPTION}\""),
            1,
        );
        return Ok((updated, true));
    }
    if key_pattern.is_match(content) {
        bail!("keybinding is already in use: {key}");
    }
    let block = format!(
        "\n[[keys.command]]\nkey = \"{key}\"\ntype = \"plugin_action\"\n\
         command = \"{PICK_ACTION}\"\ndescription = \"{PICK_DESCRIPTION}\"\n"
    );
    let updated = if let Some(index) = content.find("[worktrees]") {
        format!(
            "{}\n{}\n{}",
            content[..index].trim_end(),
            block,
            &content[index..]
        )
    } else {
        format!("{}\n{block}", content.trim_end())
    };
    Ok((updated, true))
}

pub fn configure_keybinding(path: &Path, key: &str) -> Result<bool> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("cannot read Herdr config {}", path.display()))?;
    let (updated, changed) = add_keybinding_config(&content, key)?;
    if !changed {
        return Ok(false);
    }
    let backup = path.with_file_name(format!(
        "{}.before-herdr-remote-download.bak",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("config.toml")
    ));
    if !backup.exists() {
        fs::copy(path, &backup)?;
    }
    let mode = fs::metadata(path)?.permissions().mode();
    let temporary = path.with_extension(format!("toml.tmp-{}", std::process::id()));
    fs::write(&temporary, updated)?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(mode))?;
    fs::rename(&temporary, path)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory {
        path: PathBuf,
    }

    impl TestDirectory {
        fn new() -> Self {
            let unique = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!(
                "herdr-transfer-test-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn test_server(root: &Path, token: &str) -> DownloadServer {
        DownloadServer::bind(ServerConfig {
            host: "127.0.0.1".to_string(),
            port: 0,
            destination: root.join("downloads"),
            token: token.to_string(),
            max_bytes: 1024 * 1024,
            verbose: false,
        })
        .unwrap()
    }

    #[test]
    fn resolves_context_paths_and_suffixes() {
        let root = TestDirectory::new();
        let file = root.path.join("result data.txt");
        fs::write(&file, "result").unwrap();
        let context = json!({
            "selected_text": format!("{}:42:7", file.display()),
            "focused_pane_cwd": root.path,
        });
        assert_eq!(
            resolve_path_from_context(&context).unwrap(),
            file.canonicalize().unwrap()
        );
    }

    #[test]
    fn resolves_markdown_and_relative_paths() {
        let root = TestDirectory::new();
        let file = root.path.join("result.txt");
        fs::write(&file, "result").unwrap();
        let context = json!({
            "selected_text": "[result](result.txt:12)",
            "focused_pane_cwd": root.path,
        });
        assert_eq!(
            resolve_path_from_context(&context).unwrap(),
            file.canonicalize().unwrap()
        );
    }

    #[test]
    fn clicked_file_url_takes_priority() {
        let root = TestDirectory::new();
        let file = root.path.join("result data.txt");
        fs::write(&file, "result").unwrap();
        let url = format!("file://{}", file.display()).replace(' ', "%20");
        let context = json!({
            "clicked_url": url,
            "selected_text": "missing.txt",
            "focused_pane_cwd": root.path,
        });
        assert_eq!(
            resolve_path_from_context(&context).unwrap(),
            file.canonicalize().unwrap()
        );
    }

    #[test]
    fn rejects_multiline_context() {
        let context = json!({"selected_text": "one.txt\ntwo.txt"});
        assert!(resolve_path_from_context(&context)
            .unwrap_err()
            .to_string()
            .contains("exactly one"));
    }

    #[test]
    fn token_is_private_and_reused() {
        let root = TestDirectory::new();
        let path = root.path.join("config/token");
        let first = ensure_token(&path).unwrap();
        let second = ensure_token(&path).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.len(), 64);
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn upload_saves_without_overwriting() {
        let root = TestDirectory::new();
        let token = "ab".repeat(32);
        let server = test_server(&root.path, &token);
        let port = server.local_port().unwrap();
        let destination = root.path.join("downloads");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("report.txt"), "keep").unwrap();
        let source = root.path.join("report.txt");
        fs::write(&source, "new report").unwrap();
        let handle = thread::spawn(move || server.serve_count(2).unwrap());

        let result = upload_file(
            &source,
            &ReceiverEndpoint::Tcp {
                host: "127.0.0.1".to_string(),
                port,
            },
            &token,
            5,
            1024 * 1024,
        )
        .unwrap();
        handle.join().unwrap();
        let saved = PathBuf::from(result["path"].as_str().unwrap());
        assert_eq!(saved.file_name().unwrap(), "report (1).txt");
        assert_eq!(fs::read_to_string(saved).unwrap(), "new report");
        assert_eq!(
            fs::read_to_string(destination.join("report.txt")).unwrap(),
            "keep"
        );
    }

    #[test]
    fn invalid_token_is_rejected() {
        let root = TestDirectory::new();
        let token = "ab".repeat(32);
        let server = test_server(&root.path, &token);
        let port = server.local_port().unwrap();
        let source = root.path.join("secret.txt");
        fs::write(&source, "secret").unwrap();
        let handle = thread::spawn(move || server.serve_count(2).unwrap());
        let error = upload_file(
            &source,
            &ReceiverEndpoint::Tcp {
                host: "127.0.0.1".to_string(),
                port,
            },
            &"cd".repeat(32),
            5,
            1024 * 1024,
        )
        .unwrap_err();
        handle.join().unwrap();
        let detail = format!("{error:#}");
        assert!(detail.contains("401"), "{detail}");
    }

    #[test]
    fn health_endpoint_is_http_compatible() {
        let root = TestDirectory::new();
        let server = test_server(&root.path, &"ab".repeat(32));
        let port = server.local_port().unwrap();
        let handle = thread::spawn(move || server.serve_count(1).unwrap());
        let endpoint = ReceiverEndpoint::Tcp {
            host: "127.0.0.1".to_string(),
            port,
        };
        check_receiver(&endpoint, 5).unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn chosen_mac_file_is_streamed_with_authentication() {
        let root = TestDirectory::new();
        let source = root.path.join("chosen file.txt");
        fs::write(&source, "chosen on mac").unwrap();
        let token = "ab".repeat(32);
        let server = test_server(&root.path, &token);
        let port = server.local_port().unwrap();
        let handle = thread::spawn(move || {
            let (stream, _) = server.listener.accept().unwrap();
            server.handle_stream_with_picker(stream, move || Ok(vec![source]));
        });

        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(
            stream,
            "POST {CHOOSE_FILE_PATH} HTTP/1.1\r\n\
             Authorization: Bearer {token}\r\n\
             Content-Length: 0\r\n\
             Connection: close\r\n\r\n"
        )
        .unwrap();
        stream.flush().unwrap();
        let mut reader = BufReader::new(stream);
        let mut status_line = String::new();
        read_limited_line(&mut reader, &mut status_line, MAX_HEADER_BYTES).unwrap();
        let headers = read_headers(&mut reader).unwrap();
        let length = headers["content-length"].parse::<usize>().unwrap();
        let mut body = vec![0_u8; length];
        reader.read_exact(&mut body).unwrap();
        handle.join().unwrap();

        assert!(status_line.contains("200 OK"));
        assert_eq!(
            decode_header_value(&headers["x-herdr-filename"]).unwrap(),
            "chosen file.txt"
        );
        assert_eq!(
            headers["x-herdr-sha256"],
            sha256_file(&root.path.join("chosen file.txt")).unwrap()
        );
        assert_eq!(body, b"chosen on mac");
    }

    #[test]
    fn chosen_mac_files_are_streamed_as_one_batch() {
        let root = TestDirectory::new();
        let first = root.path.join("first file.txt");
        let second = root.path.join("second.bin");
        fs::write(&first, "first").unwrap();
        fs::write(&second, "second").unwrap();
        let token = "ab".repeat(32);
        let server = test_server(&root.path, &token);
        let port = server.local_port().unwrap();
        let handle = thread::spawn(move || {
            let (stream, _) = server.listener.accept().unwrap();
            server.handle_stream_with_picker(stream, move || Ok(vec![first, second]));
        });

        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(
            stream,
            "POST {CHOOSE_FILE_PATH} HTTP/1.1\r\n\
             Authorization: Bearer {token}\r\n\
             Content-Length: 0\r\n\
             Connection: close\r\n\r\n"
        )
        .unwrap();
        stream.flush().unwrap();
        let mut reader = BufReader::new(stream);
        let mut status_line = String::new();
        read_limited_line(&mut reader, &mut status_line, MAX_HEADER_BYTES).unwrap();
        let response_headers = read_headers(&mut reader).unwrap();

        assert!(status_line.contains("200 OK"));
        assert_eq!(response_headers["content-type"], BATCH_CONTENT_TYPE);
        assert_eq!(response_headers["x-herdr-file-count"], "2");
        for (expected_name, expected_body) in [
            ("first file.txt", b"first".as_slice()),
            ("second.bin", b"second".as_slice()),
        ] {
            let headers = read_headers(&mut reader).unwrap();
            let length = headers["content-length"].parse::<usize>().unwrap();
            let mut body = vec![0_u8; length];
            reader.read_exact(&mut body).unwrap();
            assert_eq!(
                decode_header_value(&headers["x-herdr-filename"]).unwrap(),
                expected_name
            );
            assert_eq!(
                headers["x-herdr-sha256"],
                bytes_to_hex(&Sha256::digest(expected_body))
            );
            assert_eq!(body, expected_body);
        }
        handle.join().unwrap();
    }

    #[test]
    fn invalid_upload_token_does_not_open_file_picker() {
        let root = TestDirectory::new();
        let server = test_server(&root.path, &"ab".repeat(32));
        let port = server.local_port().unwrap();
        let handle = thread::spawn(move || {
            let (stream, _) = server.listener.accept().unwrap();
            server.handle_stream_with_picker(stream, || -> Result<Vec<PathBuf>> {
                panic!("unauthorized request opened the file picker")
            });
        });

        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(
            stream,
            "POST {CHOOSE_FILE_PATH} HTTP/1.1\r\n\
             Authorization: Bearer {}\r\n\
             Content-Length: 0\r\n\
             Connection: close\r\n\r\n",
            "cd".repeat(32)
        )
        .unwrap();
        stream.flush().unwrap();
        let response = read_http_response(&mut stream, MAX_RESPONSE_BYTES).unwrap();
        handle.join().unwrap();
        assert_eq!(response.status, 401);
    }

    #[test]
    fn upload_works_through_forwarded_unix_socket() {
        let root = TestDirectory::new();
        let socket_path = root.path.join("forward.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let source = root.path.join("unix.txt");
        fs::write(&source, "through unix socket").unwrap();
        let token = "ab".repeat(32);
        let destination = root.path.join("received-unix.txt");
        let destination_for_thread = destination.clone();
        let token_for_thread = token.clone();
        let handle = thread::spawn(move || {
            let (mut health, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(&mut health);
            let mut request_line = String::new();
            read_limited_line(&mut reader, &mut request_line, MAX_HEADER_BYTES).unwrap();
            read_headers(&mut reader).unwrap();
            write_json_response(
                reader.get_mut(),
                200,
                "OK",
                &json!({"service": "herdr-remote-download", "status": "ok"}),
            )
            .unwrap();

            let (mut upload, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(&mut upload);
            let mut request_line = String::new();
            read_limited_line(&mut reader, &mut request_line, MAX_HEADER_BYTES).unwrap();
            let headers = read_headers(&mut reader).unwrap();
            assert_eq!(
                headers.get("authorization"),
                Some(&format!("Bearer {token_for_thread}"))
            );
            let length = headers["content-length"].parse::<usize>().unwrap();
            let mut body = vec![0_u8; length];
            reader.read_exact(&mut body).unwrap();
            fs::write(&destination_for_thread, &body).unwrap();
            write_json_response(
                reader.get_mut(),
                201,
                "Created",
                &json!({
                    "path": destination_for_thread,
                    "bytes": length,
                    "sha256": headers["x-herdr-sha256"],
                }),
            )
            .unwrap();
        });

        let result = upload_file(
            &source,
            &ReceiverEndpoint::Unix(socket_path),
            &token,
            5,
            1024 * 1024,
        )
        .unwrap();
        handle.join().unwrap();
        assert_eq!(
            fs::read_to_string(destination).unwrap(),
            "through unix socket"
        );
        assert_eq!(result["bytes"], 19);
    }

    #[test]
    fn unavailable_receiver_fails_with_forwarding_error() {
        let root = TestDirectory::new();
        let source = root.path.join("large.bin");
        fs::write(&source, b"content").unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let error = upload_file(
            &source,
            &ReceiverEndpoint::Tcp {
                host: "127.0.0.1".to_string(),
                port,
            },
            &"ab".repeat(32),
            1,
            1024,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("local receiver is unavailable"));
    }

    #[test]
    fn checksum_mismatch_removes_partial_file() {
        let root = TestDirectory::new();
        let token = "ab".repeat(32);
        let server = test_server(&root.path, &token);
        let port = server.local_port().unwrap();
        let handle = thread::spawn(move || server.serve_count(1).unwrap());
        let body = b"corrupted";
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(
            stream,
            "POST {TRANSFER_PATH} HTTP/1.1\r\n\
             Authorization: Bearer {token}\r\n\
             Content-Length: {}\r\n\
             X-Herdr-Filename: {}\r\n\
             X-Herdr-SHA256: {}\r\n\
             Connection: close\r\n\r\n",
            body.len(),
            encode_header_value("result.bin"),
            "00".repeat(32)
        )
        .unwrap();
        stream.write_all(body).unwrap();
        stream.flush().unwrap();
        let response = read_http_response(&mut stream, MAX_RESPONSE_BYTES).unwrap();
        handle.join().unwrap();
        assert_eq!(response.status, 422);
        let destination = root.path.join("downloads");
        assert!(!destination.join("result.bin").exists());
        assert_eq!(fs::read_dir(destination).unwrap().count(), 0);
    }

    #[test]
    fn launchd_uses_rust_binary_and_loopback() {
        let plist = launchd_plist(
            Path::new("/tmp/herdr-remote-download"),
            Path::new("/tmp/token"),
            Path::new("/tmp/downloads"),
            DEFAULT_PORT,
            Path::new("/tmp/download.log"),
        );
        assert!(plist.contains("<string>/tmp/herdr-remote-download</string>"));
        assert!(plist.contains("<string>127.0.0.1</string>"));
        assert!(plist.contains("<string>18340</string>"));
        assert!(plist.contains("<string>Interactive</string>"));
        assert!(!plist.contains("python"));
    }

    #[test]
    fn systemd_unit_runs_the_rust_binary() {
        let unit = systemd_unit(
            Path::new("/home/user/.local/share/herdr-remote-download/herdr-remote-download"),
            Path::new("/home/user/.config/herdr-remote-download/token"),
            Path::new("/home/user/Downloads"),
            DEFAULT_PORT,
        );
        assert!(unit.contains("ExecStart=/home/user/.local/share/herdr-remote-download/herdr-remote-download serve --host 127.0.0.1 --port 18340"));
        assert!(unit.contains("Restart=always"));
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn directory_upload_is_extracted_on_the_receiver() {
        let root = TestDirectory::new();
        let source = root.path.join("mydir");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::write(source.join("a.txt"), "alpha").unwrap();
        fs::write(source.join("nested/b.txt"), "beta").unwrap();

        let token = "ab".repeat(32);
        let server = test_server(&root.path, &token);
        let port = server.local_port().unwrap();
        let handle = thread::spawn(move || server.serve_count(2).unwrap());

        let endpoint = ReceiverEndpoint::Tcp {
            host: "127.0.0.1".to_string(),
            port,
        };
        let response = upload_file(&source, &endpoint, &token, 30, 1024 * 1024).unwrap();
        handle.join().unwrap();

        assert_eq!(response["extracted"], true);
        let extracted = PathBuf::from(response["path"].as_str().unwrap());
        assert_eq!(extracted.file_name().unwrap(), "mydir");
        assert_eq!(fs::read_to_string(extracted.join("a.txt")).unwrap(), "alpha");
        assert_eq!(
            fs::read_to_string(extracted.join("nested/b.txt")).unwrap(),
            "beta"
        );
    }

    #[test]
    fn archive_extraction_rejects_unsafe_paths() {
        assert!(ensure_safe_entry_path(Path::new("ok/nested.txt")).is_ok());
        assert!(ensure_safe_entry_path(Path::new("/etc/passwd")).is_err());
        assert!(ensure_safe_entry_path(Path::new("../escape.txt")).is_err());
        assert!(ensure_safe_entry_path(Path::new("a/../../escape.txt")).is_err());
    }

    #[test]
    fn extraction_skips_duplicate_names_and_special_entries() {
        let root = TestDirectory::new();
        // Build an archive containing a symlink to make sure it is skipped.
        let archive_path = root.path.join("special.tar.gz");
        let file = File::create(&archive_path).unwrap();
        let encoder = GzEncoder::new(file, Compression::default());
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(0);
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_link_name("/etc/passwd").unwrap();
        builder.append_data(&mut header, "danger", io::empty()).unwrap();
        let mut regular = tar::Header::new_gnu();
        regular.set_size(5);
        regular.set_mode(0o644);
        regular.set_cksum();
        builder.append_data(&mut regular, "keep.txt", b"bytes" as &[u8]).unwrap();
        builder.into_inner().unwrap().finish().unwrap();

        let destination = root.path.join("out");
        fs::create_dir_all(&destination).unwrap();
        let extracted = extract_archive(&archive_path, &destination, "special").unwrap();
        assert!(!extracted.join("danger").exists());
        assert_eq!(fs::read_to_string(extracted.join("keep.txt")).unwrap(), "bytes");
    }

    #[test]
    fn keybinding_configuration_is_idempotent() {
        let original =
            "[keys]\nprefix = \"ctrl+b\"\n\n[worktrees]\ndirectory = \"~/.herdr/worktrees\"\n";
        let (updated, changed) = add_keybinding_config(original, "prefix+d").unwrap();
        let (repeated, changed_again) = add_keybinding_config(&updated, "prefix+d").unwrap();
        assert!(changed);
        assert!(!changed_again);
        assert_eq!(updated, repeated);
        assert!(updated.find(PICK_ACTION) < updated.find("[worktrees]"));
    }

    #[test]
    fn rejects_existing_keybinding() {
        let original = "[[keys.command]]\nkey = \"prefix+d\"\ncommand = \"other\"\n";
        assert!(add_keybinding_config(original, "prefix+d")
            .unwrap_err()
            .to_string()
            .contains("already in use"));
    }

    #[test]
    fn upgrades_legacy_direct_download_keybinding() {
        let original = format!(
            "[[keys.command]]\nkey = \"prefix+d\"\ntype = \"plugin_action\"\n\
             command = \"{DOWNLOAD_ACTION}\"\n\
             description = \"download selected remote file\"\n"
        );
        let (updated, changed) = add_keybinding_config(&original, "prefix+d").unwrap();
        assert!(changed);
        assert!(updated.contains(PICK_ACTION));
        assert!(updated.contains(PICK_DESCRIPTION));
        assert!(!updated.contains(&format!("command = \"{DOWNLOAD_ACTION}\"")));
    }
}
