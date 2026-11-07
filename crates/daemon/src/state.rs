//! Daemon state: the project handles, the live runs, and the fanout.
//!
//! Threads rather than an async runtime: the work is a local unix socket in front of a
//! local SQLite database, so a thread per connection is enough.

use crossbeam_channel::{Receiver, Sender};
use logbear_core::model::*;
use logbear_core::notify::{Candidate, Decision, Notifier};
use logbear_core::paths;
use logbear_core::proto::*;
use logbear_core::redact::Redactor;
use logbear_core::store::{self, index::IndexDb, read, retention, Db, Writer};
use logbear_core::{Error, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};

/// How long [`Daemon::resume_run`] waits for the abandoned connection to close its run.
const RESUME_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Projects held open at once, whatever the file descriptor limit allows. Every directory a
/// command runs in becomes a project, so these accumulate far faster than repos do; the
/// rest open on demand.
const MAX_OPEN_PROJECTS: usize = 64;
const MIN_OPEN_PROJECTS: usize = 4;
/// Descriptors kept back from the limit for sockets and logs.
const RESERVED_FDS: u64 = 64;
/// One open project: a writer with its WAL, the shared memory file, two idle readers with
/// theirs, a checkpointer, and the two lazy readers an agent's query can open.
const FDS_PER_PROJECT: u64 = 12;
/// A project with no live run closes after this long without being used.
const IDLE_CLOSE_MICROS: i64 = 5 * 60 * 1_000_000;

/// Everything about one project the daemon holds open.
pub struct ProjectHandle {
    pub project: Project,
    pub db: Db,
    pub notifier: Mutex<Notifier>,
    last_used: AtomicI64,
}

/// A live run's key. Run ids are rowids of a per-project table, so every project has a
/// run 1 and the id alone says nothing.
type RunKey = (ProjectId, RunId);

/// A run whose end could not be written, kept to try again.
struct Unfinished {
    live: Arc<LiveRun>,
    exit_code: Option<i32>,
    signal: Option<String>,
}

/// A live run and the writer feeding it.
pub struct LiveRun {
    pub run_id: RunId,
    pub project_id: ProjectId,
    pub command: String,
    pub writer: Mutex<Writer>,
}

/// One connected subscriber.
///
/// The channel is bounded and a subscriber that fills it is dropped, because losing a live
/// view is recoverable and losing a run is not.
pub struct Subscriber {
    pub id: u64,
    pub project: Option<ProjectId>,
    pub tx: Sender<Event>,
}

