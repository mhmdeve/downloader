//! Minimal browser native-messaging frame reader/writer for Internet Downloader.
//!
//! Frames are a 4-byte little-endian length followed by a UTF-8 JSON payload.
//! Standard output is reserved for framed responses; diagnostics go to standard error.
//! The authenticated app bridge is intentionally not implemented in this checkpoint.

use std::io::{self, Read, Write};

const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

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
    let _: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("invalid JSON frame: {e}")))?;
    Ok(Some(body))
}

fn write_frame<W: Write>(writer: &mut W, payload: &[u8]) -> io::Result<()> {
    if payload.is_empty() || payload.len() > MAX_MESSAGE_BYTES || payload.len() > u32::MAX as usize {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid frame size"));
    }
    let _: serde_json::Value = serde_json::from_slice(payload)
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

        let request: serde_json::Value = match serde_json::from_slice(&frame) {
            Ok(value) => value,
            Err(err) => {
                eprintln!("idl-native-host: JSON parse failed: {err}");
                std::process::exit(2);
            }
        };
        // Fail closed until the authenticated desktop IPC service is connected.
        let response = serde_json::json!({
            "version": 1,
            "id": request.get("id").cloned().unwrap_or(serde_json::Value::Null),
            "ok": false,
            "error": {
                "code": "APP_BRIDGE_NOT_CONFIGURED",
                "message": "Internet Downloader desktop IPC is not available yet"
            }
        });
        let payload = serde_json::to_vec(&response).expect("serializing JSON response cannot fail");
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

    #[test]
    fn reads_valid_frame() {
        let payload = br#"{"version":1,"id":7,"command":"health"}"#;
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
}
