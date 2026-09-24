//! File paths and `file://` URIs exactly as VS Code and Cursor format them.
//!
//! Ported from `src/vs/base/common/uri.ts` (`URI.file`, `uriToFsPath`, `_asFormatted`) and
//! `src/vs/base/common/extpath.ts` (`sanitizeFilePath`).

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Macos,
    Linux,
    Windows,
}

impl Platform {
    pub const fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::Macos
        } else {
            Self::Linux
        }
    }

    fn separator(self) -> char {
        match self {
            Self::Windows => '\\',
            Self::Macos | Self::Linux => '/',
        }
    }
}

struct FileUri {
    authority: String,
    path: String,
}

fn uri_file(platform: Platform, fs_path: &str) -> FileUri {
    let mut path = if platform == Platform::Windows {
        fs_path.replace('\\', "/")
    } else {
        fs_path.to_string()
    };
    let mut authority = String::new();
    if path.starts_with("//") {
        match path[2..].find('/') {
            None => {
                authority = path[2..].to_string();
                path = "/".to_string();
            }
            Some(offset) => {
                let idx = offset + 2;
                authority = path[2..idx].to_string();
                path = path[idx..].to_string();
                if path.is_empty() {
                    path = "/".to_string();
                }
            }
        }
    }
    if !path.starts_with('/') {
        path.insert(0, '/');
    }
    FileUri { authority, path }
}

fn drive_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':'
}

fn uri_to_fs_path(platform: Platform, uri: &FileUri, keep_drive_letter_casing: bool) -> String {
    let value = if !uri.authority.is_empty() && uri.path.len() > 1 {
        format!("//{}{}", uri.authority, uri.path)
    } else if drive_path(&uri.path) {
        if keep_drive_letter_casing {
            uri.path[1..].to_string()
        } else {
            format!("{}{}", uri.path[1..2].to_ascii_lowercase(), &uri.path[2..])
        }
    } else {
        uri.path.clone()
    };
    if platform == Platform::Windows {
        value.replace('/', "\\")
    } else {
        value
    }
}

/// `URI.file(path).fsPath`: the string VS Code hashes for single-folder workspaces.
pub fn fs_path(platform: Platform, path: &str) -> String {
    uri_to_fs_path(platform, &uri_file(platform, path), false)
}

/// `originalFSPath(URI.file(path))`: the string VS Code hashes for workspace files.
pub fn original_fs_path(platform: Platform, path: &str) -> String {
    uri_to_fs_path(platform, &uri_file(platform, path), true)
}

