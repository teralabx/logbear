//! The central registry at `~/.runlog/index.db`.
//!
//! Holds only which projects exist and a per-run summary, so the app can open without
//! touching every project database. It is the only place a cross-project question can be
//! answered, and agent SQL never reaches it: those queries are bound to a single project
//! database with `ATTACH` denied.

use crate::error::Result;
use crate::model::{Project, ProjectId};
use crate::paths;
use rusqlite::{Connection, OptionalExtension};
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct IndexDb {
    conn: Arc<Mutex<Connection>>,
}

impl IndexDb {
    pub fn open(path: &Path) -> Result<IndexDb> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| crate::Error::io(parent, e))?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;",
        )?;
        conn.execute_batch(super::schema::INDEX_SCHEMA)?;
        Ok(IndexDb {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn open_default() -> Result<IndexDb> {
        paths::ensure_home().map_err(|e| crate::Error::io(paths::home(), e))?;
        IndexDb::open(&paths::index_db())
    }

    fn with<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| crate::Error::other("index mutex poisoned"))?;
        f(&conn)
    }

    /// Registers a project, or returns the existing row for that root.
    pub fn register(&self, root: &Path, name: &str, db_path: &Path) -> Result<Project> {
        let now = crate::now_micros();
        let root_s = root.to_string_lossy().into_owned();
        let db_s = db_path.to_string_lossy().into_owned();
        self.with(|conn| {
            conn.execute(
                "INSERT INTO projects (name, root, db_path, created_at, last_seen_at)
                 VALUES (?1, ?2, ?3, ?4, ?4)
                 ON CONFLICT(root) DO UPDATE SET last_seen_at = excluded.last_seen_at,
                                                 name = excluded.name",
                rusqlite::params![name, root_s, db_s, now],
            )?;
            row_by_root(conn, &root_s)?.ok_or_else(|| crate::Error::NoSuchProject(root_s.clone()))
        })
    }

    /// Resolves a directory to a project.
    ///
    /// Matches by containment in either direction, so a caller at the repo root finds a
    /// project registered in `apps/api` and vice versa. The longest matching root wins,
    /// which keeps nested repos distinct.
    pub fn resolve_path(&self, dir: &Path) -> Result<Option<Project>> {
        let git_root = crate::project::project_root(dir);
        let candidates = self.list()?;

        // Exact root first, the common case.
        if let Some(p) = candidates.iter().find(|p| Path::new(&p.root) == git_root) {
            return Ok(Some(p.clone()));
        }
        // Then the deepest registered root that contains this directory, or is contained
        // by it.
        let mut matches: Vec<&Project> = candidates
            .iter()
            .filter(|p| {
                let root = Path::new(&p.root);
                crate::project::contains(root, &git_root) || crate::project::contains(&git_root, root)
            })
            .collect();
        matches.sort_by_key(|p| std::cmp::Reverse(p.root.len()));
        Ok(matches.first().map(|p| (*p).clone()))
    }

    pub fn get(&self, id: ProjectId) -> Result<Option<Project>> {
        self.with(|conn| {
            conn.query_row(SELECT_PROJECT_BY_ID, [id], map_project)
                .optional()
                .map_err(Into::into)
        })
    }

    pub fn list(&self) -> Result<Vec<Project>> {
        self.with(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, name, root, db_path, created_at, last_seen_at, retention_bytes,
                        push_enabled, notify_enabled
                   FROM projects ORDER BY last_seen_at DESC",
            )?;
            let rows = stmt.query_map([], map_project)?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    pub fn touch(&self, id: ProjectId) -> Result<()> {
        self.with(|conn| {
            conn.execute(
                "UPDATE projects SET last_seen_at = ?2 WHERE id = ?1",
                rusqlite::params![id, crate::now_micros()],
            )?;
            Ok(())
        })
    }

    pub fn update_config(
        &self,
        id: ProjectId,
        retention_bytes: Option<Option<i64>>,
        push_enabled: Option<bool>,
        notify_enabled: Option<bool>,
    ) -> Result<()> {
        self.with(|conn| {
            if let Some(v) = retention_bytes {
                conn.execute(
                    "UPDATE projects SET retention_bytes = ?2 WHERE id = ?1",
                    rusqlite::params![id, v],
                )?;
            }
            if let Some(v) = push_enabled {
                conn.execute(
                    "UPDATE projects SET push_enabled = ?2 WHERE id = ?1",
                    rusqlite::params![id, v as i64],
                )?;
            }
            if let Some(v) = notify_enabled {
                conn.execute(
                    "UPDATE projects SET notify_enabled = ?2 WHERE id = ?1",
                    rusqlite::params![id, v as i64],
                )?;
            }
            Ok(())
        })
    }

    pub fn record_run_start(
        &self,
        project_id: ProjectId,
        run_id: i64,
        command: &str,
        started_at: i64,
    ) -> Result<()> {
        self.with(|conn| {
            conn.execute(
                "INSERT INTO run_summaries (project_id, run_id, command, started_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(project_id, run_id) DO UPDATE SET started_at = excluded.started_at",
                rusqlite::params![project_id, run_id, command, started_at],
            )?;
            conn.execute(
                "UPDATE projects SET last_seen_at = ?2 WHERE id = ?1",
                rusqlite::params![project_id, started_at],
            )?;
            Ok(())
        })
    }

    pub fn record_run_end(
        &self,
        project_id: ProjectId,
        run_id: i64,
        ended_at: i64,
        exit_code: Option<i32>,
        n_error: i64,
    ) -> Result<()> {
        self.with(|conn| {
            conn.execute(
                "UPDATE run_summaries SET ended_at = ?3, exit_code = ?4, n_error = ?5
                  WHERE project_id = ?1 AND run_id = ?2",
                rusqlite::params![project_id, run_id, ended_at, exit_code, n_error],
            )?;
            Ok(())
        })
    }

    pub fn forget_run(&self, project_id: ProjectId, run_id: i64) -> Result<()> {
        self.with(|conn| {
            conn.execute(
                "DELETE FROM run_summaries WHERE project_id = ?1 AND run_id = ?2",
                rusqlite::params![project_id, run_id],
            )?;
            Ok(())
        })
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        self.with(|conn| {
            conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
                .optional()
                .map_err(Into::into)
        })
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.with(|conn| {
            conn.execute(
                "INSERT INTO meta(key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                (key, value),
            )?;
            Ok(())
        })
    }
}

