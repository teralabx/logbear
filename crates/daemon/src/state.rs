//! Daemon state: the project handles, the live runs, and the fanout.
//!
//! Threads, not an async runtime. The work is a local unix socket in front of a local
//! SQLite database — there is no IO concurrency problem here worth a runtime, and a
//! thread per connection keeps every code path readable top to bottom.

use crossbeam_channel::{Receiver, Sender};
use runlog_core::model::*;
use runlog_core::notify::{Candidate, Decision, Notifier};
use runlog_core::paths;
use runlog_core::proto::*;
use runlog_core::redact::Redactor;
use runlog_core::store::{index::IndexDb, read, retention, Db, Writer};
use runlog_core::{Error, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

/// Everything about one project the daemon holds open.
pub struct ProjectHandle {
    pub project: Project,
    pub db: Db,
    pub notifier: Mutex<Notifier>,
}

/// A live run and the writer feeding it.
pub struct LiveRun {
    pub run_id: RunId,
    pub project_id: ProjectId,
    pub command: String,
    pub writer: Mutex<Writer>,
}

/// One connected subscriber. The channel is bounded so a stalled UI cannot make the
/// daemon buffer without limit; when it fills we drop the subscriber rather than the
/// capture path, because losing a live view is recoverable and losing a run is not.
pub struct Subscriber {
    pub id: u64,
    pub project: Option<ProjectId>,
    pub tx: Sender<Event>,
}

/// A blocked `runlog_wait` call.
struct Waiter {
    id: u64,
    project_id: ProjectId,
    level: Option<Level>,
    after_entry: Option<EntryId>,
    tx: Sender<Vec<EntryView>>,
}

pub struct Daemon {
    pub index: IndexDb,
    pub settings: RwLock<Settings>,
    pub started_at: i64,
    pub shutdown: AtomicBool,
    projects: Mutex<HashMap<ProjectId, Arc<ProjectHandle>>>,
    runs: Mutex<HashMap<RunId, Arc<LiveRun>>>,
    subscribers: Mutex<Vec<Subscriber>>,
    waiters: Mutex<Vec<Waiter>>,
    redactor: RwLock<Arc<Redactor>>,
    next_id: AtomicU64,
}

impl Daemon {
    pub fn open() -> Result<Arc<Daemon>> {
        paths::ensure_home().map_err(|e| Error::io(paths::home(), e))?;
        let index = IndexDb::open_default()?;
        let settings = load_settings();
        let redactor = Arc::new(Redactor::new(
            settings.redaction_enabled,
            &settings.extra_redaction_patterns,
        ));
        Ok(Arc::new(Daemon {
            index,
            settings: RwLock::new(settings),
            started_at: runlog_core::now_micros(),
            shutdown: AtomicBool::new(false),
            projects: Mutex::new(HashMap::new()),
            runs: Mutex::new(HashMap::new()),
            subscribers: Mutex::new(Vec::new()),
            waiters: Mutex::new(Vec::new()),
            redactor: RwLock::new(redactor),
            next_id: AtomicU64::new(1),
        }))
    }

    pub fn settings(&self) -> Settings {
        self.settings.read().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn redactor(&self) -> Arc<Redactor> {
        self.redactor
            .read()
            .map(|r| r.clone())
            .unwrap_or_else(|_| Arc::new(Redactor::disabled()))
    }

    pub fn set_settings(&self, new: Settings) -> Result<()> {
        let redactor = Arc::new(Redactor::new(
            new.redaction_enabled,
            &new.extra_redaction_patterns,
        ));
        if let Ok(mut w) = self.redactor.write() {
            *w = redactor;
        }
        if let Ok(mut s) = self.settings.write() {
            *s = new.clone();
        }
        let json = serde_json::to_string_pretty(&new)?;
        std::fs::write(paths::settings_file(), json)
            .map_err(|e| Error::io(paths::settings_file(), e))?;
        Ok(())
    }

    // ---- projects ----

    /// Resolve a reference to an already-registered project.
    pub fn resolve(&self, r: &ProjectRef) -> Result<Arc<ProjectHandle>> {
        let project = match r {
            ProjectRef::Id(id) => self.index.get(*id)?,
            ProjectRef::Path(p) => self.index.resolve_path(Path::new(p))?,
        };
        let project = project.ok_or_else(|| {
            let what = match r {
                ProjectRef::Id(id) => format!("id {id}"),
                ProjectRef::Path(p) => p.clone(),
            };
            Error::NoSuchProject(format!(
                "{what} — nothing has been captured there yet. Run `runlog <your dev command>` in that directory first."
            ))
        })?;
        self.handle_for(project)
    }

    /// Resolve, registering the project if this is the first run there.
    pub fn resolve_or_register(&self, cwd: &Path, name: &str) -> Result<Arc<ProjectHandle>> {
        if let Some(p) = self.index.resolve_path(cwd)? {
            return self.handle_for(p);
        }
        let root = runlog_core::project::project_root(cwd);
        let db_path = paths::project_db(&root);
        let project = self.index.register(&root, name, &db_path)?;
        let handle = self.handle_for(project)?;
        self.broadcast(Event::ProjectsChanged);
        Ok(handle)
    }

    fn handle_for(&self, project: Project) -> Result<Arc<ProjectHandle>> {
        let mut open = self
            .projects
            .lock()
            .map_err(|_| Error::other("project map poisoned"))?;
        if let Some(h) = open.get(&project.id) {
            return Ok(h.clone());
        }
        let db = Db::open(Path::new(&project.db_path), project.id)?;
        let handle = Arc::new(ProjectHandle {
            project,
            db,
            notifier: Mutex::new(Notifier::new()),
        });
        open.insert(handle.project.id, handle.clone());
        Ok(handle)
    }

    /// Re-read a project row after its config changed, so cached handles do not serve
    /// stale mute or retention settings.
    pub fn refresh_project(&self, id: ProjectId) -> Result<()> {
        if let Ok(mut open) = self.projects.lock() {
            open.remove(&id);
        }
        if let Some(p) = self.index.get(id)? {
            self.handle_for(p)?;
        }
        Ok(())
    }

    pub fn retention_cap(&self, project: &Project) -> i64 {
        project
            .retention_bytes
            .unwrap_or_else(|| self.settings().retention_bytes)
    }

    // ---- live runs ----

    pub fn start_run(&self, stamp: &RunStamp) -> Result<(Arc<ProjectHandle>, Arc<LiveRun>)> {
        let cwd = PathBuf::from(&stamp.cwd);
        let handle = self.resolve_or_register(&cwd, &stamp.project_name)?;
        let writer = Writer::start(&handle.db, &self.index, stamp, self.redactor())?;
        let run_id = writer.run_id();

        // A restarted server should be able to notify about the same error again, rather
        // than staying silent because the previous run already reported it.
        if let Ok(mut n) = handle.notifier.lock() {
            n.reset_for_new_run();
        }

        let live = Arc::new(LiveRun {
            run_id,
            project_id: handle.project.id,
            command: stamp.command.join(" "),
            writer: Mutex::new(writer),
        });
        self.runs
            .lock()
            .map_err(|_| Error::other("run map poisoned"))?
            .insert(run_id, live.clone());

        if let Ok(run) = read::get_run(&handle.db, run_id) {
            self.broadcast_project(handle.project.id, Event::RunStarted { run });
        }
        Ok((handle, live))
    }

    pub fn finish_run(
        &self,
        handle: &ProjectHandle,
        live: &LiveRun,
        exit_code: Option<i32>,
        signal: Option<&str>,
    ) -> Result<()> {
        {
            let mut w = live
                .writer
                .lock()
                .map_err(|_| Error::other("writer poisoned"))?;
            let flushed = w.finish(&self.index, exit_code, signal)?;
            self.publish(handle, live, flushed);
        }
        self.runs
            .lock()
            .map_err(|_| Error::other("run map poisoned"))?
            .remove(&live.run_id);

        if let Ok(run) = read::get_run(&handle.db, live.run_id) {
            self.broadcast_project(handle.project.id, Event::RunFinished { run });
        }

        let settings = self.settings();
        if let Ok(mut n) = handle.notifier.lock() {
            let decision = n.run_ended(
                handle.project.id,
                &handle.project.name,
                &live.command,
                exit_code,
                signal,
                &settings,
                !handle.project.notify_enabled,
            );
            drop(n);
            self.deliver(decision);
        }

        // Enforce the cap after the run rather than during it: evicting bytes while the
        // user is still watching them arrive is the wrong moment.
        let cap = self.retention_cap(&handle.project);
        let _ = retention::enforce(&handle.db, cap);
        Ok(())
    }

    /// Push captured bytes into a run, fanning out whatever the flush produced.
    pub fn ingest(
        &self,
        handle: &ProjectHandle,
        live: &LiveRun,
        bytes: &[u8],
        captured_at: i64,
    ) -> Result<()> {
        let flushed = {
            let mut w = live
                .writer
                .lock()
                .map_err(|_| Error::other("writer poisoned"))?;
            w.push(bytes, captured_at)?
        };
        if let Some(f) = flushed {
            self.publish(handle, live, f);
        }
        Ok(())
    }

    /// Close entries that have gone quiet, so a server that stopped mid-stack-trace still
    /// shows what it printed.
    pub fn tick_runs(&self) {
        let live: Vec<Arc<LiveRun>> = match self.runs.lock() {
            Ok(m) => m.values().cloned().collect(),
            Err(_) => return,
        };
        let now = runlog_core::now_micros();
        for run in live {
            let Ok(handle) = self.resolve(&ProjectRef::Id(run.project_id)) else {
                continue;
            };
            let flushed = match run.writer.lock() {
                Ok(mut w) => w.tick(now).ok().flatten(),
                Err(_) => None,
            };
            if let Some(f) = flushed {
                self.publish(&handle, &run, f);
            }
        }
    }

    /// Fan a flush out to subscribers, waiters and the notification policy.
    fn publish(&self, handle: &ProjectHandle, live: &LiveRun, flushed: runlog_core::store::write::Flushed) {
        if flushed.entries.is_empty() {
            return;
        }
        self.broadcast_project(
            handle.project.id,
            Event::Entries {
                run_id: live.run_id,
                entries: flushed.entries.clone(),
            },
        );
        self.broadcast_project(
            handle.project.id,
            Event::Counters {
                run_id: live.run_id,
                counters: flushed.counters,
            },
        );
        self.wake_waiters(handle.project.id, &flushed.entries);

        let settings = self.settings();
        let muted = !handle.project.notify_enabled;
        let mut decisions = Vec::new();
        if let Ok(mut n) = handle.notifier.lock() {
            for e in flushed.entries.iter().filter(|e| e.entry.is_error) {
                let Some(fp) = e.entry.fingerprint_id else {
                    continue;
                };
                decisions.push(n.consider(
                    &Candidate {
                        project_id: handle.project.id,
                        project_name: handle.project.name.clone(),
                        fingerprint_id: fp,
                        template: e.template.clone().unwrap_or_else(|| e.text.clone()),
                        level: e.entry.level,
                        at: e.entry.captured_at,
                        is_new_signature: e.is_new_signature,
                        run_command: live.command.clone(),
                    },
                    &settings,
                    muted,
                ));
            }
        }
        for d in decisions {
            self.deliver(d);
        }
    }

    /// A notice goes to every subscriber so the app's in-window state matches the OS
    /// banner, and to the OS only when no app is listening to post it itself.
    fn deliver(&self, decision: Decision) {
        let notice = match decision {
            Decision::Post(n) | Decision::Update(n) => n,
            Decision::Suppress(_) => return,
        };
        let has_app = self
            .subscribers
            .lock()
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        self.broadcast(Event::Notice {
            notice: notice.clone(),
        });
        if !has_app {
            crate::os_notify::post(&notice);
        }
    }

    // ---- fanout ----

    pub fn subscribe(&self, project: Option<ProjectId>) -> (u64, Receiver<Event>) {
        let (tx, rx) = crossbeam_channel::bounded(1024);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut subs) = self.subscribers.lock() {
            subs.push(Subscriber { id, project, tx });
        }
        (id, rx)
    }

    pub fn unsubscribe(&self, id: u64) {
        if let Ok(mut subs) = self.subscribers.lock() {
            subs.retain(|s| s.id != id);
        }
    }

    pub fn broadcast(&self, event: Event) {
        self.broadcast_inner(None, event)
    }

    pub fn broadcast_project(&self, project_id: ProjectId, event: Event) {
        self.broadcast_inner(Some(project_id), event)
    }

    fn broadcast_inner(&self, project_id: Option<ProjectId>, event: Event) {
        let Ok(mut subs) = self.subscribers.lock() else {
            return;
        };
        subs.retain(|s| {
            let wants = match (s.project, project_id) {
                (None, _) => true,
                (Some(want), Some(got)) => want == got,
                (Some(_), None) => true,
            };
            if !wants {
                return true;
            }
            // A subscriber that cannot keep up is dropped. The alternative is buffering
            // without limit on the capture path, which would trade a stale UI for lost
            // output — the wrong way round.
            s.tx.try_send(event.clone()).is_ok()
        });
    }

    // ---- wait ----

    pub fn wait(
        &self,
        project_id: ProjectId,
        timeout_ms: u64,
        level: Option<Level>,
        after_entry: Option<EntryId>,
    ) -> Vec<EntryView> {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut w) = self.waiters.lock() {
            w.push(Waiter {
                id,
                project_id,
                level,
                after_entry,
                tx,
            });
        }
        let out = rx
            .recv_timeout(std::time::Duration::from_millis(timeout_ms))
            .unwrap_or_default();
        // Remove by id whether we were woken or timed out. A waiter left behind would be
        // walked by every future flush for the lifetime of the daemon.
        if let Ok(mut w) = self.waiters.lock() {
            w.retain(|x| x.id != id);
        }
        out
    }

    fn wake_waiters(&self, project_id: ProjectId, entries: &[EntryView]) {
        let Ok(mut waiters) = self.waiters.lock() else {
            return;
        };
        waiters.retain(|w| {
            if w.project_id != project_id {
                return true;
            }
            let matched: Vec<EntryView> = entries
                .iter()
                .filter(|e| w.level.map(|l| e.entry.level == l).unwrap_or(true))
                .filter(|e| w.after_entry.map(|a| e.entry.id > a).unwrap_or(true))
                .cloned()
                .collect();
            if matched.is_empty() {
                return true;
            }
            // Sent, so this waiter is done. A send failure means the caller already
            // timed out and went away.
            let _ = w.tx.try_send(matched);
            false
        });
    }

    /// Close runs left open by a daemon that stopped without finishing them.
    ///
    /// Runs at startup, before the socket accepts anything, because until it has run the
    /// home screen would claim a dead run is live. Every registered project is opened for
    /// this — it happens once and costs a file open each.
    pub fn reconcile_orphaned_runs(&self) -> usize {
        let mut closed = 0;
        for project in self.index.list().unwrap_or_default() {
            let Ok(handle) = self.handle_for(project) else {
                continue;
            };
            match runlog_core::store::write::close_orphaned_runs(&handle.db) {
                Ok(n) => closed += n,
                Err(e) => eprintln!("runlogd: could not reconcile {}: {e}", handle.project.name),
            }
        }
        closed
    }

    /// Finish everything still streaming, so a deliberate shutdown does not leave behind
    /// exactly the orphans the startup pass has to clean up.
    pub fn close_active_runs(&self) {
        let live: Vec<Arc<LiveRun>> = match self.runs.lock() {
            Ok(m) => m.values().cloned().collect(),
            Err(_) => return,
        };
        for run in live {
            let Ok(handle) = self.resolve(&ProjectRef::Id(run.project_id)) else {
                continue;
            };
            // No exit code: the process may well still be alive, and claiming to know how
            // it ended would be a lie. This reads as an unexpected death, which is true
            // from runlog's point of view — it stopped watching.
            let _ = self.finish_run(&handle, &run, None, None);
        }
    }

    pub fn active_run_count(&self) -> usize {
        self.runs.lock().map(|r| r.len()).unwrap_or(0)
    }

    pub fn open_projects(&self) -> Vec<Arc<ProjectHandle>> {
        self.projects
            .lock()
            .map(|p| p.values().cloned().collect())
            .unwrap_or_default()
    }
}