fn encode_component(value: &str, is_path: bool, is_authority: bool) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        let keep = byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'.' | b'_' | b'~')
            || (is_path && byte == b'/')
            || (is_authority && matches!(byte, b'[' | b']' | b':'));
        if keep {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `URI.file(path).toString()`: the `file://` form Cursor writes to `workspace.json` and storage.json.
pub fn file_uri(platform: Platform, path: &str) -> String {
    let uri = uri_file(platform, path);
    let mut out = String::from("file://");
    if !uri.authority.is_empty() {
        let authority = uri.authority.to_lowercase();
        match authority.rfind(':') {
            None => out.push_str(&encode_component(&authority, false, true)),
            Some(idx) => {
                out.push_str(&encode_component(&authority[..idx], false, true));
                out.push_str(&authority[idx..]);
            }
        }
    }
    let mut path = uri.path;
    if drive_path(&path) && path.as_bytes()[1].is_ascii_uppercase() {
        path = format!("/{}{}", path[1..2].to_ascii_lowercase(), &path[2..]);
    }
    out.push_str(&encode_component(&path, true, false));
    out
}

fn percent_decode(value: &str) -> String {
    let decoded = percent_encoding::percent_decode_str(value);
    match decoded.decode_utf8() {
        Ok(text) => text.into_owned(),
        Err(_) => value.to_string(),
    }
}

/// Parse a `file://` URI the way `URI.parse(uri).fsPath` does. Other schemes return `None`.
pub fn parse_file_uri(platform: Platform, uri: &str) -> Option<String> {
    let rest = uri
        .get(..7)
        .filter(|scheme| scheme.eq_ignore_ascii_case("file://"))
        .map(|_| &uri[7..])?;
    let (authority, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };
    let path = path.split(['?', '#']).next().unwrap_or("/");
    let parsed = FileUri {
        authority: percent_decode(authority),
        path: percent_decode(path),
    };
    Some(uri_to_fs_path(platform, &parsed, false))
}

/// Path of a non-file URI such as `vscode-remote://host/path`.
pub fn remote_uri_path(uri: &str) -> Option<String> {
    let (scheme, rest) = uri.split_once("://")?;
    if scheme.eq_ignore_ascii_case("file") {
        return None;
    }
    let path = rest.find('/').map(|idx| &rest[idx..]).unwrap_or("/");
    Some(percent_decode(path.split(['?', '#']).next().unwrap_or("/")))
}

fn is_windows_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    (bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/'))
        || path.starts_with("\\\\")
        || path.starts_with("//")
}

fn is_absolute(platform: Platform, path: &str) -> bool {
    match platform {
        Platform::Windows => is_windows_absolute(path),
        Platform::Macos | Platform::Linux => path.starts_with('/'),
    }
}

fn normalize_segments(root: &str, rest: &str, separator: char, windows: bool) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let splitter: &[char] = if windows { &['\\', '/'] } else { &['/'] };
    for segment in rest.split(splitter) {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    let joined = parts.join(&separator.to_string());
    format!("{root}{joined}")
}

fn windows_root(path: &str) -> (String, &str) {
    let bytes = path.as_bytes();
    if path.starts_with("\\\\") || path.starts_with("//") {
        let body = &path[2..];
        let mut pieces = body.splitn(3, ['\\', '/']);
        let server = pieces.next().unwrap_or("");
        let share = pieces.next().unwrap_or("");
        let rest = pieces.next().unwrap_or("");
        return (format!("\\\\{server}\\{share}\\"), rest);
    }
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        let rest = &path[2..];
        let rooted = rest.starts_with(['\\', '/']);
        let root = if rooted {
            format!("{}\\", &path[..2])
        } else {
            path[..2].to_string()
        };
        return (root, rest);
    }
    if path.starts_with(['\\', '/']) {
        return ("\\".to_string(), &path[1..]);
    }
    (String::new(), path)
}

/// `sanitizeFilePath(candidate, cwd)`: absolute, lexically normalized, no trailing separator.
pub fn sanitize_path(platform: Platform, candidate: &str, cwd: &str) -> String {
    let mut candidate = candidate.to_string();
    if platform == Platform::Windows && candidate.ends_with(':') {
        candidate.push('\\');
    }
    if !is_absolute(platform, &candidate) {
        let separator = platform.separator();
        candidate = format!("{cwd}{separator}{candidate}");
    }
    let normalized = match platform {
        Platform::Windows => {
            let (root, rest) = windows_root(&candidate);
            normalize_segments(&root, rest, '\\', true)
        }
        Platform::Macos | Platform::Linux => {
            let rest = candidate.trim_start_matches('/');
            normalize_segments("/", rest, '/', false)
        }
    };
    remove_trailing_separator(platform, &normalized)
}

fn remove_trailing_separator(platform: Platform, candidate: &str) -> String {
    match platform {
        Platform::Windows => {
            let mut trimmed = candidate.trim_end_matches('\\').to_string();
            if trimmed.ends_with(':') {
                trimmed.push('\\');
            }
            if trimmed.is_empty() {
                trimmed.push('\\');
            }
            trimmed
        }
        Platform::Macos | Platform::Linux => {
            if candidate.len() > 1 {
                candidate.trim_end_matches('/').to_string()
            } else {
                candidate.to_string()
            }
        }
    }
}