const SELECT_PROJECT_BY_ID: &str = "SELECT id, name, root, db_path, created_at, last_seen_at,
                                           retention_bytes, push_enabled, notify_enabled
                                      FROM projects WHERE id = ?1";

fn row_by_root(conn: &Connection, root: &str) -> Result<Option<Project>> {
    conn.query_row(
        "SELECT id, name, root, db_path, created_at, last_seen_at, retention_bytes,
                push_enabled, notify_enabled
           FROM projects WHERE root = ?1",
        [root],
        map_project,
    )
    .optional()
    .map_err(Into::into)
}

fn map_project(r: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: r.get(0)?,
        name: r.get(1)?,
        root: r.get(2)?,
        db_path: r.get(3)?,
        created_at: r.get(4)?,
        last_seen_at: r.get(5)?,
        retention_bytes: r.get(6)?,
        push_enabled: r.get::<_, i64>(7)? != 0,
        notify_enabled: r.get::<_, i64>(8)? != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registering_the_same_root_twice_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let idx = IndexDb::open(&tmp.path().join("index.db")).unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let a = idx.register(&root, "proj", &tmp.path().join("a.db")).unwrap();
        let b = idx.register(&root, "proj", &tmp.path().join("a.db")).unwrap();
        assert_eq!(a.id, b.id);
        assert_eq!(idx.list().unwrap().len(), 1);
    }

    #[test]
    fn a_subdirectory_resolves_to_its_project() {
        let tmp = tempfile::tempdir().unwrap();
        let idx = IndexDb::open(&tmp.path().join("index.db")).unwrap();
        let root = tmp.path().join("repo");
        let nested = root.join("apps/api");
        std::fs::create_dir_all(&nested).unwrap();
        let p = idx.register(&root, "repo", &tmp.path().join("a.db")).unwrap();

        let found = idx.resolve_path(&nested).unwrap().unwrap();
        assert_eq!(found.id, p.id, "monorepo subdirectory must find the repo root");
    }

    #[test]
    fn the_deepest_registered_root_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let idx = IndexDb::open(&tmp.path().join("index.db")).unwrap();
        let outer = tmp.path().join("outer");
        let inner = outer.join("vendor/inner");
        std::fs::create_dir_all(&inner).unwrap();
        idx.register(&outer, "outer", &tmp.path().join("o.db")).unwrap();
        let i = idx.register(&inner, "inner", &tmp.path().join("i.db")).unwrap();

        assert_eq!(idx.resolve_path(&inner).unwrap().unwrap().id, i.id);
    }

    #[test]
    fn an_unregistered_directory_resolves_to_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let idx = IndexDb::open(&tmp.path().join("index.db")).unwrap();
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        assert!(idx.resolve_path(&elsewhere).unwrap().is_none());
    }

    #[test]
    fn per_project_config_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let idx = IndexDb::open(&tmp.path().join("index.db")).unwrap();
        let root = tmp.path().join("p");
        std::fs::create_dir_all(&root).unwrap();
        let p = idx.register(&root, "p", &tmp.path().join("p.db")).unwrap();
        assert!(p.push_enabled);

        idx.update_config(p.id, Some(Some(2048)), Some(false), None).unwrap();
        let after = idx.get(p.id).unwrap().unwrap();
        assert_eq!(after.retention_bytes, Some(2048));
        assert!(!after.push_enabled);
        assert!(after.notify_enabled, "untouched fields must not change");
    }
}