fn load_settings() -> Settings {
    std::fs::read_to_string(paths::settings_file())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// `RUNLOG_HOME` is process-global, so tests that point it at their own directory
    /// must not overlap. Without this they pass or fail depending on scheduling.
    pub(crate) static HOME_LOCK: Mutex<()> = Mutex::new(());

    fn daemon(tmp: &tempfile::TempDir) -> Arc<Daemon> {
        std::env::set_var("RUNLOG_HOME", tmp.path());
        Daemon::open().unwrap()
    }

    #[test]
    fn a_first_run_registers_its_project() {
        let _guard = HOME_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(&tmp);
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();

        let stamp = RunStamp {
            command: vec!["mix".into(), "phx.server".into()],
            cwd: work.to_string_lossy().into_owned(),
            project_name: "work".into(),
            ..Default::default()
        };
        let (handle, live) = d.start_run(&stamp).unwrap();
        assert_eq!(handle.project.name, "work");
        assert_eq!(d.active_run_count(), 1);

        d.ingest(&handle, &live, b"Error: boom\n", runlog_core::now_micros()).unwrap();
        d.finish_run(&handle, &live, Some(1), None).unwrap();
        assert_eq!(d.active_run_count(), 0);

        let run = read::get_run(&handle.db, live.run_id).unwrap();
        assert_eq!(run.exit_code, Some(1));
    }

    #[test]
    fn a_subscriber_sees_the_run_it_asked_for() {
        let _guard = HOME_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(&tmp);
        let work = tmp.path().join("w2");
        std::fs::create_dir_all(&work).unwrap();

        let (_id, rx) = d.subscribe(None);
        let stamp = RunStamp {
            command: vec!["npm".into(), "test".into()],
            cwd: work.to_string_lossy().into_owned(),
            project_name: "w2".into(),
            ..Default::default()
        };
        let (handle, live) = d.start_run(&stamp).unwrap();
        d.ingest(&handle, &live, b"Error: boom\n", runlog_core::now_micros()).unwrap();
        d.finish_run(&handle, &live, Some(1), None).unwrap();

        let mut saw_start = false;
        let mut saw_entries = false;
        while let Ok(ev) = rx.recv_timeout(std::time::Duration::from_millis(200)) {
            match ev {
                Event::RunStarted { .. } => saw_start = true,
                Event::Entries { entries, .. } => {
                    saw_entries |= entries.iter().any(|e| e.entry.is_error)
                }
                _ => {}
            }
        }
        assert!(saw_start, "no run_started event");
        assert!(saw_entries, "the error never reached the subscriber");
    }

    #[test]
    fn resolving_an_unknown_directory_says_what_to_do_about_it() {
        let _guard = HOME_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(&tmp);
        let err = match d.resolve(&ProjectRef::Path("/nowhere/at/all".into())) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("an unregistered directory should not resolve"),
        };
        assert!(err.contains("runlog <your dev command>"), "unhelpful: {err}");
    }

    #[test]
    fn settings_survive_a_restart() {
        let _guard = HOME_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(&tmp);
        let mut s = d.settings();
        s.block_budget = 7;
        s.notifications_enabled = false;
        d.set_settings(s).unwrap();

        let again = Daemon::open().unwrap();
        assert_eq!(again.settings().block_budget, 7);
        assert!(!again.settings().notifications_enabled);
    }
}