/// A blocked `logbear_wait` call.
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
    /// Serialises opening, which can take as long as a migration, without holding
    /// `projects` and so every request for a project that is already open.
    opening: Mutex<()>,
    max_open_projects: usize,
    runs: Mutex<HashMap<RunKey, Arc<LiveRun>>>,
    unfinished: Mutex<Vec<Unfinished>>,
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
            started_at: logbear_core::now_micros(),
            shutdown: AtomicBool::new(false),
            projects: Mutex::new(HashMap::new()),
            opening: Mutex::new(()),
            max_open_projects: open_project_limit(),
            runs: Mutex::new(HashMap::new()),
            unfinished: Mutex::new(Vec::new()),
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

    /// Resolves a reference to an already-registered project.
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
                "{what} — nothing has been captured there yet. Run `logbear <your dev command>` in that directory first."
            ))
        })?;
        self.handle_for(project)
    }

    /// Resolves a directory, registering the project if this is the first run there.
    pub fn resolve_or_register(&self, cwd: &Path, name: &str) -> Result<Arc<ProjectHandle>> {
        if let Some(p) = self.index.resolve_path(cwd)? {
            return self.handle_for(p);
        }
        let root = logbear_core::project::project_root(cwd);
        let db_path = paths::project_db(&root);
        let project = self.index.register(&root, name, &db_path)?;
        let handle = self.handle_for(project)?;
        self.broadcast(Event::ProjectsChanged);
        Ok(handle)
    }

    fn open_handles(&self) -> MutexGuard<'_, HashMap<ProjectId, Arc<ProjectHandle>>> {
        self.projects.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn cached_handle(&self, id: ProjectId) -> Option<Arc<ProjectHandle>> {
        let open = self.open_handles();
        let handle = open.get(&id)?;
        handle.last_used.store(logbear_core::now_micros(), Ordering::Relaxed);
        Some(handle.clone())
    }

    fn handle_for(&self, project: Project) -> Result<Arc<ProjectHandle>> {
        if let Some(h) = self.cached_handle(project.id) {
            return Ok(h);
        }
        let _opening = self.opening.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(h) = self.cached_handle(project.id) {
            return Ok(h);
        }
        // Dropped once `projects` is unlocked: closing a database is a checkpoint.
        let closed = self.make_room();

        let (db, quarantined) = open_project_db(&project)?;
        if let Some(aside) = quarantined {
            eprintln!(
                "logbeard: {}: database was damaged; moved it to {} and started afresh",
                project.name,
                aside.display()
            );
            let _ = self.index.forget_runs(project.id);
        }
        let _ = db.stamp_identity(&project);

        // Fresh from disk with no live run here: a run it calls open belongs to a daemon
        // that is gone. Done on open rather than at startup so startup need not open every
        // project, and it stays cheap: no open runs, no work.
        if !self.has_live_run(project.id) {
            match store::write::reconcile_orphans(&db, &self.index) {
                Ok(n) if n > 0 => {
                    eprintln!("logbeard: {}: closed {n} run(s) left open", project.name)
                }
                Err(e) => eprintln!("logbeard: {}: could not close old runs: {e}", project.name),
                _ => {}
            }
        }

        let handle = Arc::new(ProjectHandle {
            project,
            db,
            notifier: Mutex::new(Notifier::new()),
            last_used: AtomicI64::new(logbear_core::now_micros()),
        });
        self.open_handles().insert(handle.project.id, handle.clone());
        if handle.db.derived_stale() {
            rebuild_in_background(handle.clone());
        }
        drop(closed);
        Ok(handle)
    }

    /// Removes the least recently used closable handle if the limit has been reached.
    fn make_room(&self) -> Vec<Arc<ProjectHandle>> {
        let mut open = self.open_handles();
        let mut closed = Vec::new();
        while open.len() >= self.max_open_projects {
            let victim = open
                .values()
                .filter(|h| self.closable(h))
                .min_by_key(|h| h.last_used.load(Ordering::Relaxed))
                .map(|h| h.project.id);
            match victim.and_then(|id| open.remove(&id)) {
                Some(h) => closed.push(h),
                // Everything is busy. Going over the limit beats refusing a run.
                None => break,
            }
        }
        closed
    }

    /// Nothing is using it: no live run, and no connection holding the handle. The count of
    /// 1 is the map's own reference.
    fn closable(&self, handle: &Arc<ProjectHandle>) -> bool {
        Arc::strong_count(handle) == 1 && !self.has_live_run(handle.project.id)
    }

    fn has_live_run(&self, project: ProjectId) -> bool {
        self.live_runs().keys().any(|(p, _)| *p == project)
    }

    /// Closes projects nothing has used for a while, handing back their descriptors and
    /// memory.
    pub fn close_idle_projects(&self) {
        let cutoff = logbear_core::now_micros() - IDLE_CLOSE_MICROS;
        let closed: Vec<Arc<ProjectHandle>> = {
            let mut open = self.open_handles();
            let idle: Vec<ProjectId> = open
                .values()
                .filter(|h| h.last_used.load(Ordering::Relaxed) < cutoff && self.closable(h))
                .map(|h| h.project.id)
                .collect();
            idle.iter().filter_map(|id| open.remove(id)).collect()
        };
        drop(closed);
    }

    #[cfg(test)]
    pub(crate) fn close_idle_projects_now(&self) {
        for handle in self.open_projects() {
            handle.last_used.store(0, Ordering::Relaxed);
        }
        self.close_idle_projects();
    }

    /// Re-reads a project row after its config changed, so cached handles do not serve
    /// stale mute or retention settings.
    pub fn refresh_project(&self, id: ProjectId) -> Result<()> {
        let stale = self.open_handles().remove(&id);
        if let Some(p) = self.index.get(id)? {
            self.handle_for(p)?;
        }
        drop(stale);
        Ok(())
    }

    /// Replaces a project database found to be damaged while in use.
    fn quarantine_project(&self, handle: &ProjectHandle) -> Result<()> {
        self.open_handles().remove(&handle.project.id);
        let aside = store::quarantine(handle.db.path())?;
        eprintln!(
            "logbeard: {}: database is damaged; moved it to {}",
            handle.project.name,
            aside.display()
        );
        self.index.forget_runs(handle.project.id)
    }

    /// On-disk size of every registered project, open or not.
    pub fn total_db_bytes(&self) -> i64 {
        self.index
            .list()
            .unwrap_or_default()
            .iter()
            .flat_map(|p| {
                ["", "-wal", "-shm"].map(|suffix| format!("{}{suffix}", p.db_path))
            })
            .filter_map(|path| std::fs::metadata(path).ok())
            .map(|m| m.len() as i64)
            .sum()
    }

    pub fn retention_cap(&self, project: &Project) -> i64 {
        project
            .retention_bytes
            .unwrap_or_else(|| self.settings().retention_bytes)
    }

    // ---- live runs ----

    pub fn start_run(&self, stamp: &RunStamp) -> Result<(Arc<ProjectHandle>, Arc<LiveRun>)> {
        let cwd = PathBuf::from(&stamp.cwd);
        let mut handle = self.resolve_or_register(&cwd, &stamp.project_name)?;
        let writer = match Writer::start(&handle.db, &self.index, stamp, self.redactor()) {
            Err(e) if e.is_corrupt() => {
                eprintln!("logbeard: {}: {e}", handle.project.name);
                self.quarantine_project(&handle)?;
                handle = self.resolve_or_register(&cwd, &stamp.project_name)?;
                Writer::start(&handle.db, &self.index, stamp, self.redactor())?
            }
            other => other?,
        };
        let run_id = writer.run_id();

        // A restarted server should be able to notify about the same error again rather
        // than stay silent because the previous run reported it.
        if let Ok(mut n) = handle.notifier.lock() {
            n.reset_for_new_run();
        }

        let live = Arc::new(LiveRun {
            run_id,
            project_id: handle.project.id,
            command: self.redactor().redact_argv(&stamp.command).join(" "),
            writer: Mutex::new(writer),
        });
        self.live_runs().insert((handle.project.id, run_id), live.clone());

        if let Ok(run) = read::get_run(&handle.db, run_id) {
            self.broadcast_project(handle.project.id, Event::RunStarted { run });
        }
        Ok((handle, live))
    }

    /// Reopens a run whose shim abandoned the socket mid-run, so its spooled tail lands in
    /// the same run. `None` when the run cannot be continued and a new one should be
    /// started instead.
    pub fn resume_run(
        &self,
        stamp: &RunStamp,
        shim_started_at: i64,
        run_id: RunId,
    ) -> Result<Option<(Arc<ProjectHandle>, Arc<LiveRun>)>> {
        let cwd = PathBuf::from(&stamp.cwd);
        let Some(project) = self.index.resolve_path(&cwd)? else {
            return Ok(None);
        };

        // The connection that started the run closes it when it reads EOF, after landing
        // whatever was still in the socket. Those bytes precede the spool's, so wait for
        // it; a connection still stuck after that is left alone and the tail gets a run of
        // its own rather than interleaving with it.
        let deadline = std::time::Instant::now() + RESUME_WAIT;
        while self.live_runs().contains_key(&(project.id, run_id)) {
            if std::time::Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        let handle = self.handle_for(project)?;
        let Some(writer) = Writer::resume(&handle.db, run_id, stamp, shim_started_at, self.redactor())? else {
            return Ok(None);
        };

        let live = Arc::new(LiveRun {
            run_id,
            project_id: handle.project.id,
            command: self.redactor().redact_argv(&stamp.command).join(" "),
            writer: Mutex::new(writer),
        });
        self.live_runs().insert((handle.project.id, run_id), live.clone());
        Ok(Some((handle, live)))
    }

    /// Whether a connection is still streaming this run. Run ids are per project, so the
    /// stamp says which project is meant.
    pub fn is_live(&self, stamp: &RunStamp, run_id: RunId) -> bool {
        let Ok(Some(project)) = self.index.resolve_path(Path::new(&stamp.cwd)) else {
            return false;
        };
        self.live_runs().contains_key(&(project.id, run_id))
    }

    fn live_runs(&self) -> MutexGuard<'_, HashMap<RunKey, Arc<LiveRun>>> {
        self.runs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn writer<'a>(&self, live: &'a LiveRun) -> MutexGuard<'a, Writer> {
        // A panic in the parser leaves the writer's buffers consistent: they are only
        // appended to, and only by a line that was fully built.
        live.writer.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn finish_run(
        &self,
        handle: &ProjectHandle,
        live: &LiveRun,
        exit_code: Option<i32>,
        signal: Option<&str>,
    ) -> Result<()> {
        let finished = self.writer(live).finish(&self.index, exit_code, signal);
        // Off the live list either way, and only now, since this is what a resume waits for.
        // The shim has gone, so nothing more is coming; what storage refused is kept to try
        // again rather than leaving the run open until a restart.
        let taken = self.live_runs().remove(&(handle.project.id, live.run_id));
        match finished {
            Ok(flushed) => {
                self.after_finish(handle, live, flushed, exit_code, signal);
                Ok(())
            }
            Err(e) => {
                if let Some(live) = taken {
                    self.unfinished.lock().unwrap_or_else(PoisonError::into_inner).push(Unfinished {
                        live,
                        exit_code,
                        signal: signal.map(str::to_string),
                    });
                }
                Err(e)
            }
        }
    }

    /// Tries again to record the end of runs storage refused.
    pub fn retry_unfinished(&self) {
        let waiting = std::mem::take(
            &mut *self.unfinished.lock().unwrap_or_else(PoisonError::into_inner),
        );
        for u in waiting {
            let signal = u.signal.as_deref();
            let finished = self.writer(&u.live).finish(&self.index, u.exit_code, signal);
            let handle = self.resolve(&ProjectRef::Id(u.live.project_id));
            match (finished, handle) {
                (Ok(flushed), Ok(handle)) => {
                    self.after_finish(&handle, &u.live, flushed, u.exit_code, signal)
                }
                (Ok(_), Err(_)) => {}
                (Err(_), _) => {
                    self.unfinished.lock().unwrap_or_else(PoisonError::into_inner).push(u)
                }
            }
        }
    }

    /// Tells everyone a run has ended.
    fn after_finish(
        &self,
        handle: &ProjectHandle,
        live: &LiveRun,
        flushed: logbear_core::store::write::Flushed,
        exit_code: Option<i32>,
        signal: Option<&str>,
    ) {
        self.publish(handle, live, flushed);
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

        // Retention is left to its thread, which will see the size within seconds. The
        // caller here is waiting to report the run's exit.
    }

    /// Pushes captured bytes into a run, fanning out whatever the flush produced.
    pub fn ingest(
        &self,
        handle: &ProjectHandle,
        live: &LiveRun,
        bytes: &[u8],
        captured_at: i64,
    ) -> Result<()> {
        let flushed = self.writer(live).push(bytes, captured_at)?;
        if let Some(f) = flushed {
            self.publish(handle, live, f);
        }
        Ok(())
    }

    /// Closes entries that have gone quiet, so a server that stopped mid-stack-trace still
    /// shows what it printed.
    pub fn tick_runs(&self) {
        let live: Vec<Arc<LiveRun>> = self.live_runs().values().cloned().collect();
        let now = logbear_core::now_micros();
        for run in live {
            let Ok(handle) = self.resolve(&ProjectRef::Id(run.project_id)) else {
                continue;
            };
            let flushed = self.writer(&run).tick(now).ok().flatten();
            if let Some(f) = flushed {
                self.publish(&handle, &run, f);
            }
        }
    }

    /// Fans a flush out to subscribers, waiters and the notification policy.
    fn publish(&self, handle: &ProjectHandle, live: &LiveRun, flushed: logbear_core::store::write::Flushed) {
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
                        evidence: e.entry.evidence,
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

    /// Sends a notice to every subscriber, and to the OS only when no app is listening to
    /// post it itself.
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
            // A subscriber that cannot keep up is dropped; buffering without limit on the
            // capture path would trade a stale UI for lost output.
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
        // Removed by id whether woken or timed out: a waiter left behind would be walked
        // by every future flush for the lifetime of the daemon.
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
            // A send failure means the caller already timed out and went away.
            let _ = w.tx.try_send(matched);
            false
        });
    }

    /// Closes runs left open by a daemon that stopped without finishing them.
    ///
    /// Must run at startup before the socket accepts anything, or the home screen would
    /// claim a dead run is live. Only the projects the index says have one are opened, since
    /// every directory a command ran in is a project and there can be thousands. A project
    /// the index does not know about is caught the first time it is opened.
    pub fn reconcile_orphaned_runs(&self) -> usize {
        let mut closed = 0;
        for id in self.index.projects_with_open_runs().unwrap_or_default() {
            let Ok(Some(project)) = self.index.get(id) else {
                continue;
            };
            // Opening a project reconciles it. It is only counted here if it was not open.
            let before = self.open_run_summaries(id);
            if let Err(e) = self.handle_for(project) {
                eprintln!("logbeard: could not reconcile project {id}: {e}");
                continue;
            }
            closed += before.saturating_sub(self.open_run_summaries(id));
        }
        closed
    }

    fn open_run_summaries(&self, id: ProjectId) -> usize {
        self.index.open_run_count(id).unwrap_or(0)
    }

    /// Finishes everything still streaming on shutdown, so a clean stop does not leave
    /// behind the orphans the startup pass has to clean up.
    pub fn close_active_runs(&self) {
        let live: Vec<Arc<LiveRun>> = self.live_runs().values().cloned().collect();
        for run in live {
            let Ok(handle) = self.resolve(&ProjectRef::Id(run.project_id)) else {
                continue;
            };
            // No exit code: the process may still be alive, so claiming to know how it
            // ended would be wrong.
            let _ = self.finish_run(&handle, &run, None, None);
        }
    }

    pub fn active_run_count(&self) -> usize {
        self.live_runs().len()
    }

    pub fn open_projects(&self) -> Vec<Arc<ProjectHandle>> {
        self.open_handles().values().cloned().collect()
    }

    /// Hands back what idle projects and the allocator are holding, for when nothing is
    /// being captured.
    pub fn release_idle_memory(&self) {
        for handle in self.open_projects() {
            if !self.has_live_run(handle.project.id) {
                handle.db.release_memory();
                // With no commits coming, nothing else would fold in what is left.
                let _ = handle.db.checkpoint_truncate();
            }
        }
        release_allocator_memory();
    }
}

