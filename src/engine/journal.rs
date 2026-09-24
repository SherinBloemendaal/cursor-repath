//! Per-command undo journal.
//!
//! Database rows are recorded once, before their first change, as full row images in a
//! streamed undo log (`rows.undo`). Files and directories are either copied aside before
//! they change, recorded as created, or recorded as renamed. Undo runs in reverse order.

use anyhow::{Context, Result, bail};
use rusqlite::types::Value;
use rusqlite::{Connection, params_from_iter};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use super::fsops::{self, copy_tree, exists, move_path, remove_path};
use super::sql::sidecars;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Table {
    Item,
    Disk,
    Headers,
}

impl Table {
    pub fn name(self) -> &'static str {
        match self {
            Self::Item => "ItemTable",
            Self::Disk => "cursorDiskKV",
            Self::Headers => "composerHeaders",
        }
    }

    pub fn key_column(self) -> &'static str {
        match self {
            Self::Item | Self::Disk => "key",
            Self::Headers => "composerId",
        }
    }

    fn tag(self) -> u8 {
        match self {
            Self::Item => 1,
            Self::Disk => 2,
            Self::Headers => 3,
        }
    }

    fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            1 => Some(Self::Item),
            2 => Some(Self::Disk),
            3 => Some(Self::Headers),
            _ => None,
        }
    }
}

pub type RowImage = Vec<(String, Value)>;

enum Step {
    Restore {
        live: PathBuf,
        copy: Option<PathBuf>,
    },
    Rename {
        from: PathBuf,
        to: PathBuf,
    },
}

pub struct Journal {
    dir: PathBuf,
    undo: Option<BufWriter<File>>,
    recorded: HashSet<(Table, String)>,
    steps: Vec<Step>,
    saved: HashSet<PathBuf>,
    created: Vec<PathBuf>,
    serial: usize,
}

