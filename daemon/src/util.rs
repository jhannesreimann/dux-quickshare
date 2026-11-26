//! Small helpers: paths, file naming, MIME types, time.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::proto::{file_type, text_type};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Positive random 63-bit payload id (Android treats ids as signed longs).
pub fn random_payload_id() -> i64 {
    (rand::random::<u64>() >> 1) as i64
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home_dir().join(".config"))
        .join("dms-quickshare")
}

pub fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(std::env::temp_dir)
}

pub fn socket_path() -> PathBuf {
    runtime_dir().join("dms-quickshare.sock")
}

/// The XDG download directory (honours ~/.config/user-dirs.dirs).
pub fn default_download_dir() -> PathBuf {
    let home = home_dir();
    let dirs_file = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"))
        .join("user-dirs.dirs");
    if let Ok(text) = std::fs::read_to_string(dirs_file) {
        for line in text.lines() {
            if let Some(value) = line.trim().strip_prefix("XDG_DOWNLOAD_DIR=") {
                let value = value
                    .trim()
                    .trim_matches('"')
                    .replace("$HOME", &home.to_string_lossy());
                let path = PathBuf::from(value);
                if path.is_absolute() {
                    return path;
                }
            }
        }
    }
    home.join("Downloads")
}

pub fn expand_tilde(p: &str) -> PathBuf {
    if p == "~" {
        home_dir()
    } else if let Some(rest) = p.strip_prefix("~/") {
        home_dir().join(rest)
    } else {
        PathBuf::from(p)
    }
}

/// A friendly default device name: pretty hostname, else machine model, else hostname.
pub fn default_device_name() -> String {
    let read = |p: &str| {
        std::fs::read_to_string(p)
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    };
    if let Some(info) = read("/etc/machine-info") {
        for line in info.lines() {
            if let Some(v) = line.strip_prefix("PRETTY_HOSTNAME=") {
                let v = v.trim_matches('"').trim();
                if !v.is_empty() {
                    return v.to_owned();
                }
            }
        }
    }
    let junk = |s: &str| {
        let l = s.to_lowercase();
        l.contains("to be filled")
            || l.contains("default string")
            || l.contains("system product")
            || l == "none"
    };
    // Lenovo stores the marketing name in product_version, most vendors in product_name.
    for p in [
        "/sys/class/dmi/id/product_version",
        "/sys/class/dmi/id/product_name",
    ] {
        if let Some(v) =
            read(p).filter(|v| !junk(v) && v.chars().any(|c| c.is_alphabetic()) && v.len() > 3)
        {
            return v;
        }
    }
    read("/etc/hostname").unwrap_or_else(|| "Linux".to_owned())
}

/// Reduce a peer-supplied file name to a safe single path component.
pub fn sanitize_file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let mut cleaned: String = base.chars().filter(|c| !c.is_control()).collect();
    cleaned = cleaned.trim().to_owned();
    while cleaned.starts_with('.') {
        cleaned.remove(0);
    }
    if cleaned.is_empty() {
        cleaned = "file".to_owned();
    }
    // Keep names within common filesystem limits (255 bytes).
    while cleaned.len() > 200 {
        cleaned.pop();
    }
    cleaned
}

/// Pick a path in `dir` that does not exist yet ("a.jpg", "a (1).jpg", ...).
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    for n in 1.. {
        let candidate = dir.join(format!("{stem} ({n}){ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!()
}

/// (MIME type, Quick Share file type) for a file name.
pub fn mime_for(name: &str) -> (&'static str, i32) {
    let ext = name
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mime = match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "heic" => "image/heic",
        "heif" => "image/heif",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "tif" | "tiff" => "image/tiff",
        "mp4" | "m4v" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "3gp" => "video/3gpp",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        "pdf" => "application/pdf",
        "txt" | "log" | "md" => "text/plain",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "zip" => "application/zip",
        "apk" => "application/vnd.android.package-archive",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "odt" => "application/vnd.oasis.opendocument.text",
        "ods" => "application/vnd.oasis.opendocument.spreadsheet",
        "odp" => "application/vnd.oasis.opendocument.presentation",
        "vcf" => "text/x-vcard",
        _ => "application/octet-stream",
    };
    let kind = match mime.split('/').next().unwrap_or_default() {
        "image" => file_type::IMAGE,
        "video" => file_type::VIDEO,
        "audio" => file_type::AUDIO,
        "application" | "text" if mime != "application/octet-stream" => file_type::DOCUMENT,
        _ => file_type::UNKNOWN,
    };
    (mime, kind)
}

pub fn looks_like_url(text: &str) -> bool {
    let t = text.trim();
    !t.contains(char::is_whitespace) && (t.starts_with("http://") || t.starts_with("https://"))
}

pub fn text_kind_name(kind: i32) -> &'static str {
    match kind {
        text_type::URL => "url",
        text_type::ADDRESS => "address",
        text_type::PHONE_NUMBER => "phone",
        _ => "text",
    }
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1000.0 && unit < UNITS.len() - 1 {
        v /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

/// Launch a program detached from us (used to open files/URLs).
pub fn spawn_detached(program: &str, args: &[&str]) -> bool {
    std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}

pub fn open_path(path: &str) {
    if !spawn_detached("xdg-open", &[path]) {
        spawn_detached("gio", &["open", path]);
    }
}

pub fn copy_to_clipboard(text: &str) {
    if !spawn_detached("dms", &["cl", "copy", text]) {
        spawn_detached("wl-copy", &[text]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_names() {
        assert_eq!(sanitize_file_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_file_name("..\\x.jpg"), "x.jpg");
        assert_eq!(sanitize_file_name(".."), "file");
        assert_eq!(sanitize_file_name(".hidden"), "hidden");
        assert_eq!(sanitize_file_name("IMG_1.jpg"), "IMG_1.jpg");
    }

    #[test]
    fn mime_types() {
        assert_eq!(mime_for("a.JPG"), ("image/jpeg", file_type::IMAGE));
        assert_eq!(mime_for("a.pdf").1, file_type::DOCUMENT);
        assert_eq!(mime_for("noext").1, file_type::UNKNOWN);
    }
}
