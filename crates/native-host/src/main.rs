//! Browser native-messaging protocol boundary for Internet Downloader.
//!
//! Frames are a 4-byte little-endian length followed by a UTF-8 JSON payload.
//! Standard output is reserved for framed responses; diagnostics go to standard error.
//! The authenticated desktop IPC bridge is not connected yet; valid requests fail closed.

use std::io::{self, Read, Write};
use serde_json::{json, Value};
use url::Url;

const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
const PROTOCOL_VERSION: u64 = 1;
const ALLOWED_COMMANDS: &[&str] = &["health", "inspect", "enqueue", "status"];

#[derive(Debug, PartialEq, Eq)]
struct ProtocolError {
    code: &'static str,
    message: &'static str,
}

fn validate_request(request: &Value) -> Result<(), ProtocolError> {
    let object = request.as_object().ok_or(ProtocolError {
        code: "INVALID_REQUEST",
        message: "Request must be a JSON object",
    })?;

    if object.get("version").and_then(Value::as_u64) != Some(PROTOCOL_VERSION) {
        return Err(ProtocolError {
            code: "UNSUPPORTED_VERSION",
            message: "Protocol version must be 1",
        });
    }

    match object.get("id") {
        Some(Value::String(id)) if !id.is_empty() && id.len() <= 128 => {}
        Some(Value::Number(number)) if number.is_u64() || number.is_i64() => {}
        _ => {
            return Err(ProtocolError {
                code: "INVALID_REQUEST_ID",
                message: "Request id must be a non-empty string (max 128 bytes) or integer",
            })
        }
    }

    let command = object.get("command").and_then(Value::as_str).ok_or(ProtocolError {
        code: "MISSING_COMMAND",
        message: "A command is required",
    })?;
    if !ALLOWED_COMMANDS.contains(&command) {
        return Err(ProtocolError {
            code: "UNKNOWN_COMMAND",
            message: "Command is not supported",
        });
    }

    if command == "inspect" || command == "enqueue" {
        let raw_url = object.get("url").and_then(Value::as_str).ok_or(ProtocolError {
            code: "INVALID_URL",
            message: "This command requires a URL string",
        })?;
        let parsed = Url::parse(raw_url).map_err(|_| ProtocolError {
            code: "INVALID_URL",
            message: "URL is invalid",
        })?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return Err(ProtocolError {
                code: "INVALID_URL",
                message: "Only HTTP(S) URLs without embedded credentials are supported",
            });
        }
    }

    Ok(())
}

fn response_for(request: &Value) -> Value {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    match validate_request(request) {
        Err(error) => json!({
            "version": PROTOCOL_VERSION,
            "id": id,
            "ok": false,
            "error": { "code": error.code, "message": error.message }
        }),
        Ok(()) => json!({
            "version": PROTOCOL_VERSION,
            "id": id,
            "ok": false,
            "error": {
                "code": "APP_BRIDGE_NOT_CONFIGURED",
                "message": "Internet Downloader desktop IPC is not available yet"
            }
        }),
    }
}

fn read_frame<R: Read>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut header = [0_u8; 4];
    let mut filled = 0;
    while filled < header.len() {
        match reader.read(&mut header[filled..])? {
            0 if filled == 0 => return Ok(None),
            0 => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated frame header")),
            n => filled += n,
        }
    }

    let size = u32::from_le_bytes(header) as usize;
    if size == 0 || size > MAX_MESSAGE_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid frame size"));
    }

    let mut body = vec![0_u8; size];
    reader.read_exact(&mut body)?;
    let _: Value = serde_json::from_slice(&body)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("invalid JSON frame: {e}")))?;
    Ok(Some(body))
}

fn write_frame<W: Write>(writer: &mut W, payload: &[u8]) -> io::Result<()> {
    if payload.is_empty() || payload.len() > MAX_MESSAGE_BYTES || payload.len() > u32::MAX as usize {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid frame size"));
    }
    let _: Value = serde_json::from_slice(payload)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("invalid JSON payload: {e}")))?;
    writer.write_all(&(payload.len() as u32).to_le_bytes())?;
    writer.write_all(payload)?;
    writer.flush()
}

fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut input = stdin.lock();
    let mut output = stdout.lock();

    loop {
        let frame = match read_frame(&mut input) {
            Ok(Some(frame)) => frame,
            Ok(None) => break,
            Err(err) => {
                eprintln!("idl-native-host: invalid browser message: {err}");
                std::process::exit(2);
            }
        };

        let request: Value = match serde_json::from_slice(&frame) {
            Ok(value) => value,
            Err(err) => {
                eprintln!("idl-native-host: JSON parse failed: {err}");
                std::process::exit(2);
            }
        };
        let payload = serde_json::to_vec(&response_for(&request))
            .expect("serializing JSON response cannot fail");
        if let Err(err) = write_frame(&mut output, &payload) {
            eprintln!("idl-native-host: failed to write browser response: {err}");
            std::process::exit(3);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn encode(payload: &[u8]) -> Vec<u8> {
        let mut frame = (payload.len() as u32).to_le_bytes().to_vec();
        frame.extend_from_slice(payload);
        frame
    }

    fn request(command: &str) -> Value {
        json!({"version": 1, "id": "test-1", "command": command})
    }

    #[test]
    fn reads_valid_frame() {
        let payload = br#"{ "version":1,"id":7,"command":"health" }"#;
        assert_eq!(read_frame(&mut Cursor::new(encode(payload))).unwrap().unwrap(), payload);
    }

    #[test]
    fn clean_eof_is_ok() {
        assert!(read_frame(&mut Cursor::new(Vec::<u8>::new())).unwrap().is_none());
    }

    #[test]
    fn rejects_truncated_header() {
        assert_eq!(read_frame(&mut Cursor::new(vec![1, 0])).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn rejects_truncated_body() {
        let mut frame = 5_u32.to_le_bytes().to_vec();
        frame.extend_from_slice(b"{}");
        assert_eq!(read_frame(&mut Cursor::new(frame)).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn rejects_zero_length() {
        assert_eq!(read_frame(&mut Cursor::new(0_u32.to_le_bytes().to_vec())).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_oversized_length_before_allocating() {
        let size = (MAX_MESSAGE_BYTES as u32) + 1;
        assert_eq!(read_frame(&mut Cursor::new(size.to_le_bytes().to_vec())).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_invalid_json() {
        assert_eq!(read_frame(&mut Cursor::new(encode(b"{not json}"))).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn writes_native_frame() {
        let payload = br#"{"ok":true}"#;
        let mut out = Vec::new();
        write_frame(&mut out, payload).unwrap();
        assert_eq!(&out[..4], &(payload.len() as u32).to_le_bytes());
        assert_eq!(&out[4..], payload);
    }

    #[test]
    fn rejects_invalid_output_json() {
        assert_eq!(write_frame(&mut Vec::new(), b"{oops}").unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn accepts_supported_health_request_schema() {
        assert_eq!(validate_request(&request("health")), Ok(()));
    }

    #[test]
    fn rejects_wrong_protocol_version() {
        let mut value = request("health");
        value["version"] = json!(2);
        assert_eq!(validate_request(&value).unwrap_err().code, "UNSUPPORTED_VERSION");
    }

    #[test]
    fn rejects_missing_or_invalid_request_id() {
        let mut value = request("health");
        value.as_object_mut().unwrap().remove("id");
        assert_eq!(validate_request(&value).unwrap_err().code, "INVALID_REQUEST_ID");
        value["id"] = json!("");
        assert_eq!(validate_request(&value).unwrap_err().code, "INVALID_REQUEST_ID");
    }

    #[test]
    fn rejects_unknown_command() {
        assert_eq!(validate_request(&request("shell")).unwrap_err().code, "UNKNOWN_COMMAND");
    }

    #[test]
    fn requires_http_url_for_inspect_and_enqueue() {
        let mut value = request("inspect");
        assert_eq!(validate_request(&value).unwrap_err().code, "INVALID_URL");
        value["url"] = json!("file:///etc/passwd");
        assert_eq!(validate_request(&value).unwrap_err().code, "INVALID_URL");
        value["url"] = json!("https://user:pass@example.com/file");
        assert_eq!(validate_request(&value).unwrap_err().code, "INVALID_URL");
        value["url"] = json!("https://example.com/file");
        assert_eq!(validate_request(&value), Ok(()));
    }

    #[test]
    fn valid_request_fails_closed_until_ipc_exists() {
        let response = response_for(&request("health"));
        assert_eq!(response["ok"], json!(false));
        assert_eq!(response["error"]["code"], json!("APP_BRIDGE_NOT_CONFIGURED"));
    }

    #[test]
    fn invalid_request_returns_structured_error() {
        let response = response_for(&request("unknown"));
        assert_eq!(response["ok"], json!(false));
        assert_eq!(response["error"]["code"], json!("UNKNOWN_COMMAND"));
        assert_eq!(response["id"], json!("test-1"));
    }
}