/// Opens a project's database, quarantining it if it is damaged.
///
/// Returns where a damaged file went. A newer database is passed up as an error: it is not
/// damaged, and it is not this build's to move.
fn open_project_db(project: &Project) -> Result<(Db, Option<PathBuf>)> {
    let path = Path::new(&project.db_path);
    match Db::open(path, project.id) {
        Err(e) if e.is_corrupt() => {
            let aside = store::quarantine(path)?;
            Ok((Db::open(path, project.id)?, Some(aside)))
        }
        other => Ok((other?, None)),
    }
}

/// Rebuilds a project's derived layer without holding up whoever asked for the project.
/// Reads see the entries come back as it goes, and capture carries on alongside it. The
/// thread holds the handle, which keeps the project from being closed under it.
fn rebuild_in_background(handle: Arc<ProjectHandle>) {
    let spawned = std::thread::Builder::new()
        .name("logbear-rebuild".into())
        .spawn(move || match retention::rebuild_derived(&handle.db) {
            Ok(n) => eprintln!(
                "{}: rebuilt {n} entries from chunks after a derived-schema change",
                handle.project.name
            ),
            // Still marked as outstanding, so the next open picks it up.
            Err(e) => eprintln!("{}: derived rebuild failed: {e}", handle.project.name),
        });
    if let Err(e) = spawned {
        eprintln!("logbeard: could not start a rebuild: {e}");
    }
}

