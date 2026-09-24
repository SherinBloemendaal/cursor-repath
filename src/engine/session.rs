//! One write command: journal, a single global-DB transaction, verify, and rollback.

use anyhow::{Result, anyhow};
use rusqlite::Connection;

use super::Runtime;
use super::db::Writer;
use super::journal::Journal;
use super::sql;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tx {
    None,
    Open,
    Committed,
}

pub struct Session<'r> {
    pub rt: &'r Runtime,
    pub journal: Journal,
    global: Option<Connection>,
    tx: Tx,
}

impl<'r> Session<'r> {
    fn begin(rt: &'r Runtime, label: &str) -> Result<Self> {
        rt.check()?;
        let journal = Journal::create(&rt.layout.backup_root(), label)?;
        Ok(Self {
            rt,
            journal,
            global: None,
            tx: Tx::None,
        })
    }

    pub fn step(&self) -> Result<()> {
        self.rt.check()
    }

    pub fn has_global(&self) -> bool {
        self.rt.layout.global_db().exists()
    }

    fn ensure_tx(&mut self) -> Result<()> {
        if self.global.is_none() {
            let conn = sql::open_rw(&self.rt.layout.global_db())?;
            conn.execute_batch("BEGIN IMMEDIATE")?;
            self.global = Some(conn);
            self.tx = Tx::Open;
        }
        Ok(())
    }

    /// Writer on the global database inside this command's transaction.
    pub fn db(&mut self) -> Result<Writer<'_>> {
        self.ensure_tx()?;
        let conn = self.global.as_ref().expect("transaction was just opened");
        Ok(Writer::new(conn, &mut self.journal))
    }

    /// Connection that sees this command's uncommitted writes.
    pub fn conn(&mut self) -> Result<&Connection> {
        self.ensure_tx()?;
        Ok(self.global.as_ref().expect("transaction was just opened"))
    }

    fn commit(&mut self) -> Result<()> {
        self.journal.flush()?;
        if self.tx == Tx::Open
            && let Some(conn) = &self.global
        {
            conn.execute_batch("COMMIT")?;
            self.tx = Tx::Committed;
        }
        Ok(())
    }

    fn abort(mut self, cause: anyhow::Error) -> anyhow::Error {
        let mut failures = Vec::new();
        match (self.tx, &self.global) {
            (Tx::Open, Some(conn)) => {
                if let Err(err) = conn.execute_batch("ROLLBACK") {
                    failures.push(format!("roll back the global database: {err}"));
                }
            }
            (Tx::Committed, Some(conn)) => {
                if let Err(err) = self.journal.replay_rows(conn) {
                    failures.push(format!("restore global database rows: {err:#}"));
                }
            }
            _ => {}
        }
        self.global = None;
        failures.extend(self.journal.undo_files());
        let dir = self.journal.dir().to_path_buf();
        if failures.is_empty() {
            match self.journal.discard() {
                Ok(()) => anyhow!("{cause:#}\nAll changes were rolled back."),
                Err(err) => anyhow!(
                    "{cause:#}\nAll changes were rolled back, but the backup at {} could not be removed: {err:#}",
                    dir.display()
                ),
            }
        } else {
            anyhow!(
                "{cause:#}\nRollback incomplete. Backup kept at {}:\n- {}",
                dir.display(),
                failures.join("\n- ")
            )
        }
    }

    fn close(mut self) {
        self.global = None;
        let dir = self.journal.dir().to_path_buf();
        if let Err(err) = self.journal.discard() {
            crate::ui::warn(&format!(
                "backup at {} could not be removed: {err:#}",
                dir.display()
            ));
        }
    }
}

