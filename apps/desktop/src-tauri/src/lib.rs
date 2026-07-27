//! The desktop app's backend.
//!
//! It is a *client*, and holds no state the daemon does not own. Every command below is a
//! thin translation of a frontend call into a daemon request — the app can be closed,
//! restarted, or never opened at all, and capture is unaffected.
//!
//! Two things it does own, because they need an application identity the daemon has not
//! got: posting OS notifications, and installing the integrations (see [`install`]).

mod install;

use runlog_core::client::Client;
use runlog_core::model::*;
use runlog_core::proto::*;
use serde::Serialize;
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, State};
use tauri_plugin_notification::NotificationExt;

/// The channel the frontend listens on. One event name carrying a tagged union, rather
/// than a name per event: the union is already exhaustive in the type system, and one
/// listener cannot fall out of sync with six.
const EVENT: &str = "runlog://event";

struct Backend {
    /// Serialised because a `Client` is one socket with request/response ordering.
    /// Commands are short and local, so contention is not a real cost.
    client: Mutex<Option<Client>>,
}

impl Backend {
    fn new() -> Backend {
        Backend {
            client: Mutex::new(None),
        }
    }

    /// Run something against the daemon, reconnecting once if the socket has gone away —
    /// which happens routinely when the daemon is upgraded or restarted underneath us.
    fn with<T>(&self, f: impl Fn(&mut Client) -> runlog_core::Result<T>) -> Result<T, String> {
        let mut guard = self
            .client
            .lock()
            .map_err(|_| "backend lock poisoned".to_string())?;
        if guard.is_none() {
            *guard = Some(Client::connect_or_start().map_err(fmt_daemon_error)?);
        }
        if let Some(client) = guard.as_mut() {
            match f(client) {
                Ok(v) => return Ok(v),
                Err(e) if is_disconnect(&e) => *guard = None,
                Err(e) => return Err(e.to_string()),
            }
        }
        let mut fresh = Client::connect_or_start().map_err(fmt_daemon_error)?;
        let out = f(&mut fresh).map_err(|e| e.to_string());
        *guard = Some(fresh);
        out
    }
}

fn is_disconnect(e: &runlog_core::Error) -> bool {
    matches!(
        e,
        runlog_core::Error::DaemonUnreachable(_)
            | runlog_core::Error::Protocol(_)
            | runlog_core::Error::Bare(_)
    )
}