impl Journal {
    pub fn create(root: &Path, label: &str) -> Result<Self> {
        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ");
        let dir = root.join(format!("{stamp}-{label}-{}", std::process::id()));
        fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create backup dir {}", dir.display()))?;
        Ok(Self {
            dir,
            undo: None,
            recorded: HashSet::new(),
            steps: Vec::new(),
            saved: HashSet::new(),
            created: Vec::new(),
            serial: 0,
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn slot(&mut self, kind: &str) -> PathBuf {
        self.serial += 1;
        self.dir.join(kind).join(self.serial.to_string())
    }

    /// Copy `live` aside before it changes. A missing path is recorded so undo removes it.
    pub fn save(&mut self, live: &Path) -> Result<()> {
        if self.created.iter().any(|root| live.starts_with(root))
            || !self.saved.insert(live.to_path_buf())
        {
            return Ok(());
        }
        let copy = if exists(live) {
            let slot = self.slot("files");
            copy_tree(live, &slot)?;
            Some(slot)
        } else {
            None
        };
        self.steps.push(Step::Restore {
            live: live.to_path_buf(),
            copy,
        });
        Ok(())
    }

    pub fn save_sqlite(&mut self, db: &Path) -> Result<()> {
        self.save(db)?;
        for sidecar in sidecars(db) {
            self.save(&sidecar)?;
        }
        Ok(())
    }

    /// Record a path this command is about to create.
    pub fn created(&mut self, path: &Path) -> Result<()> {
        if exists(path) {
            bail!("refusing to overwrite {}", path.display());
        }
        self.save(path)?;
        self.created.push(path.to_path_buf());
        Ok(())
    }

    pub fn renamed(&mut self, from: &Path, to: &Path) {
        self.steps.push(Step::Rename {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
        });
    }

    /// Move `from` to `to` and record it.
    pub fn move_path(&mut self, from: &Path, to: &Path) -> Result<()> {
        let mut moved = false;
        let outcome = fsops::move_path_with(from, to, || moved = true);
        if moved {
            self.renamed(from, to);
        }
        outcome
    }

    /// Move `live` into the journal so a removal can be undone.
    pub fn stash(&mut self, live: &Path) -> Result<()> {
        if !exists(live) {
            return Ok(());
        }
        let slot = self.slot("stash");
        self.move_path(live, &slot)
    }

    fn writer(&mut self) -> Result<&mut BufWriter<File>> {
        if self.undo.is_none() {
            let file = File::create(self.dir.join("rows.undo"))?;
            self.undo = Some(BufWriter::new(file));
        }
        Ok(self.undo.as_mut().expect("undo log was just opened"))
    }

    pub fn is_recorded(&self, table: Table, key: &str) -> bool {
        self.recorded.contains(&(table, key.to_string()))
    }

    /// Every chat whose header or composer-keyed rows this command touched.
    pub fn chat_ids(&self) -> HashSet<String> {
        self.recorded
            .iter()
            .filter_map(|(table, key)| match table {
                Table::Headers => Some(key.clone()),
                Table::Disk => crate::cursor::registry::composer_of(key).map(str::to_string),
                Table::Item => None,
            })
            .collect()
    }

    /// Record the current image of one row before its first change.
    pub fn record_row(&mut self, conn: &Connection, table: Table, key: &str) -> Result<()> {
        if self.is_recorded(table, key) {
            return Ok(());
        }
        let image = read_image(conn, table, key)?;
        self.record_image(table, key, image.as_ref())
    }

    pub fn record_image(
        &mut self,
        table: Table,
        key: &str,
        image: Option<&RowImage>,
    ) -> Result<()> {
        if !self.recorded.insert((table, key.to_string())) {
            return Ok(());
        }
        let mut buf = Vec::new();
        encode_record(&mut buf, table, key, image);
        self.writer()?.write_all(&buf)?;
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        if let Some(writer) = self.undo.as_mut() {
            writer.flush()?;
            writer.get_ref().sync_all()?;
        }
        Ok(())
    }

    /// Restore every recorded row in one transaction.
    pub fn replay_rows(&mut self, conn: &Connection) -> Result<usize> {
        self.flush()?;
        let path = self.dir.join("rows.undo");
        if !path.exists() {
            return Ok(0);
        }
        let mut reader = BufReader::new(File::open(&path)?);
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let applied = (|| -> Result<usize> {
            let mut count = 0usize;
            while let Some((table, key, image)) = decode_record(&mut reader)? {
                restore_row(conn, table, &key, image.as_ref())?;
                count += 1;
            }
            Ok(count)
        })();
        match applied {
            Ok(count) => {
                conn.execute_batch("COMMIT")?;
                Ok(count)
            }
            Err(err) => {
                conn.execute_batch("ROLLBACK").ok();
                Err(err)
            }
        }
    }

    /// Undo filesystem steps newest first. Returns one message per failed step.
    pub fn undo_files(&mut self) -> Vec<String> {
        let mut failures = Vec::new();
        while let Some(step) = self.steps.pop() {
            let outcome = match &step {
                Step::Restore { live, copy } => remove_path(live).and_then(|()| match copy {
                    Some(copy) => copy_tree(copy, live),
                    None => Ok(()),
                }),
                Step::Rename { from, to } => move_back(from, to),
            };
            if let Err(err) = outcome {
                let what = match &step {
                    Step::Restore { live, .. } => format!("restore {}", live.display()),
                    Step::Rename { from, to } => {
                        format!("move {} back to {}", to.display(), from.display())
                    }
                };
                failures.push(format!("{what}: {err:#}"));
            }
        }
        failures
    }

    pub fn discard(mut self) -> Result<()> {
        self.undo = None;
        remove_path(&self.dir)
    }
}

fn move_back(from: &Path, to: &Path) -> Result<()> {
    if !exists(to) {
        bail!("{} is missing", to.display());
    }
    remove_path(from)?;
    move_path(to, from)
}

fn read_image(conn: &Connection, table: Table, key: &str) -> Result<Option<RowImage>> {
    let sql = format!(
        "SELECT * FROM {} WHERE {} = ?1",
        table.name(),
        table.key_column()
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let names: Vec<String> = stmt
        .column_names()
        .iter()
        .map(|name| name.to_string())
        .collect();
    let mut rows = stmt.query([key])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let mut image = Vec::with_capacity(names.len());
    for (index, name) in names.into_iter().enumerate() {
        image.push((name, row.get::<_, Value>(index)?));
    }
    Ok(Some(image))
}

fn restore_row(conn: &Connection, table: Table, key: &str, image: Option<&RowImage>) -> Result<()> {
    conn.prepare_cached(&format!(
        "DELETE FROM {} WHERE {} = ?1",
        table.name(),
        table.key_column()
    ))?
    .execute([key])?;
    let Some(image) = image else {
        return Ok(());
    };
    let columns: Vec<&str> = image.iter().map(|(name, _)| name.as_str()).collect();
    let marks: Vec<String> = (1..=columns.len())
        .map(|index| format!("?{index}"))
        .collect();
    let sql = format!(
        "INSERT INTO {} ({}) VALUES ({})",
        table.name(),
        columns.join(", "),
        marks.join(", ")
    );
    conn.prepare_cached(&sql)?
        .execute(params_from_iter(image.iter().map(|(_, value)| value)))?;
    Ok(())
}

fn put_bytes(buf: &mut Vec<u8>, bytes: &[u8]) {
    buf.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    buf.extend_from_slice(bytes);
}

fn encode_record(buf: &mut Vec<u8>, table: Table, key: &str, image: Option<&RowImage>) {
    buf.push(table.tag());
    put_bytes(buf, key.as_bytes());
    match image {
        None => buf.push(0),
        Some(image) => {
            buf.push(1);
            buf.extend_from_slice(&(image.len() as u32).to_le_bytes());
            for (name, value) in image {
                put_bytes(buf, name.as_bytes());
                match value {
                    Value::Null => buf.push(0),
                    Value::Integer(number) => {
                        buf.push(1);
                        buf.extend_from_slice(&number.to_le_bytes());
                    }
                    Value::Real(number) => {
                        buf.push(2);
                        buf.extend_from_slice(&number.to_le_bytes());
                    }
                    Value::Text(text) => {
                        buf.push(3);
                        put_bytes(buf, text.as_bytes());
                    }
                    Value::Blob(bytes) => {
                        buf.push(4);
                        put_bytes(buf, bytes);
                    }
                }
            }
        }
    }
}

fn read_exact<const N: usize>(reader: &mut impl Read) -> Result<[u8; N]> {
    let mut bytes = [0u8; N];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn read_vec(reader: &mut impl Read) -> Result<Vec<u8>> {
    let len = u64::from_le_bytes(read_exact::<8>(reader)?);
    let mut bytes = vec![0u8; usize::try_from(len).context("undo record too large")?];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}

type Record = (Table, String, Option<RowImage>);

fn decode_record(reader: &mut impl Read) -> Result<Option<Record>> {
    let mut tag = [0u8; 1];
    if reader.read(&mut tag)? == 0 {
        return Ok(None);
    }
    let table = Table::from_tag(tag[0]).context("corrupt undo log: unknown table")?;
    let key = String::from_utf8(read_vec(reader)?).context("corrupt undo log: key")?;
    let present = read_exact::<1>(reader)?[0];
    if present == 0 {
        return Ok(Some((table, key, None)));
    }
    let count = u32::from_le_bytes(read_exact::<4>(reader)?);
    let mut image = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let name = String::from_utf8(read_vec(reader)?).context("corrupt undo log: column")?;
        let value = match read_exact::<1>(reader)?[0] {
            0 => Value::Null,
            1 => Value::Integer(i64::from_le_bytes(read_exact::<8>(reader)?)),
            2 => Value::Real(f64::from_le_bytes(read_exact::<8>(reader)?)),
            3 => {
                Value::Text(String::from_utf8(read_vec(reader)?).context("corrupt undo log: text")?)
            }
            4 => Value::Blob(read_vec(reader)?),
            _ => bail!("corrupt undo log: unknown value type"),
        };
        image.push((name, value));
    }
    Ok(Some((table, key, Some(image))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema(conn: &Connection) {
        conn.execute_batch(
            "CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);
             CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);
             CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, workspaceId TEXT, createdAt INTEGER, lastUpdatedAt INTEGER, isArchived INTEGER, isSubagent INTEGER, recency INTEGER, checkpointAt INTEGER, value TEXT, subagentTypeName TEXT);",
        )
        .unwrap();
    }

    fn dump(conn: &Connection) -> Vec<String> {
        let mut out = Vec::new();
        for table in ["ItemTable", "cursorDiskKV", "composerHeaders"] {
            let mut stmt = conn
                .prepare(&format!("SELECT * FROM {table} ORDER BY 1"))
                .unwrap();
            let columns = stmt.column_count();
            let mut rows = stmt.query([]).unwrap();
            while let Some(row) = rows.next().unwrap() {
                let values: Vec<String> = (0..columns)
                    .map(|index| format!("{:?}", row.get::<_, Value>(index).unwrap()))
                    .collect();
                out.push(format!("{table}:{}", values.join("|")));
            }
        }
        out
    }

    #[test]
    fn replay_restores_null_blob_text_and_absent_rows() {
        let temp = tempfile::tempdir().unwrap();
        let conn = Connection::open(temp.path().join("g.db")).unwrap();
        schema(&conn);
        conn.execute_batch(
            "INSERT INTO cursorDiskKV VALUES ('bubbleId:a:1', NULL);
             INSERT INTO cursorDiskKV VALUES ('bubbleId:a:2', X'00FF10');
             INSERT INTO cursorDiskKV VALUES ('composerData:a', '{\"x\":1}');
             INSERT INTO ItemTable VALUES ('composer.planRegistry', '/old');
             INSERT INTO composerHeaders VALUES ('a', 'ws', 1, 2, 0, 0, 3, NULL, '{}', NULL);",
        )
        .unwrap();
        let before = dump(&conn);
        let mut journal = Journal::create(&temp.path().join("backups"), "test").unwrap();
        for key in [
            "bubbleId:a:1",
            "bubbleId:a:2",
            "composerData:a",
            "composerData:new",
        ] {
            journal.record_row(&conn, Table::Disk, key).unwrap();
        }
        journal
            .record_row(&conn, Table::Item, "composer.planRegistry")
            .unwrap();
        journal.record_row(&conn, Table::Headers, "a").unwrap();
        conn.execute_batch(
            "UPDATE cursorDiskKV SET value = 'changed' WHERE key LIKE 'bubbleId:%';
             DELETE FROM cursorDiskKV WHERE key = 'composerData:a';
             INSERT INTO cursorDiskKV VALUES ('composerData:new', 'fresh');
             UPDATE ItemTable SET value = '/new';
             UPDATE composerHeaders SET workspaceId = 'other';",
        )
        .unwrap();
        journal
            .record_row(&conn, Table::Disk, "bubbleId:a:1")
            .unwrap();
        assert_ne!(dump(&conn), before);
        assert_eq!(journal.replay_rows(&conn).unwrap(), 6);
        assert_eq!(dump(&conn), before);
    }

    #[test]
    fn undo_files_reverses_saves_creates_and_moves() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::write(root.join("storage.json"), "old").unwrap();
        fs::create_dir_all(root.join("ws/a")).unwrap();
        fs::write(root.join("ws/a/workspace.json"), "a").unwrap();
        let mut journal = Journal::create(&root.join("backups"), "test").unwrap();
        journal.save(&root.join("storage.json")).unwrap();
        fs::write(root.join("storage.json"), "new").unwrap();
        journal
            .move_path(&root.join("ws/a"), &root.join("ws/b"))
            .unwrap();
        journal.save(&root.join("ws/b/workspace.json")).unwrap();
        fs::write(root.join("ws/b/workspace.json"), "b").unwrap();
        journal.created(&root.join("created")).unwrap();
        fs::create_dir_all(root.join("created/deep")).unwrap();
        fs::write(root.join("gone"), "keep").unwrap();
        journal.stash(&root.join("gone")).unwrap();
        assert!(!root.join("gone").exists());
        assert!(journal.undo_files().is_empty());
        assert_eq!(
            fs::read_to_string(root.join("storage.json")).unwrap(),
            "old"
        );
        assert_eq!(
            fs::read_to_string(root.join("ws/a/workspace.json")).unwrap(),
            "a"
        );
        assert!(!root.join("ws/b").exists());
        assert!(!root.join("created").exists());
        assert_eq!(fs::read_to_string(root.join("gone")).unwrap(), "keep");
    }

    #[test]
    fn undo_reports_failures_and_keeps_going() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::write(root.join("one"), "1").unwrap();
        let mut journal = Journal::create(&root.join("backups"), "test").unwrap();
        journal.save(&root.join("one")).unwrap();
        journal.renamed(&root.join("missing-from"), &root.join("missing-to"));
        fs::write(root.join("one"), "2").unwrap();
        let failures = journal.undo_files();
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("missing-to"), "{failures:?}");
        assert_eq!(fs::read_to_string(root.join("one")).unwrap(), "1");
    }
}