/// How many projects fit in this process's file descriptor limit.
fn open_project_limit() -> usize {
    let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: `limit` is a valid out-pointer for the duration of the call.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return MIN_OPEN_PROJECTS;
    }
    let fit = limit.rlim_cur.saturating_sub(RESERVED_FDS) / FDS_PER_PROJECT;
    (fit as usize).clamp(MIN_OPEN_PROJECTS, MAX_OPEN_PROJECTS)
}

/// Asks the allocator to return free memory to the system. Freed memory otherwise stays
/// with the process, which is how a finished ingest could leave a daemon near a gigabyte.
fn release_allocator_memory() {
    #[cfg(target_os = "macos")]
    {
        extern "C" {
            fn malloc_zone_pressure_relief(zone: *mut libc::c_void, goal: libc::size_t) -> libc::size_t;
        }
        // SAFETY: a null zone means every zone and a zero goal means as much as possible.
        unsafe {
            malloc_zone_pressure_relief(std::ptr::null_mut(), 0);
        }
    }
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: takes no pointers and only touches the allocator's own state.
    unsafe {
        libc::malloc_trim(0);
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

    /// `LOGBEAR_HOME` is process-global, so tests that point it at their own directory must
    /// not overlap.
    pub(crate) static HOME_LOCK: Mutex<()> = Mutex::new(());

    fn daemon(tmp: &tempfile::TempDir) -> Arc<Daemon> {
        std::env::set_var("LOGBEAR_HOME", tmp.path());
        Daemon::open().unwrap()
    }

    fn stamp_in(dir: &Path) -> RunStamp {
        std::fs::create_dir_all(dir).unwrap();
        RunStamp {
            command: vec!["mix".into(), "phx.server".into()],
            cwd: dir.to_string_lossy().into_owned(),
            project_name: "p".into(),
            ..Default::default()
        }
    }

    #[test]
    fn live_runs_in_two_projects_with_the_same_id_stay_apart() {
        // Every project numbers its runs from 1. Keyed on the id alone, the second run
        // replaced the first in the map, and the first stopped being ticked or closed.
        let _guard = HOME_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(&tmp);
        let (a, run_a) = d.start_run(&stamp_in(&tmp.path().join("a"))).unwrap();
        let (b, run_b) = d.start_run(&stamp_in(&tmp.path().join("b"))).unwrap();
        assert_eq!(run_a.run_id, run_b.run_id, "setup: the ids collide");
        assert_ne!(a.project.id, b.project.id);
        assert_eq!(d.active_run_count(), 2);

        // An error in A, with no end yet, while B finishes.
        d.ingest(&a, &run_a, b"[error] in project a\n", logbear_core::now_micros()).unwrap();
        d.finish_run(&b, &run_b, Some(0), None).unwrap();
        assert_eq!(d.active_run_count(), 1, "finishing B must not close A");

        // A tick reaches it, so it shows up with nothing further from the producer.
        std::thread::sleep(std::time::Duration::from_millis(450));
        d.tick_runs();
        assert_eq!(read::errors_since(&a.db, 0).unwrap(), 1, "the error in A vanished");
    }

    #[test]
    fn a_resume_waits_only_on_its_own_projects_run() {
        let _guard = HOME_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(&tmp);
        let dir_a = tmp.path().join("a");
        let (a, run_a) = d.start_run(&stamp_in(&dir_a)).unwrap();
        d.finish_run(&a, &run_a, None, None).unwrap();
        let stamp_b = stamp_in(&tmp.path().join("b"));
        let (_b, _run_b) = d.start_run(&stamp_b).unwrap();

        let started = std::time::Instant::now();
        let stamp_a = stamp_in(&dir_a);
        d.resume_run(&stamp_a, logbear_core::now_micros(), run_a.run_id).unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "held up by a run in another project with the same id"
        );
    }

    #[test]
    fn open_projects_stay_within_the_limit_and_closed_ones_reopen() {
        let _guard = HOME_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let mut d = daemon(&tmp);
        Arc::get_mut(&mut d).unwrap().max_open_projects = 4;

        let first = tmp.path().join("p0");
        for i in 0..12 {
            let (h, live) = d.start_run(&stamp_in(&tmp.path().join(format!("p{i}")))).unwrap();
            d.ingest(&h, &live, b"[info] hello\n", logbear_core::now_micros()).unwrap();
            d.finish_run(&h, &live, Some(0), None).unwrap();
            assert!(d.open_projects().len() <= 4, "{} open", d.open_projects().len());
        }

        let (h, live) = d.start_run(&stamp_in(&first)).unwrap();
        assert_eq!(live.run_id, 2, "it reopened with its history");
        d.finish_run(&h, &live, Some(0), None).unwrap();
    }

    #[test]
    fn a_project_in_use_is_not_closed_to_make_room() {
        let _guard = HOME_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let mut d = daemon(&tmp);
        Arc::get_mut(&mut d).unwrap().max_open_projects = 1;
        let (a, live) = d.start_run(&stamp_in(&tmp.path().join("a"))).unwrap();
        let (b, other) = d.start_run(&stamp_in(&tmp.path().join("b"))).unwrap();
        assert_eq!(d.open_projects().len(), 2, "over the limit beats closing a live run");
        d.finish_run(&b, &other, Some(0), None).unwrap();
        d.finish_run(&a, &live, Some(0), None).unwrap();
    }

    #[test]
    fn idle_projects_close_and_busy_ones_do_not() {
        let _guard = HOME_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(&tmp);
        let (idle, idle_run) = d.start_run(&stamp_in(&tmp.path().join("idle"))).unwrap();
        d.finish_run(&idle, &idle_run, Some(0), None).unwrap();
        let idle_id = idle.project.id;
        idle.last_used.store(0, Ordering::Relaxed);
        drop(idle);
        let (busy, busy_run) = d.start_run(&stamp_in(&tmp.path().join("busy"))).unwrap();
        busy.last_used.store(0, Ordering::Relaxed);

        d.close_idle_projects();

        let open: Vec<ProjectId> = d.open_projects().iter().map(|h| h.project.id).collect();
        assert_eq!(open, vec![busy.project.id]);
        assert!(!open.contains(&idle_id));
        d.finish_run(&busy, &busy_run, Some(0), None).unwrap();
    }

    #[test]
    fn startup_opens_only_projects_with_something_to_reconcile() {
        let _guard = HOME_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        {
            let d = daemon(&tmp);
            for name in ["a", "b", "c"] {
                let (h, live) = d.start_run(&stamp_in(&tmp.path().join(name))).unwrap();
                d.finish_run(&h, &live, Some(0), None).unwrap();
            }
            // Left open, as by a daemon that is killed.
            let _left_open = d.start_run(&stamp_in(&tmp.path().join("crashed"))).unwrap();
        }

        let d = Daemon::open().unwrap();
        assert_eq!(d.reconcile_orphaned_runs(), 1);
        let open = d.open_projects();
        assert_eq!(open.len(), 1, "every project was opened to find one orphan");
        assert_eq!(read::active_runs(&open[0].db).unwrap().len(), 0);
        assert!(d.index.projects_with_open_runs().unwrap().is_empty());

        // And the next start has nothing to do.
        assert_eq!(Daemon::open().unwrap().reconcile_orphaned_runs(), 0);
    }

    #[test]
    fn a_run_the_index_never_heard_of_is_closed_when_its_project_opens() {
        let _guard = HOME_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("a");
        let (run_id, project) = {
            let d = daemon(&tmp);
            let (h, live) = d.start_run(&stamp_in(&dir)).unwrap();
            d.index.forget_runs(h.project.id).unwrap();
            (live.run_id, h.project.id)
        };

        let d = Daemon::open().unwrap();
        let h = d.resolve(&ProjectRef::Id(project)).unwrap();
        assert!(read::get_run(&h.db, run_id).unwrap().ended_at.is_some());
    }

    #[test]
    fn a_damaged_project_database_is_quarantined_and_capture_carries_on() {
        let _guard = HOME_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("a");
        let d = daemon(&tmp);
        let (h, live) = d.start_run(&stamp_in(&dir)).unwrap();
        d.finish_run(&h, &live, Some(0), None).unwrap();
        let path = h.db.path().to_path_buf();
        drop(h);
        drop(live);
        d.close_idle_projects_now();

        std::fs::write(&path, b"overwritten by something that is not a database").unwrap();
        let _ = std::fs::remove_file(format!("{}-wal", path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", path.display()));

        let (h, live) = d.start_run(&stamp_in(&dir)).unwrap();
        d.ingest(&h, &live, b"[info] back in business\n", logbear_core::now_micros()).unwrap();
        d.finish_run(&h, &live, Some(0), None).unwrap();
        assert_eq!(live.run_id, 1, "a new database");

        let corrupt = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter(|f| f.as_ref().unwrap().file_name().to_string_lossy().contains(".corrupt-"))
            .count();
        assert_eq!(corrupt, 1, "the damaged file should be kept");
    }

    #[test]
    fn a_database_from_a_newer_build_is_an_error_not_a_quarantine() {
        let _guard = HOME_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("a");
        let d = daemon(&tmp);
        let (h, live) = d.start_run(&stamp_in(&dir)).unwrap();
        d.finish_run(&h, &live, Some(0), None).unwrap();
        let path = h.db.path().to_path_buf();
        drop(h);
        drop(live);
        d.close_idle_projects_now();
        logbear_core_raw_stamp(&path, 999);

        let err = d.start_run(&stamp_in(&dir)).err().expect("must be refused");
        assert!(err.to_string().contains("Upgrade logbear"), "{err}");
        assert!(path.exists(), "the file must stay where it is");
        let moved = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter(|f| f.as_ref().unwrap().file_name().to_string_lossy().contains(".corrupt-"))
            .count();
        assert_eq!(moved, 0);
    }

    fn logbear_core_raw_stamp(path: &Path, version: i64) {
        // No sqlite here, so through the file header: `user_version` is at byte 60.
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.seek(SeekFrom::Start(60)).unwrap();
        f.write_all(&(version as u32).to_be_bytes()).unwrap();
    }

    #[test]
    fn a_refused_finish_is_retried_rather_than_lost() {
        let _guard = HOME_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(&tmp);
        let (h, live) = d.start_run(&stamp_in(&tmp.path().join("a"))).unwrap();
        let bytes: Vec<u8> = (0..4000).flat_map(|i| format!("[info] line {i} padding padding padding\n").into_bytes()).collect();
        d.ingest(&h, &live, &bytes, logbear_core::now_micros()).ok();
        h.db.with(|c| {
            let pages: i64 = c.query_row("PRAGMA page_count", [], |r| r.get(0))?;
            c.execute_batch(&format!("PRAGMA max_page_count = {pages};"))?;
            Ok(())
        })
        .unwrap();

        assert!(d.finish_run(&h, &live, Some(7), None).is_err());
        assert_eq!(d.active_run_count(), 0, "the shim has gone, so the run is not live");

        h.db.with(|c| {
            c.execute_batch("PRAGMA max_page_count = 1073741823;")?;
            Ok(())
        })
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        d.retry_unfinished();

        let run = read::get_run(&h.db, live.run_id).unwrap();
        assert_eq!(run.exit_code, Some(7));
        assert_eq!(run.counters.n_entries, 4000);
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

        d.ingest(&handle, &live, b"Error: boom\n", logbear_core::now_micros()).unwrap();
        d.finish_run(&handle, &live, Some(1), None).unwrap();
        assert_eq!(d.active_run_count(), 0);

        let run = read::get_run(&handle.db, live.run_id).unwrap();
        assert_eq!(run.exit_code, Some(1));
    }

    /// Notifications are built from this copy, not from the run row.
    #[test]
    fn a_live_run_holds_no_argv_secret() {
        let _guard = HOME_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(&tmp);
        let work = tmp.path().join("w3");
        std::fs::create_dir_all(&work).unwrap();

        let stamp = RunStamp {
            command: vec!["app".into(), "--password=hunter2".into()],
            cwd: work.to_string_lossy().into_owned(),
            project_name: "w3".into(),
            ..Default::default()
        };
        let (handle, live) = d.start_run(&stamp).unwrap();
        assert!(!live.command.contains("hunter2"), "{}", live.command);
        d.finish_run(&handle, &live, Some(0), None).unwrap();

        let resumed = d.resume_run(&stamp, logbear_core::now_micros(), live.run_id);
        // Finished runs do not resume; what matters is that nothing here unmasks it.
        assert!(resumed.unwrap().is_none());
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
        d.ingest(&handle, &live, b"Error: boom\n", logbear_core::now_micros()).unwrap();
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
        assert!(err.contains("logbear <your dev command>"), "unhelpful: {err}");
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