fn fmt_daemon_error(e: runlog_core::Error) -> String {
    format!(
        "The runlog daemon is not running and could not be started ({e}). \
         Open Settings → Install and register the background service."
    )
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------
//
// `(async)` on everything that talks to the daemon: Tauri runs a plain `fn` on the main
// thread, and these all block on a socket round trip, so one slow reply stops the window
// painting. `reveal_path` is the exception — it drives AppKit and has to stay there.

#[tauri::command(async)]
fn daemon_status(backend: State<'_, Backend>) -> Result<DaemonStatus, String> {
    backend.with(|c| c.status())
}

#[tauri::command(async)]
fn list_projects(backend: State<'_, Backend>) -> Result<Vec<Project>, String> {
    backend.with(|c| c.projects())
}

#[tauri::command(async)]
fn project_status(
    backend: State<'_, Backend>,
    project: ProjectRef,
) -> Result<ProjectStatus, String> {
    backend.with(|c| c.project_status(project.clone()))
}

#[tauri::command(async)]
fn list_runs(
    backend: State<'_, Backend>,
    project: ProjectRef,
    limit: Option<usize>,
    offset: Option<usize>,
) -> Result<Vec<Run>, String> {
    let (limit, offset) = (limit.unwrap_or(100), offset.unwrap_or(0));
    backend.with(|c| c.runs(project.clone(), limit, offset))
}

#[tauri::command(async)]
fn get_run(backend: State<'_, Backend>, project: ProjectRef, run_id: RunId) -> Result<Run, String> {
    backend.with(|c| {
        c.call(&Request::GetRun {
            project: project.clone(),
            run_id,
        })
    })
}

#[tauri::command(async)]
fn entries(
    backend: State<'_, Backend>,
    project: ProjectRef,
    filter: Option<EntryFilter>,
) -> Result<Vec<EntryView>, String> {
    let filter = filter.unwrap_or_default();
    backend.with(|c| c.entries(project.clone(), filter.clone()))
}

#[tauri::command(async)]
fn fingerprints(
    backend: State<'_, Backend>,
    project: ProjectRef,
    limit: Option<usize>,
    errors_only: Option<bool>,
) -> Result<Vec<Fingerprint>, String> {
    let (limit, errors_only) = (limit.unwrap_or(200), errors_only.unwrap_or(false));
    backend.with(|c| {
        c.call(&Request::Fingerprints {
            project: project.clone(),
            limit,
            errors_only,
        })
    })
}

#[derive(Serialize)]
struct HistoryPoint {
    run_id: RunId,
    count: i64,
    started_at: i64,
}

#[tauri::command(async)]
fn fingerprint_history(
    backend: State<'_, Backend>,
    project: ProjectRef,
    fingerprint_id: FingerprintId,
    limit: Option<usize>,
) -> Result<Vec<HistoryPoint>, String> {
    let limit = limit.unwrap_or(30);
    // Expressed as SQL rather than as another protocol verb: this is exactly the kind of
    // question the query endpoint exists for, and adding a request per chart would make
    // the protocol grow without bound.
    let result = backend.with(|c| {
        c.query(
            project.clone(),
            "SELECT r.id, COALESCE(rf.count, 0), r.started_at
               FROM runs r
               LEFT JOIN run_fingerprints rf
                 ON rf.run_id = r.id AND rf.fingerprint_id = ?1
              ORDER BY r.started_at DESC LIMIT ?2",
            vec![
                serde_json::json!(fingerprint_id),
                serde_json::json!(limit as i64),
            ],
            Some(limit),
        )
    })?;

    let mut points: Vec<HistoryPoint> = result
        .rows
        .iter()
        .filter_map(|row| {
            Some(HistoryPoint {
                run_id: row.first()?.as_i64()?,
                count: row.get(1)?.as_i64()?,
                started_at: row.get(2)?.as_i64()?,
            })
        })
        .collect();
    points.reverse();
    Ok(points)
}

#[tauri::command(async)]
fn run_diff(
    backend: State<'_, Backend>,
    project: ProjectRef,
    a: Option<RunRef>,
    b: Option<RunRef>,
) -> Result<RunDiff, String> {
    backend.with(|c| c.diff(project.clone(), a.clone(), b.clone()))
}

#[tauri::command(async)]
fn raw_stream(
    backend: State<'_, Backend>,
    project: ProjectRef,
    run_id: RunId,
) -> Result<String, String> {
    let bytes = backend.with(|c| {
        let mut buf: Vec<u8> = Vec::new();
        c.raw_stream(project.clone(), run_id, |chunk| {
            buf.extend_from_slice(chunk);
            Ok(())
        })?;
        Ok(buf)
    })?;
    // Lossy on purpose: a dev log can carry a half-written multi-byte character at a
    // flush boundary, and refusing to show the run over one bad byte helps nobody.
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[tauri::command(async)]
fn get_settings(backend: State<'_, Backend>) -> Result<Settings, String> {
    backend.with(|c| c.call(&Request::GetSettings))
}

#[tauri::command(async)]
fn update_settings(backend: State<'_, Backend>, settings: Settings) -> Result<(), String> {
    backend.with(|c| {
        c.call::<serde_json::Value>(&Request::UpdateSettings {
            settings: settings.clone(),
        })
        .map(|_| ())
    })
}

#[tauri::command(async)]
fn update_project(
    backend: State<'_, Backend>,
    project: ProjectRef,
    retention_bytes: Option<Option<i64>>,
    push_enabled: Option<bool>,
    notify_enabled: Option<bool>,
) -> Result<(), String> {
    backend.with(|c| {
        c.call::<serde_json::Value>(&Request::UpdateProject {
            project: project.clone(),
            retention_bytes,
            push_enabled,
            notify_enabled,
        })
        .map(|_| ())
    })
}

#[tauri::command(async)]
fn retention(backend: State<'_, Backend>, project: ProjectRef) -> Result<RetentionState, String> {
    let status = backend.with(|c| c.project_status(project.clone()))?;
    let result = backend.with(|c| {
        c.query(
            project.clone(),
            "SELECT (SELECT count(*) FROM runs),
                    (SELECT COALESCE(sum(chunks_evicted), 0) FROM runs),
                    (SELECT min(started_at) FROM runs)",
            vec![],
            Some(1),
        )
    })?;
    let row = result.rows.first().cloned().unwrap_or_default();
    let num = |i: usize| row.get(i).and_then(|v| v.as_i64());
    let default_cap = backend
        .with(|c| c.call::<Settings>(&Request::GetSettings))
        .map(|s| s.retention_bytes)
        .unwrap_or(1024 * 1024 * 1024);

    Ok(RetentionState {
        project_id: status.project.id,
        bytes_used: status.db_bytes,
        bytes_cap: status.project.retention_bytes.unwrap_or(default_cap),
        runs_total: num(0).unwrap_or(0),
        runs_evicted: num(1).unwrap_or(0),
        oldest_run_at: num(2),
    })
}

#[tauri::command(async)]
fn reparse(
    backend: State<'_, Backend>,
    project: ProjectRef,
    run_id: Option<RunId>,
) -> Result<usize, String> {
    backend.with(|c| {
        c.call(&Request::Reparse {
            project: project.clone(),
            run_id,
        })
    })
}

#[tauri::command(async)]
fn delete_run(
    backend: State<'_, Backend>,
    project: ProjectRef,
    run_id: RunId,
) -> Result<(), String> {
    backend.with(|c| {
        c.call::<serde_json::Value>(&Request::DeleteRun {
            project: project.clone(),
            run_id,
        })
        .map(|_| ())
    })
}

#[tauri::command(async)]
fn query(
    backend: State<'_, Backend>,
    project: ProjectRef,
    sql: String,
    max_rows: Option<usize>,
) -> Result<QueryResult, String> {
    backend.with(|c| c.query(project.clone(), &sql, vec![], max_rows))
}

// ---- install ----

#[tauri::command(async)]
fn install_status() -> install::InstallStatus {
    install::status()
}

#[tauri::command(async)]
fn install_step(step: String) -> Result<install::StepState, String> {
    let parsed = install::Step::parse(&step).ok_or_else(|| format!("unknown step {step:?}"))?;
    install::install(parsed)
}

#[tauri::command(async)]
fn uninstall_step(step: String) -> Result<install::StepState, String> {
    let parsed = install::Step::parse(&step).ok_or_else(|| format!("unknown step {step:?}"))?;
    install::uninstall(parsed)
}

#[tauri::command]
fn reveal_path(app: AppHandle, path: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .open_path(path, None::<&str>)
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Live events
// ---------------------------------------------------------------------------

/// One long-lived subscription, forwarded to the webview.
///
/// It reconnects on its own rather than surfacing a failure, because the daemon restarting
/// is a normal event — an upgrade, a reboot — and the app should simply pick it back up.
/// The frontend learns about the gap from the daemon-health dot, not an error dialog.
fn spawn_event_bridge(app: AppHandle) {
    std::thread::spawn(move || loop {
        match Client::connect_or_start().and_then(|c| c.subscribe(None)) {
            Ok(mut sub) => {
                let _ = app.emit("runlog://connected", true);
                while let Ok(Some(event)) = sub.next() {
                    if let Event::Notice { notice } = &event {
                        post_notification(&app, notice);
                    }
                    if app.emit(EVENT, &event).is_err() {
                        return; // the window is gone
                    }
                }
                let _ = app.emit("runlog://connected", false);
            }
            Err(_) => {
                let _ = app.emit("runlog://connected", false);
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    });
}

/// The app posts notifications rather than the daemon whenever it is running: it has an
/// application identity and an icon, which a headless daemon does not.
fn post_notification(app: &AppHandle, notice: &Notice) {
    let _ = app
        .notification()
        .builder()
        .title(&notice.title)
        .body(if notice.count > 1 {
            format!("{} (×{})", notice.body, notice.count)
        } else {
            notice.body.clone()
        })
        .show();
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .manage(Backend::new())
        .setup(|app| {
            // Off the main thread: linking a binary and registering a launch agent both
            // shell out, and a window that will not paint until that finishes reads as a
            // hang on exactly the launch where first impressions are formed.
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                if let Some(done) = install::first_run_setup() {
                    for (what, outcome) in &done {
                        match outcome {
                            Ok(state) => eprintln!("runlog: {what} — {}", state.detail),
                            Err(e) => eprintln!("runlog: {what} failed — {e}"),
                        }
                    }
                    let _ = handle.emit("runlog://first-run", install::status());
                }
            });
            spawn_event_bridge(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            daemon_status,
            list_projects,
            project_status,
            list_runs,
            get_run,
            entries,
            fingerprints,
            fingerprint_history,
            run_diff,
            raw_stream,
            get_settings,
            update_settings,
            update_project,
            retention,
            reparse,
            delete_run,
            query,
            install_status,
            install_step,
            uninstall_step,
            reveal_path,
        ])
        .run(tauri::generate_context!())
        .expect("error while running runlog");
}
