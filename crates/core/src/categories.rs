//! IDM-style categories, derived from the file extension.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Compressed,
    Documents,
    Music,
    Programs,
    Video,
    Images,
    Other,
}

impl Category {
    pub const ALL: [Category; 7] = [
        Category::Compressed,
        Category::Documents,
        Category::Music,
        Category::Programs,
        Category::Video,
        Category::Images,
        Category::Other,
    ];

    /// Sub-folder name used under the base download directory.
    pub fn folder(self) -> &'static str {
        match self {
            Category::Compressed => "Compressed",
            Category::Documents => "Documents",
            Category::Music => "Music",
            Category::Programs => "Programs",
            Category::Video => "Video",
            Category::Images => "Images",
            Category::Other => "General",
        }
    }

    pub fn from_extension(ext: &str) -> Category {
        let e = ext.trim_start_matches('.').to_ascii_lowercase();
        match e.as_str() {
            "zip" | "rar" | "7z" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "lz" | "lzma" | "cab"
            | "arj" | "z" | "iso" | "img" | "wim" => Category::Compressed,
            "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "rtf" | "odt" | "ods"
            | "odp" | "csv" | "epub" | "mobi" | "djvu" | "md" | "pages" | "numbers" | "key" => {
                Category::Documents
            }
            "mp3" | "wav" | "flac" | "aac" | "ogg" | "oga" | "opus" | "wma" | "m4a" | "mid" | "midi"
            | "aiff" | "ape" | "amr" => Category::Music,
            "exe" | "msi" | "msix" | "apk" | "dmg" | "pkg" | "deb" | "rpm" | "appimage" | "bat" | "sh"
            | "run" | "jar" | "bin" | "ipa" | "snap" | "flatpak" => Category::Programs,
            "mp4" | "mkv" | "avi" | "mov" | "wmv" | "flv" | "webm" | "m4v" | "mpg" | "mpeg" | "3gp"
            | "ts" | "m2ts" | "ogv" | "vob" | "m3u8" | "mpd" => Category::Video,
            "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" | "svg" | "tif" | "tiff" | "ico" | "heic"
            | "avif" | "psd" | "raw" => Category::Images,
            _ => Category::Other,
        }
    }

    pub fn from_filename(name: &str) -> Category {
        match name.rsplit_once('.') {
            Some((_, ext)) if !ext.is_empty() => Self::from_extension(ext),
            _ => Category::Other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies() {
        assert_eq!(Category::from_filename("a.ZIP"), Category::Compressed);
        assert_eq!(Category::from_filename("movie.mkv"), Category::Video);
        assert_eq!(Category::from_filename("setup.exe"), Category::Programs);
        assert_eq!(Category::from_filename("song.flac"), Category::Music);
        assert_eq!(Category::from_filename("report.pdf"), Category::Documents);
        assert_eq!(Category::from_filename("photo.jpeg"), Category::Images);
        assert_eq!(Category::from_filename("noext"), Category::Other);
        assert_eq!(Category::from_filename("weird."), Category::Other);
    }
}
