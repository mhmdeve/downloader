//! Filename extraction, sanitising and collision handling.

use percent_encoding::percent_decode_str;
use std::path::{Path, PathBuf};
use url::Url;

/// Parse a `Content-Disposition` header value and return the suggested filename.
/// Supports RFC 6266 `filename*=UTF-8''...` (preferred) and plain/quoted `filename=`.
pub fn parse_content_disposition(value: &str) -> Option<String> {
    let mut plain: Option<String> = None;
    let mut extended: Option<String> = None;

    for part in split_params(value).into_iter().skip(1) {
        let (key, val) = match part.split_once('=') {
            Some((k, v)) => (k.trim().to_ascii_lowercase(), v.trim()),
            None => continue,
        };
        if key == "filename*" {
            // charset'language'percent-encoded
            let mut it = val.splitn(3, '\'');
            let charset = it.next().unwrap_or("").to_ascii_lowercase();
            let _lang = it.next();
            if let Some(enc) = it.next() {
                let bytes: Vec<u8> = percent_decode_str(enc).collect();
                let s = if charset == "iso-8859-1" || charset == "latin1" {
                    bytes.iter().map(|&b| b as char).collect::<String>()
                } else {
                    String::from_utf8_lossy(&bytes).into_owned()
                };
                if !s.is_empty() {
                    extended = Some(s);
                }
            }
        } else if key == "filename" {
            let s = unquote(val);
            if !s.is_empty() {
                plain = Some(s);
            }
        }
    }
    extended.or(plain)
}

/// Split on `;` while respecting double quotes.
fn split_params(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut in_q = false;
    let mut esc = false;
    for (i, c) in s.char_indices() {
        if esc {
            esc = false;
            continue;
        }
        match c {
            '\\' if in_q => esc = true,
            '"' => in_q = !in_q,
            ';' if !in_q => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

fn unquote(v: &str) -> String {
    let v = v.trim();
    if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        let inner = &v[1..v.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut esc = false;
        for c in inner.chars() {
            if esc {
                out.push(c);
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else {
                out.push(c);
            }
        }
        out
    } else {
        v.to_string()
    }
}

/// Last non-empty path segment of the URL, percent-decoded.
pub fn filename_from_url(url: &Url) -> Option<String> {
    let seg = url.path_segments()?.rev().find(|s| !s.is_empty())?;
    let decoded = percent_decode_str(seg).decode_utf8_lossy().into_owned();
    if decoded.is_empty() {
        None
    } else {
        Some(decoded)
    }
}

/// Make a name safe on Windows, macOS and Linux.
pub fn sanitize(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 || c == '\u{7f}' => '_',
            c => c,
        })
        .collect();
    s = s.trim().trim_end_matches(['.', ' ']).to_string();
    s = s.trim_start_matches('.').to_string();
    if s.is_empty() {
        s = "download".into();
    }
    // Windows reserved device names (with or without extension).
    let stem = s.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit()
            && stem.as_bytes()[3] != b'0');
    if reserved {
        s.insert(0, '_');
    }
    truncate_keep_ext(&s, 200)
}

/// Truncate to at most `max` bytes on a char boundary, preserving the extension.
fn truncate_keep_ext(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let (stem, ext) = match s.rfind('.') {
        Some(i) if s.len() - i <= 16 => (&s[..i], &s[i..]),
        _ => (s, ""),
    };
    let budget = max.saturating_sub(ext.len());
    let mut cut = budget.min(stem.len());
    while cut > 0 && !stem.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{}", &stem[..cut], ext)
}

pub fn extension_for_mime(mime: &str) -> Option<&'static str> {
    let m = mime.split(';').next()?.trim().to_ascii_lowercase();
    Some(match m.as_str() {
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/x-matroska" => "mkv",
        "video/quicktime" => "mov",
        "video/x-msvideo" => "avi",
        "video/mp2t" => "ts",
        "audio/mpeg" => "mp3",
        "audio/mp4" | "audio/x-m4a" => "m4a",
        "audio/ogg" => "ogg",
        "audio/wav" | "audio/x-wav" => "wav",
        "audio/flac" => "flac",
        "audio/webm" => "weba",
        "application/pdf" => "pdf",
        "application/zip" => "zip",
        "application/x-7z-compressed" => "7z",
        "application/vnd.rar" | "application/x-rar-compressed" => "rar",
        "application/gzip" | "application/x-gzip" => "gz",
        "application/x-tar" => "tar",
        "application/vnd.apple.mpegurl" | "application/x-mpegurl" => "m3u8",
        "application/dash+xml" => "mpd",
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "text/plain" => "txt",
        "application/msword" => "doc",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => "pptx",
        "application/x-msdownload" | "application/vnd.microsoft.portable-executable" => "exe",
        "application/vnd.android.package-archive" => "apk",
        "application/x-apple-diskimage" => "dmg",
        _ => return None,
    })
}

