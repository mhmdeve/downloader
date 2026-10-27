//! Small authenticated JSON-lines service for the desktop app.
//!
//! The caller must generate a fresh high-entropy token for each app session and
//! deliver it to the native host through a private per-user bootstrap mechanism.
//! This crate never persists or logs the token. It binds only to IPv4 loopback.

use idl_core::{manager::{AddRequest, Manager, StartMode}, types::RequestInfo};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{fs::{self, OpenOptions}, io::{self, Write}, net::SocketAddr, path::PathBuf, sync::Arc};
use subtle::ConstantTimeEq;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Semaphore,
    task::JoinHandle,
};
use url::Url;

const MAX_LINE_BYTES: usize = 64 * 1024;
const MAX_CLIENTS: usize = 16;
const VERSION: u64 = 1;

#[derive(Clone, Debug, Deserialize)]
struct Request {
    version: u64,
    id: Value,
    token: String,
    command: String,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    request: Option<RequestInfo>,
    #[serde(default)]
    start: Option<StartMode>,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    code: &'static str,
    message: &'static str,
}

fn error_response(id: Value, code: &'static str, message: &'static str) -> Value {
    json!({ "version": VERSION, "id": id, "ok": false, "error": ErrorBody { code, message } })
}

fn token_matches(expected: &[u8], supplied: &[u8]) -> bool {
    if expected.len() != supplied.len() {
        return false;
    }
    bool::from(expected.ct_eq(supplied))
}

fn validate_http_url(raw: &str) -> Result<(), (&'static str, &'static str)> {
    let url = Url::parse(raw).map_err(|_| ("INVALID_URL", "URL is invalid"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(("INVALID_URL", "Only HTTP(S) URLs without embedded credentials are supported"));
    }
    Ok(())
}

async fn route(manager: &Arc<Manager>, token: &str, line: &[u8]) -> Value {
    let parsed: Value = match serde_json::from_slice(line) {
        Ok(value) => value,
        Err(_) => return error_response(Value::Null, "INVALID_JSON", "Request is not valid JSON"),
    };
    let id = parsed.get("id").cloned().unwrap_or(Value::Null);
    let request: Request = match serde_json::from_value(parsed) {
        Ok(request) => request,
        Err(_) => return error_response(id, "INVALID_REQUEST", "Required request fields are missing or invalid"),
    };
    if !token_matches(token.as_bytes(), request.token.as_bytes()) {
        return error_response(request.id, "UNAUTHORIZED", "Authentication failed");
    }
    if request.version != VERSION {
        return error_response(request.id, "UNSUPPORTED_VERSION", "Protocol version must be 1");
    }
    match request.command.as_str() {
        "health" => json!({ "version": VERSION, "id": request.id, "ok": true, "data": { "service": "idl-ipc", "ready": true } }),
        "status" => json!({ "version": VERSION, "id": request.id, "ok": true, "data": { "items": manager.list() } }),
        "inspect" => {
            let raw_url = match request.url.as_deref() {
                Some(url) => url,
                None => return error_response(request.id, "INVALID_URL", "This command requires a URL"),
            };
            if let Err((code, message)) = validate_http_url(raw_url) {
                return error_response(request.id, code, message);
            }
            let info = request.request.unwrap_or_else(|| RequestInfo::new(raw_url));
            match manager.inspect(&info).await {
                Ok(result) => json!({ "version": VERSION, "id": request.id, "ok": true, "data": result }),
                Err(_) => error_response(request.id, "INSPECT_FAILED", "The URL could not be inspected"),
            }
        }
        "enqueue" => {
            let info = request.request.or_else(|| request.url.map(RequestInfo::new));
            let info = match info {
                Some(info) => info,
                None => return error_response(request.id, "INVALID_URL", "This command requires a URL"),
            };
            if let Err((code, message)) = validate_http_url(&info.url) {
                return error_response(request.id, code, message);
            }
            let add = AddRequest { request: info, start: request.start.unwrap_or(StartMode::Now), ..Default::default() };
            match manager.add(add).await {
                Ok(result) => json!({ "version": VERSION, "id": request.id, "ok": true, "data": result }),
                Err(_) => error_response(request.id, "ENQUEUE_FAILED", "The download could not be queued"),
            }
        }
        _ => error_response(request.id, "UNKNOWN_COMMAND", "Command is not supported"),
    }
}

async fn serve_client(mut stream: TcpStream, manager: Arc<Manager>, token: Arc<String>) {
    loop {
        let mut line = Vec::with_capacity(1024);
        loop {
            let mut byte = [0u8; 1];
            match stream.read(&mut byte).await {
                Ok(0) => return,
                Ok(_) if byte[0] == b'\n' => break,
                Ok(_) => {
                    if line.len() >= MAX_LINE_BYTES {
                        let response = error_response(Value::Null, "REQUEST_TOO_LARGE", "Request line exceeds 64 KiB");
                        let mut bytes = serde_json::to_vec(&response).unwrap_or_default();
                        bytes.push(b'\n');
                        let _ = stream.write_all(&bytes).await;
                        return;
                    }
                    line.push(byte[0]);
                }
                Err(_) => return,
            }
        }
        if line.is_empty() {
            let response = error_response(Value::Null, "INVALID_REQUEST", "Empty request line");
            let mut bytes = serde_json::to_vec(&response).unwrap_or_default();
            bytes.push(b'\n');
            if stream.write_all(&bytes).await.is_err() { return; }
            continue;
        }
        let response = route(&manager, &token, &line).await;
        let mut bytes = match serde_json::to_vec(&response) {
            Ok(bytes) => bytes,
            Err(_) => return,
        };
        bytes.push(b'\n');
        if stream.write_all(&bytes).await.is_err() { return; }
    }
}

/// Running loopback service. Dropping it aborts the listener task.
pub struct IpcServer {
    addr: SocketAddr,
    task: JoinHandle<()>,
    bootstrap_path: Option<PathBuf>,
}

fn bootstrap_file_path() -> io::Result<PathBuf> {
    let base = dirs::config_dir().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "user config directory unavailable"))?;
    Ok(base.join("internet-downloader").join("ipc.json"))
}