/// Normalize a user-supplied path the way Cursor does before it computes a workspace id.
pub fn normalize_path(path: &Path) -> PathBuf {
    let platform = Platform::current();
    let cwd = std::env::current_dir()
        .map(|dir| dir.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".to_string());
    let sanitized = sanitize_path(platform, &path.to_string_lossy(), &cwd);
    PathBuf::from(fs_path(platform, &sanitized))
}

/// `URI.file(path)` authority and decoded path components.
pub fn uri_parts(platform: Platform, fs_path: &str) -> (String, String) {
    let uri = uri_file(platform, fs_path);
    (uri.authority, uri.path)
}

pub fn path_uri(path: &Path) -> String {
    file_uri(Platform::current(), &path.to_string_lossy())
}

pub fn uri_path(uri: &str) -> Option<PathBuf> {
    parse_file_uri(Platform::current(), uri)
        .or_else(|| remote_uri_path(uri))
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_uri_encodes_every_reserved_character() {
        assert_eq!(
            file_uri(Platform::Macos, "/Users/me/my project (1)/a+b,c"),
            "file:///Users/me/my%20project%20%281%29/a%2Bb%2Cc"
        );
        assert_eq!(
            file_uri(
                Platform::Macos,
                "/Users/me/Library/Application Support/Cursor/Workspaces/1/workspace.json"
            ),
            "file:///Users/me/Library/Application%20Support/Cursor/Workspaces/1/workspace.json"
        );
        assert_eq!(
            file_uri(Platform::Macos, "/tmp/caf\u{e9}"),
            "file:///tmp/caf%C3%A9"
        );
    }

    #[test]
    fn windows_uri_lowercases_drive_and_encodes_colon() {
        assert_eq!(
            file_uri(Platform::Windows, "C:\\Users\\Me\\Proj"),
            "file:///c%3A/Users/Me/Proj"
        );
        assert_eq!(
            file_uri(Platform::Windows, "\\\\Server\\Share\\x"),
            "file://server/Share/x"
        );
    }

    #[test]
    fn parses_cursor_uris_back_to_fs_paths() {
        assert_eq!(
            parse_file_uri(Platform::Windows, "file:///c%3A/Users/Me/Proj").as_deref(),
            Some("c:\\Users\\Me\\Proj")
        );
        assert_eq!(
            parse_file_uri(Platform::Windows, "file:///C:/Users/Me/Proj").as_deref(),
            Some("c:\\Users\\Me\\Proj")
        );
        assert_eq!(
            parse_file_uri(Platform::Macos, "file:///Users/me/my%20project%20%281%29").as_deref(),
            Some("/Users/me/my project (1)")
        );
        assert_eq!(
            parse_file_uri(Platform::Windows, "file://server/Share/x").as_deref(),
            Some("\\\\server\\Share\\x")
        );
        assert_eq!(
            parse_file_uri(Platform::Macos, "vscode-remote://ssh/x"),
            None
        );
        assert_eq!(
            remote_uri_path("vscode-remote://ssh-remote%2Bbox/home/me").as_deref(),
            Some("/home/me")
        );
    }

    #[test]
    fn fs_path_matches_vscode() {
        assert_eq!(fs_path(Platform::Windows, "C:\\Users\\Me"), "c:\\Users\\Me");
        assert_eq!(
            original_fs_path(Platform::Windows, "C:\\Users\\Me"),
            "C:\\Users\\Me"
        );
        assert_eq!(fs_path(Platform::Macos, "/Users/Me"), "/Users/Me");
        assert_eq!(fs_path(Platform::Linux, "/home/me/a\\b"), "/home/me/a\\b");
    }

    #[test]
    fn sanitize_matches_vscode_on_posix() {
        let cwd = "/work";
        assert_eq!(sanitize_path(Platform::Macos, "/a/b/", cwd), "/a/b");
        assert_eq!(
            sanitize_path(Platform::Macos, "/a//b/./c/../d//", cwd),
            "/a/b/d"
        );
        assert_eq!(sanitize_path(Platform::Linux, "rel/x/", cwd), "/work/rel/x");
        assert_eq!(sanitize_path(Platform::Linux, "/", cwd), "/");
        assert_eq!(sanitize_path(Platform::Linux, "/..", cwd), "/");
    }

    #[test]
    fn sanitize_matches_vscode_on_windows() {
        let cwd = "C:\\work";
        assert_eq!(
            sanitize_path(Platform::Windows, "C:\\a\\b\\", cwd),
            "C:\\a\\b"
        );
        assert_eq!(
            sanitize_path(Platform::Windows, "C:/a/./b/../c/", cwd),
            "C:\\a\\c"
        );
        assert_eq!(sanitize_path(Platform::Windows, "C:", cwd), "C:\\");
        assert_eq!(sanitize_path(Platform::Windows, "C:\\", cwd), "C:\\");
        assert_eq!(
            sanitize_path(Platform::Windows, "rel\\x", cwd),
            "C:\\work\\rel\\x"
        );
        assert_eq!(
            sanitize_path(Platform::Windows, "\\\\srv\\share\\dir\\", cwd),
            "\\\\srv\\share\\dir"
        );
    }
}