/// Run `body`, commit, run `verify`, and roll everything back if any of it fails or
/// Cursor starts before the command returns.
pub fn run<'r, T>(
    rt: &'r Runtime,
    label: &str,
    body: impl FnOnce(&mut Session<'r>) -> Result<T>,
    verify: impl FnOnce(&mut Session<'r>, &T) -> Result<()>,
) -> Result<T> {
    let mut session = Session::begin(rt, label)?;
    let outcome = (|| {
        let value = body(&mut session)?;
        session.step()?;
        session.commit()?;
        verify(&mut session, &value)?;
        session.step()?;
        Ok(value)
    })();
    match outcome {
        Ok(value) => {
            session.close();
            Ok(value)
        }
        Err(err) => Err(session.abort(err)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::journal::Table;
    use crate::engine::{FixedProbe, Layout};
    use rusqlite::types::Value;
    use std::sync::Arc;

    fn runtime(root: &std::path::Path) -> Runtime {
        let layout = Layout {
            cursor_root: root.join("Cursor"),
            projects_dir: root.join("projects"),
            crepath_home: root.join("crepath"),
        };
        std::fs::create_dir_all(layout.global_storage()).unwrap();
        let conn = Connection::open(layout.global_db()).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.execute_batch(crate::cursor::registry::GLOBAL_SCHEMA)
            .unwrap();
        conn.execute(
            "INSERT INTO cursorDiskKV (key, value) VALUES ('k', 'before')",
            [],
        )
        .unwrap();
        Runtime {
            layout,
            dry_run: false,
            yes: true,
            profile: None,
            probe: Arc::new(FixedProbe(false)),
            quiet: true,
        }
    }

    fn value(rt: &Runtime) -> Value {
        Connection::open(rt.layout.global_db())
            .unwrap()
            .query_row(
                "SELECT value FROM cursorDiskKV WHERE key = 'k'",
                [],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[test]
    fn failed_verify_rolls_back_committed_rows_and_files() {
        let temp = tempfile::tempdir().unwrap();
        let rt = runtime(temp.path());
        let file = temp.path().join("storage.json");
        std::fs::write(&file, "before").unwrap();
        let err = run(
            &rt,
            "test",
            |session| {
                session.journal.save(&file)?;
                std::fs::write(&file, "after")?;
                session
                    .db()?
                    .put(Table::Disk, "k", &Value::Text("after".into()))?;
                session
                    .db()?
                    .put(Table::Disk, "new", &Value::Text("x".into()))?;
                Ok(())
            },
            |session, ()| {
                let now: String = session.conn()?.query_row(
                    "SELECT value FROM cursorDiskKV WHERE key = 'k'",
                    [],
                    |row| row.get(0),
                )?;
                assert_eq!(now, "after");
                anyhow::bail!("verify failed: forced")
            },
        )
        .unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("verify failed: forced"), "{message}");
        assert!(
            message.contains("All changes were rolled back"),
            "{message}"
        );
        assert_eq!(value(&rt), Value::Text("before".into()));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "before");
        let count: i64 = Connection::open(rt.layout.global_db())
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM cursorDiskKV WHERE key = 'new'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
        assert_eq!(
            std::fs::read_dir(rt.layout.backup_root()).unwrap().count(),
            0
        );
    }

    #[test]
    fn failed_body_rolls_back_the_open_transaction() {
        let temp = tempfile::tempdir().unwrap();
        let rt = runtime(temp.path());
        let err = run(
            &rt,
            "test",
            |session| {
                session
                    .db()?
                    .put(Table::Disk, "k", &Value::Text("after".into()))?;
                anyhow::bail!("step failed")
            },
            |_, ()| Ok(()),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("rolled back"));
        assert_eq!(value(&rt), Value::Text("before".into()));
    }

    #[test]
    fn success_prunes_the_backup() {
        let temp = tempfile::tempdir().unwrap();
        let rt = runtime(temp.path());
        run(
            &rt,
            "test",
            |session| {
                session
                    .db()?
                    .put(Table::Disk, "k", &Value::Text("after".into()))
            },
            |_, ()| Ok(()),
        )
        .unwrap();
        assert_eq!(value(&rt), Value::Text("after".into()));
        assert_eq!(
            std::fs::read_dir(rt.layout.backup_root()).unwrap().count(),
            0
        );
    }
}