fn hex_token(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn write_bootstrap(port: u16, token: &str) -> io::Result<PathBuf> {
    let path = bootstrap_file_path()?;
    let parent = path.parent().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid bootstrap path"))?;
    fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }

    let temporary = parent.join(format!(".ipc.json.tmp-{}", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    let result: io::Result<PathBuf> = (|| {
        serde_json::to_writer(&mut file, &json!({"port": port, "token": token}))
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        file.write_all(&[10])?;
        file.sync_all()?;
        #[cfg(windows)]
        if path.exists() {
            fs::remove_file(&path)?;
        }
        fs::rename(&temporary, &path)?;
        Ok(path.clone())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

impl IpcServer {
    /// Bind to 127.0.0.1 on an ephemeral port. The app must keep the token secret
    /// and provide it to the native host via a private local bootstrap channel.
    pub async fn bind(manager: Arc<Manager>, token: String) -> io::Result<Self> {
        if token.len() < 32 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "IPC token must be at least 32 bytes"));
        }
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let addr = listener.local_addr()?;
        let token = Arc::new(token);
        let permits = Arc::new(Semaphore::new(MAX_CLIENTS));
        let task = tokio::spawn(async move {
            loop {
                let (stream, peer) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => break,
                };
                if !peer.ip().is_loopback() { continue; }
                let permit = match permits.clone().acquire_owned().await {
                    Ok(permit) => permit,
                    Err(_) => break,
                };
                let manager = manager.clone();
                let token = token.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    serve_client(stream, manager, token).await;
                });
            }
        });
        Ok(Self { addr, task, bootstrap_path: None })
    }

    /// Start the service and publish its ephemeral port plus a fresh 256-bit token
    /// to the native host through the private per-user bootstrap file.
    pub async fn bind_with_bootstrap(manager: Arc<Manager>) -> io::Result<Self> {
        let mut random = [0u8; 32];
        getrandom::fill(&mut random)
            .map_err(|error| io::Error::new(io::ErrorKind::Other, format!("secure random generation failed: {error}")))?;
        let token = hex_token(&random);
        let mut server = Self::bind(manager, token.clone()).await?;
        server.bootstrap_path = Some(write_bootstrap(server.local_addr().port(), &token)?);
        Ok(server)
    }

    pub fn local_addr(&self) -> SocketAddr { self.addr }
}

impl Drop for IpcServer {
    fn drop(&mut self) { self.task.abort(); if let Some(path) = &self.bootstrap_path { let _ = fs::remove_file(path); } }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_comparison_accepts_equal_and_rejects_different() {
        assert!(token_matches(b"some-long-random-token-value-1234", b"some-long-random-token-value-1234"));
        assert!(!token_matches(b"some-long-random-token-value-1234", b"some-long-random-token-value-1235"));
        assert!(!token_matches(b"expected", b"short"));
    }

    #[test]
    fn token_hex_encoding_preserves_256_bits() {
        assert_eq!(hex_token(&[0x00, 0x7f, 0xff]), "007fff");
        assert_eq!(hex_token(&[0u8; 32]).len(), 64);
    }

    #[test]
    fn url_validation_rejects_file_scheme_and_credentials() {
        assert!(validate_http_url("https://example.com/a").is_ok());
        assert!(validate_http_url("file:///etc/passwd").is_err());
        assert!(validate_http_url("https://user:pass@example.com/a").is_err());
    }
    #[tokio::test]
    async fn loopback_service_authenticates_requests_end_to_end() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::TcpStream;

        let data_dir = std::env::temp_dir().join(format!("idl-ipc-test-{}", std::process::id()));
        let manager = Manager::new(&data_dir, tokio::runtime::Handle::current()).unwrap();
        let token = "test-token-with-at-least-32-bytes-long".to_owned();
        let server = IpcServer::bind(manager.clone(), token.clone()).await.unwrap();

        for (sent_token, expected_ok, expected_code) in [
            (token.as_str(), true, ""),
            ("wrong-token-with-at-least-32-bytes", false, "UNAUTHORIZED"),
        ] {
            let mut stream = TcpStream::connect(server.local_addr()).await.unwrap();
            let request = json!({
                "version": 1, "id": "integration-1", "token": sent_token, "command": "health"
            });
            let mut bytes = serde_json::to_vec(&request).unwrap();
            bytes.push(10);
            stream.write_all(&bytes).await.unwrap();
            let mut response = Vec::new();
            BufReader::new(stream).read_until(10, &mut response).await.unwrap();
            let response: Value = serde_json::from_slice(&response).unwrap();
            assert_eq!(response["ok"], json!(expected_ok));
            if expected_ok {
                assert_eq!(response["data"]["ready"], json!(true));
            } else {
                assert_eq!(response["error"]["code"], json!(expected_code));
            }
        }

        drop(server);
        drop(manager);
        let _ = fs::remove_dir_all(data_dir);
    }

}
