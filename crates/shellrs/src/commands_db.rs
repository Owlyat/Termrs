//! SQLite-backed command database: saved shell commands with comments.
//!
//! Rows carry the command text (which may contain `{name}` placeholders), a
//! free-text comment describing what it does, optional tags and a use counter
//! (so the picker can rank frequently used commands first). The database is a
//! single `commands.db` file next to the config unless `[commands] db` points
//! somewhere else.

use std::path::{Path, PathBuf};

use rusqlite::Connection;

/// One saved command.
#[derive(Debug, Clone)]
pub struct SavedCommand {
    pub id: i64,
    pub command: String,
    pub comment: String,
    pub tags: String,
    pub uses: i64,
}

/// Handle to the SQLite command store.
pub struct CommandDb {
    conn: Connection,
    path: PathBuf,
}

impl CommandDb {
    /// Open (creating the file/schema when missing). The parent directory is
    /// created on demand.
    pub fn open(path: &Path) -> Result<Self, String> {
        if let Some(dir) = path.parent()
            && !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
            }
        let conn =
            Connection::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS commands (
                 id         INTEGER PRIMARY KEY AUTOINCREMENT,
                 command    TEXT NOT NULL,
                 comment    TEXT NOT NULL DEFAULT '',
                 tags       TEXT NOT NULL DEFAULT '',
                 uses       INTEGER NOT NULL DEFAULT 0,
                 created_at TEXT NOT NULL DEFAULT (datetime('now')),
                 last_used  TEXT
             );",
        )
        .map_err(|e| e.to_string())?;
        Ok(Self {
            conn,
            path: path.to_path_buf(),
        })
    }

    /// Path backing this store.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Insert a command; returns its new id.
    pub fn add(&self, command: &str, comment: &str, tags: &str) -> Result<i64, String> {
        self.conn
            .execute(
                "INSERT INTO commands (command, comment, tags) VALUES (?1, ?2, ?3)",
                rusqlite::params![command, comment, tags],
            )
            .map_err(|e| e.to_string())?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Rewrite an existing row's command + comment (tags and use counter
    /// kept). Errors when no row with `id` exists.
    pub fn update(&self, id: i64, command: &str, comment: &str) -> Result<(), String> {
        let rows = self
            .conn
            .execute(
                "UPDATE commands SET command = ?1, comment = ?2 WHERE id = ?3",
                rusqlite::params![command, comment, id],
            )
            .map_err(|e| e.to_string())?;
        if rows == 0 {
            return Err("no such command".into());
        }
        Ok(())
    }

    /// Every saved command, most-used first (then newest).
    pub fn all(&self) -> Result<Vec<SavedCommand>, String> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, command, comment, tags, uses FROM commands \
                 ORDER BY uses DESC, id DESC",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| {
                Ok(SavedCommand {
                    id: r.get(0)?,
                    command: r.get(1)?,
                    comment: r.get(2)?,
                    tags: r.get(3)?,
                    uses: r.get(4)?,
                })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
    }

    /// Delete a command by id.
    pub fn delete(&self, id: i64) -> Result<(), String> {
        self.conn
            .execute("DELETE FROM commands WHERE id = ?1", [id])
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Bump the use counter (called when a command is run).
    pub fn mark_used(&self, id: i64) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE commands SET uses = uses + 1, last_used = datetime('now') WHERE id = ?1",
                [id],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// Unique `{name}` placeholders in `template`, in first-seen order.
/// A name is `[A-Za-z0-9_-]+`; anything else in braces is left untouched.
pub fn placeholders(template: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else { break };
        let name = &after[..close];
        if !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            && !out.iter().any(|n| n == name)
        {
            out.push(name.to_string());
        }
        rest = &after[close + 1..];
    }
    out
}

/// Fill `{name}` placeholders with `values`, quoting values that contain
/// spaces unless the placeholder already sits inside quotes.
pub fn fill(template: &str, values: &[(String, String)]) -> String {
    let mut out = template.to_string();
    for (name, value) in values {
        out = crate::ai::substitute_placeholder(&out, &format!("{{{name}}}"), value).0;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "shellrs-db-{tag}-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        p
    }

    #[test]
    fn add_query_delete_roundtrip() {
        let path = temp_db("roundtrip");
        let db = CommandDb::open(&path).expect("open");
        let id = db.add("git status", "show the working tree", "git").unwrap();
        assert!(id > 0);
        let all = db.all().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].command, "git status");
        assert_eq!(all[0].comment, "show the working tree");
        assert_eq!(all[0].uses, 0);

        db.mark_used(id).unwrap();
        assert_eq!(db.all().unwrap()[0].uses, 1);

        db.delete(id).unwrap();
        assert!(db.all().unwrap().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn placeholders_are_unique_and_ordered() {
        assert_eq!(placeholders("cp {src} {dst} {src}"), vec!["src", "dst"]);
        assert_eq!(placeholders("echo {missing_brace"), Vec::<String>::new());
        assert_eq!(placeholders("echo {a b}"), Vec::<String>::new());
        assert_eq!(placeholders("no placeholders here"), Vec::<String>::new());
    }

    #[test]
    fn fill_quotes_only_when_needed() {
        let v = vec![("file".to_string(), "a b.txt".to_string())];
        assert_eq!(fill("rm {file}", &v), "rm \"a b.txt\"");
        assert_eq!(fill("rm \"{file}\"", &v), "rm \"a b.txt\"");
        let one = vec![("file".to_string(), "notes.txt".to_string())];
        assert_eq!(fill("cat {file}", &one), "cat notes.txt");
    }
}
