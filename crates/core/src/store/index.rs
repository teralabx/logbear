//! The central registry at `~/.logbear/index.db`.
//!
//! Holds only which projects exist and a per-run summary, so the app can open without
//! touching every project database. It is the only place a cross-project question can be
//! answered, and agent SQL never reaches it: those queries are bound to a single project
//! database with `ATTACH` denied.

use crate::error::Result;
use crate::model::{Project, ProjectId};
use crate::paths;
use crate::store::{get_meta, quarantine, schema};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

/// Summaries restored per project when the index is rebuilt.
const REBUILT_SUMMARIES: i64 = 5_000;

#[derive(Clone)]
pub struct IndexDb {
    conn: Arc<Mutex<Connection>>,
}

impl IndexDb {
    pub fn open(path: &Path) -> Result<IndexDb> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| crate::Error::io(parent, e))?;
        }
        paths::create_private(path).map_err(|e| crate::Error::io(path, e))?;
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;",
        )?;
        conn.execute_batch(schema::INDEX_SCHEMA)?;
        // Small enough to check on every open, and everything depends on it, which is the
        // opposite of the project databases: those can be gigabytes and fail one at a time.
        let verdict: String = conn.query_row("PRAGMA quick_check(1)", [], |r| r.get(0))?;
        if verdict != "ok" {
            return Err(crate::Error::Corrupt(path.to_path_buf(), verdict));
        }
        Ok(IndexDb {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn open_default() -> Result<IndexDb> {
        paths::ensure_home().map_err(|e| crate::Error::io(paths::home(), e))?;
        IndexDb::open_or_rebuild(&paths::index_db(), &paths::home().join("projects"))
    }

    /// Opens the index, recovering it from the project databases if it is damaged or gone.
    ///
    /// The daemon cannot start without an index, so a damaged one otherwise means a process
    /// supervisor restarting it forever while capture goes dark. Nothing here is the only
    /// copy: projects and runs are all in the project databases. What cannot come back is
    /// any global setting kept in `meta`, and per-project settings on a database no daemon
    /// has stamped.
    pub fn open_or_rebuild(path: &Path, projects: &Path) -> Result<IndexDb> {
        let index = match IndexDb::open(path) {
            Err(e) if e.is_corrupt() => {
                let aside = quarantine(path)?;
                eprintln!("logbeard: {e}; moved it to {} and rebuilding", aside.display());
                IndexDb::open(path)?
            }
            other => other?,
        };
        if index.list()?.is_empty() {
            let restored = index.rebuild_from(projects)?;
            if restored > 0 {
                eprintln!("logbeard: restored {restored} project(s) to the index");
            }
        }
        Ok(index)
    }

    /// Registers every project database in `dir` that can be read, returning how many.
    /// One that cannot be read is skipped: it is not this index's to judge.
    pub fn rebuild_from(&self, dir: &Path) -> Result<usize> {
        let Ok(files) = std::fs::read_dir(dir) else {
            return Ok(0);
        };
        let mut paths: Vec<PathBuf> = files
            .filter_map(|f| f.ok())
            .map(|f| f.path())
            .filter(|p| p.extension().is_some_and(|x| x == "db"))
            .collect();
        paths.sort();
        let mut restored = 0;
        for path in paths {
            match self.restore_project(&path) {
                Ok(true) => restored += 1,
                Ok(false) => {}
                Err(e) => eprintln!("logbeard: could not restore {}: {e}", path.display()),
            }
        }
        Ok(restored)
    }

    fn restore_project(&self, path: &Path) -> Result<bool> {
        let source = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        source.execute_batch("PRAGMA busy_timeout = 2000;")?;
        let Some(project) = identity_of(&source, path)? else {
            return Ok(false);
        };
        let db_path = path.to_string_lossy().into_owned();

        let mut stmt = source.prepare(
            "SELECT r.id, r.command, r.started_at, r.ended_at, r.exit_code,
                    COALESCE(c.n_error, 0)
               FROM runs r LEFT JOIN run_counters c ON c.run_id = r.id
              ORDER BY r.started_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([REBUILT_SUMMARIES], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, Option<i32>>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })?;
        let summaries = rows.collect::<std::result::Result<Vec<_>, _>>()?;

        let mut conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let tx = conn.transaction()?;
        // The id is the one its runs carry; a clash with a project restored already (a copied
        // file, say) gives this one a new id rather than merging two histories.
        let taken: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM projects WHERE id = ?1)",
            [project.id],
            |r| r.get(0),
        )?;
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO projects (id, name, root, db_path, created_at, last_seen_at,
                                             retention_bytes, push_enabled, notify_enabled)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                if taken { None } else { Some(project.id) },
                project.name,
                project.root,
                db_path,
                project.created_at,
                project.last_seen_at,
                project.retention_bytes,
                project.push_enabled as i64,
                project.notify_enabled as i64,
            ],
        )?;
        if inserted == 0 {
            return Ok(false);
        }
        let id = tx.last_insert_rowid();
        for (run_id, command, started_at, ended_at, exit_code, n_error) in summaries {
            tx.execute(
                "INSERT OR REPLACE INTO run_summaries
                   (project_id, run_id, command, started_at, ended_at, exit_code, n_error)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![id, run_id, command, started_at, ended_at, exit_code, n_error],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }

    /// Projects with a run the index believes is still open. After a crash those are the
    /// only ones startup needs to touch.
    pub fn projects_with_open_runs(&self) -> Result<Vec<ProjectId>> {
        self.with(|conn| {
            let mut stmt =
                conn.prepare("SELECT DISTINCT project_id FROM run_summaries WHERE ended_at IS NULL")?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    /// How many of a project's runs the index believes are still open.
    pub fn open_run_count(&self, project_id: ProjectId) -> Result<usize> {
        self.with(|conn| {
            let n: i64 = conn.query_row(
                "SELECT count(*) FROM run_summaries WHERE project_id = ?1 AND ended_at IS NULL",
                [project_id],
                |r| r.get(0),
            )?;
            Ok(n as usize)
        })
    }

    /// Drops a project's summaries, for when its database was replaced by an empty one.
    pub fn forget_runs(&self, project_id: ProjectId) -> Result<()> {
        self.with(|conn| {
            conn.execute("DELETE FROM run_summaries WHERE project_id = ?1", [project_id])?;
            Ok(())
        })
    }

    /// Folds the index's WAL into the file. It writes too little to reach SQLite's own
    /// threshold, which is why a healthy `index.db` could sit beside a far larger log.
    pub fn checkpoint(&self) -> Result<()> {
        self.with(|conn| {
            crate::store::wal::checkpoint(conn, "TRUNCATE")?;
            Ok(())
        })
    }

    fn with<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
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

/// Works out a project's row from its own database.
///
/// A database stamped by a daemon says so outright. Otherwise the root comes from the newest
/// run's directory: the file is named for a hash of the root, so the ancestor that hashes
/// to the file name is the root, even if the directory has since gone.
fn identity_of(source: &Connection, path: &Path) -> Result<Option<Project>> {
    if let Some(json) = get_meta(source, schema::PROJECT_IDENTITY) {
        if let Ok(project) = serde_json::from_str(&json) {
            return Ok(Some(project));
        }
    }
    let newest: Option<(ProjectId, String, i64, i64)> = source
        .query_row(
            "SELECT project_id, cwd, started_at,
                    (SELECT min(started_at) FROM runs)
               FROM runs ORDER BY started_at DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(3)?, r.get(2)?)),
        )
        .optional()?;
    let Some((id, cwd, created_at, last_seen_at)) = newest else {
        return Ok(None);
    };
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned());
    let cwd = PathBuf::from(cwd);
    let root = cwd
        .ancestors()
        .find(|dir| Some(paths::project_hash(dir)) == stem)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| crate::project::project_root(&cwd));
    Ok(Some(Project {
        id,
        name: crate::project::project_name(&root),
        root: root.to_string_lossy().into_owned(),
        db_path: path.to_string_lossy().into_owned(),
        created_at,
        last_seen_at,
        retention_bytes: None,
        push_enabled: true,
        notify_enabled: true,
    }))
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

    use crate::model::RunStamp;
    use crate::redact::Redactor;
    use crate::store::{Db, Writer};

    /// A home with one project that has one finished run, returned with the project.
    fn populated(home: &Path) -> Project {
        let projects = home.join("projects");
        let root = home.join("repo");
        std::fs::create_dir_all(root.join("apps/api")).unwrap();
        let db_path = projects.join(format!("{}.db", paths::project_hash(&root)));
        let idx = IndexDb::open(&home.join("index.db")).unwrap();
        let project = idx.register(&root, "repo", &db_path).unwrap();
        let db = Db::open(&db_path, project.id).unwrap();
        let stamp = RunStamp {
            command: vec!["mix".into()],
            cwd: root.join("apps/api").to_string_lossy().into_owned(),
            ..Default::default()
        };
        let mut w = Writer::start(&db, &idx, &stamp, std::sync::Arc::new(Redactor::disabled())).unwrap();
        w.push(b"[error] boom\n", crate::now_micros()).unwrap();
        w.finish(&idx, Some(1), None).unwrap();
        project
    }

    fn summaries(idx: &IndexDb) -> i64 {
        idx.with(|c| Ok(c.query_row("SELECT count(*) FROM run_summaries", [], |r| r.get(0))?))
            .unwrap()
    }

    #[test]
    fn a_damaged_index_is_moved_aside_and_rebuilt_from_the_project_databases() {
        let tmp = tempfile::tempdir().unwrap();
        let project = populated(tmp.path());
        let path = tmp.path().join("index.db");
        std::fs::remove_file(format!("{}-wal", path.display())).ok();
        std::fs::remove_file(format!("{}-shm", path.display())).ok();
        std::fs::write(&path, b"this is not a database, and never was one").unwrap();

        let idx = IndexDb::open_or_rebuild(&path, &tmp.path().join("projects")).unwrap();

        let restored = idx.list().unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].id, project.id, "its runs carry this id");
        assert_eq!(restored[0].db_path, project.db_path);
        assert!(restored[0].root.ends_with("repo"), "{}", restored[0].root);
        assert_eq!(summaries(&idx), 1);

        let kept = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter(|f| f.as_ref().unwrap().file_name().to_string_lossy().contains(".corrupt-"))
            .count();
        assert_eq!(kept, 1, "the damaged file is evidence, not rubbish");
    }

    #[test]
    fn a_stamped_project_gets_its_settings_back_too() {
        let tmp = tempfile::tempdir().unwrap();
        let mut project = populated(tmp.path());
        project.retention_bytes = Some(4096);
        project.notify_enabled = false;
        Db::open(Path::new(&project.db_path), project.id)
            .unwrap()
            .stamp_identity(&project)
            .unwrap();

        let idx = IndexDb::open(&tmp.path().join("fresh.db")).unwrap();
        assert_eq!(idx.rebuild_from(&tmp.path().join("projects")).unwrap(), 1);
        let back = idx.get(project.id).unwrap().unwrap();
        assert_eq!(back.retention_bytes, Some(4096));
        assert!(!back.notify_enabled);
    }

    #[test]
    fn an_index_that_went_missing_is_rebuilt_as_well() {
        let tmp = tempfile::tempdir().unwrap();
        populated(tmp.path());
        let idx = IndexDb::open_or_rebuild(&tmp.path().join("new.db"), &tmp.path().join("projects"))
            .unwrap();
        assert_eq!(idx.list().unwrap().len(), 1);
    }

    #[test]
    fn a_project_database_that_cannot_be_read_does_not_stop_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        populated(tmp.path());
        std::fs::write(tmp.path().join("projects/0000.db"), b"garbage in a project slot").unwrap();
        let idx = IndexDb::open(&tmp.path().join("new.db")).unwrap();
        assert_eq!(idx.rebuild_from(&tmp.path().join("projects")).unwrap(), 1);
    }

    #[test]
    fn open_runs_name_the_projects_startup_must_look_at() {
        let tmp = tempfile::tempdir().unwrap();
        let idx = IndexDb::open(&tmp.path().join("index.db")).unwrap();
        idx.record_run_start(1, 1, "a", 1).unwrap();
        idx.record_run_end(1, 1, 2, Some(0), 0).unwrap();
        idx.record_run_start(2, 1, "b", 1).unwrap();
        assert_eq!(idx.projects_with_open_runs().unwrap(), vec![2]);
        assert_eq!(idx.open_run_count(2).unwrap(), 1);
    }

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
