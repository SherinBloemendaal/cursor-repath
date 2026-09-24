//! SQLite connections and owned row values.

use anyhow::{Context, Result};
use rusqlite::types::Value;
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub fn sidecars(db: &Path) -> Vec<PathBuf> {
    ["-wal", "-shm", "-journal"]
        .into_iter()
        .map(|suffix| {
            let mut name = db.file_name().unwrap_or_default().to_os_string();
            name.push(suffix);
            db.with_file_name(name)
        })
        .collect()
}

fn sqlite_uri(path: &Path, immutable: bool) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let mut encoded = String::with_capacity(text.len() + 16);
    for ch in text.chars() {
        match ch {
            '%' => encoded.push_str("%25"),
            '?' => encoded.push_str("%3F"),
            '#' => encoded.push_str("%23"),
            other => encoded.push(other),
        }
    }
    let lead = if encoded.starts_with('/') { "" } else { "/" };
    let mode = if immutable {
        "mode=ro&immutable=1"
    } else {
        "mode=ro"
    };
    format!("file:{lead}{encoded}?{mode}")
}

/// Read-only connection that never creates files next to the database.
pub fn open_ro(path: &Path) -> Result<Connection> {
    let immutable = !sidecars(path)
        .iter()
        .filter(|sidecar| !sidecar.to_string_lossy().ends_with("-shm"))
        .any(|sidecar| sidecar.exists());
    let conn = Connection::open_with_flags(
        sqlite_uri(path, immutable),
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("failed to open {} read-only", path.display()))?;
    conn.busy_timeout(Duration::from_secs(5))?;
    Ok(conn)
}

pub fn open_rw(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("failed to open {}", path.display()))?;
    conn.busy_timeout(Duration::from_secs(5))?;
    Ok(conn)
}

pub fn text(value: &Value) -> Option<&str> {
    match value {
        Value::Text(text) => Some(text),
        Value::Blob(bytes) => std::str::from_utf8(bytes).ok(),
        Value::Null | Value::Integer(_) | Value::Real(_) => None,
    }
}

/// Same storage class as `original`, holding `updated`.
pub fn like(original: &Value, updated: String) -> Value {
    match original {
        Value::Blob(_) => Value::Blob(updated.into_bytes()),
        _ => Value::Text(updated),
    }
}

pub fn size(value: &Value) -> u64 {
    match value {
        Value::Text(text) => text.len() as u64,
        Value::Blob(bytes) => bytes.len() as u64,
        Value::Null => 0,
        Value::Integer(_) | Value::Real(_) => 8,
    }
}

pub fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |row| row.get(0),
        )
        .context("failed to query sqlite_master")?;
    Ok(count > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_open_leaves_wal_database_untouched() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("state.vscdb");
        {
            let conn = Connection::open(&db).unwrap();
            conn.pragma_update(None, "journal_mode", "WAL").unwrap();
            conn.execute_batch(
                "CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);
                 INSERT INTO ItemTable VALUES ('k', 'v');",
            )
            .unwrap();
        }
        let before: Vec<_> = std::fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        let bytes = std::fs::read(&db).unwrap();
        {
            let conn = open_ro(&db).unwrap();
            let value: String = conn
                .query_row("SELECT value FROM ItemTable WHERE key = 'k'", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(value, "v");
            assert!(conn.execute("DELETE FROM ItemTable", []).is_err());
        }
        let after: Vec<_> = std::fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(before, after);
        assert_eq!(std::fs::read(&db).unwrap(), bytes);
    }

    #[test]
    fn uri_escapes_reserved_characters() {
        assert_eq!(
            sqlite_uri(Path::new("/a/App Support/50%?#/s.db"), true),
            "file:/a/App Support/50%25%3F%23/s.db?mode=ro&immutable=1"
        );
        assert_eq!(
            sqlite_uri(Path::new("C:\\Users\\me\\s.db"), false),
            "file:/C:/Users/me/s.db?mode=ro"
        );
    }
}