/// Decide the final filename from the server's hints.
pub fn choose_filename(url: &Url, content_disposition: Option<&str>, content_type: Option<&str>) -> String {
    let mut name = content_disposition
        .and_then(parse_content_disposition)
        .or_else(|| filename_from_url(url))
        .unwrap_or_else(|| "download".into());
    // Content-Disposition names occasionally contain path components.
    if let Some((_, tail)) = name.rsplit_once(['/', '\\']) {
        name = tail.to_string();
    }
    let has_ext = name.rsplit_once('.').map(|(s, e)| !s.is_empty() && !e.is_empty() && e.len() <= 8).unwrap_or(false);
    if !has_ext {
        if let Some(ext) = content_type.and_then(extension_for_mime) {
            name = format!("{name}.{ext}");
        }
    }
    sanitize(&name)
}

/// `dir/name`, or `dir/stem (n).ext` if that (or its resume sidecar) already exists.
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let taken = |p: &Path| p.exists() || sidecar_path(p).exists();
    let first = dir.join(name);
    if !taken(&first) {
        return first;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    for n in 1..10_000 {
        let cand = dir.join(format!("{stem} ({n}){ext}"));
        if !taken(&cand) {
            return cand;
        }
    }
    first
}

/// Resume-state sidecar path for a destination file: `<file>.idl`.
pub fn sidecar_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_os_string();
    s.push(".idl");
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_disposition_variants() {
        assert_eq!(parse_content_disposition(r#"attachment; filename="a b.zip""#).unwrap(), "a b.zip");
        assert_eq!(parse_content_disposition("attachment; filename=plain.bin").unwrap(), "plain.bin");
        assert_eq!(
            parse_content_disposition("attachment; filename=\"fallback.txt\"; filename*=UTF-8''na%C3%AFve%20file.txt")
                .unwrap(),
            "naïve file.txt"
        );
        assert_eq!(
            parse_content_disposition(r#"attachment; filename="semi;colon.txt""#).unwrap(),
            "semi;colon.txt"
        );
        assert!(parse_content_disposition("inline").is_none());
    }

    #[test]
    fn url_names() {
        let u = Url::parse("https://x.com/a/b/My%20File.tar.gz?token=1").unwrap();
        assert_eq!(filename_from_url(&u).unwrap(), "My File.tar.gz");
        let u = Url::parse("https://x.com/").unwrap();
        assert!(filename_from_url(&u).is_none());
    }

    #[test]
    fn sanitizing() {
        assert_eq!(sanitize("a:b*c?.txt"), "a_b_c_.txt");
        assert_eq!(sanitize("CON.txt"), "_CON.txt");
        assert_eq!(sanitize("lpt1"), "_lpt1");
        assert_eq!(sanitize("com0"), "com0");
        assert_eq!(sanitize("..hidden. "), "hidden");
        assert_eq!(sanitize(""), "download");
        let long = format!("{}.mp4", "é".repeat(300));
        let s = sanitize(&long);
        assert!(s.len() <= 200 && s.ends_with(".mp4"));
    }

    #[test]
    fn choose() {
        let u = Url::parse("https://x.com/get?id=5").unwrap();
        assert_eq!(choose_filename(&u, None, Some("video/mp4")), "get.mp4");
        assert_eq!(choose_filename(&u, Some("attachment; filename=\"../../evil.exe\""), None), "evil.exe");
        let u = Url::parse("https://x.com/dir/file.iso").unwrap();
        assert_eq!(choose_filename(&u, None, Some("application/octet-stream")), "file.iso");
    }

    #[test]
    fn uniqueness() {
        let d = std::env::temp_dir().join(format!("idl-uniq-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("a.txt"), b"x").unwrap();
        assert_eq!(unique_path(&d, "a.txt"), d.join("a (1).txt"));
        std::fs::write(d.join("a (1).txt"), b"x").unwrap();
        assert_eq!(unique_path(&d, "a.txt"), d.join("a (2).txt"));
        assert_eq!(unique_path(&d, "b.txt"), d.join("b.txt"));
        std::fs::remove_dir_all(&d).ok();
    }
}
