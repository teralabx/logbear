//! One connection handler.
//!
//! The three kinds of client share one socket and are told apart by what they send first: a
//! connection that sends `StartRun` becomes an ingest stream, one that sends `Subscribe`
//! becomes an event feed, and everything else is request/response.

use crate::state::{Daemon, LiveRun, ProjectHandle};
use logbear_core::model::*;
use logbear_core::proto::*;
use logbear_core::store::{read, retention, sandbox, schema};
use logbear_core::{diff, Error, Result};
use std::io::{BufReader, BufWriter, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;

pub fn serve(daemon: Arc<Daemon>, stream: UnixStream) {
    let peer = stream.try_clone();
    let mut reader = BufReader::new(stream);
    let Ok(write_half) = peer else { return };
    let mut writer = BufWriter::new(write_half);

    let mut ingest: Option<(Arc<ProjectHandle>, Arc<LiveRun>)> = None;

    loop {
        let frame = match Frame::read_from(&mut reader) {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(_) => break,
        };

        match frame {
            Frame::Data { captured_at, bytes } => {
                if let Some((handle, live)) = &ingest {
                    // A storage failure must not stop the stream: the user's process is
                    // still running and still emitting output.
                    let _ = daemon.ingest(handle, live, &bytes, captured_at);
                }
            }
            Frame::Json(payload) => {
                let request: Request = match serde_json::from_slice(&payload) {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = reply(&mut writer, Response::err("bad_request", e.to_string()));
                        continue;
                    }
                };

                match request {
                    Request::StartRun { stamp } => match daemon.start_run(&stamp) {
                        Ok((handle, live)) => {
                            let run_id = live.run_id;
                            ingest = Some((handle, live));
                            if reply(&mut writer, Response::ok(run_id)).is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            let _ = reply(&mut writer, Response::err("start_run", e.to_string()));
                        }
                    },

                    Request::FinishRun {
                        exit_code, signal, ..
                    } => {
                        if let Some((handle, live)) = ingest.take() {
                            let _ =
                                daemon.finish_run(&handle, &live, exit_code, signal.as_deref());
                        }
                        let _ = reply(&mut writer, Response::ok(()));
                        break;
                    }

                    Request::Subscribe { project } => {
                        let project_id = project
                            .as_ref()
                            .and_then(|p| daemon.resolve(p).ok())
                            .map(|h| h.project.id);
                        if reply(&mut writer, Response::ok(())).is_err() {
                            break;
                        }
                        stream_events(&daemon, &mut writer, project_id);
                        break;
                    }

                    Request::RawStream {
                        project,
                        run_id,
                        start_line,
                        end_line,
                        tail_lines,
                    } => {
                        let outcome = stream_raw(
                            &daemon,
                            &mut writer,
                            &project,
                            run_id,
                            start_line,
                            end_line,
                            tail_lines,
                        );
                        let response = match outcome {
                            Ok(()) => Response::ok(()),
                            Err(e) => Response::err("raw_stream", e.to_string()),
                        };
                        if reply(&mut writer, response).is_err() {
                            break;
                        }
                    }

                    Request::Shutdown => {
                        let _ = reply(&mut writer, Response::ok(()));
                        daemon
                            .shutdown
                            .store(true, std::sync::atomic::Ordering::SeqCst);
                        break;
                    }

                    other => {
                        let response = match handle(&daemon, other) {
                            Ok(v) => Response::Ok { data: v },
                            Err(e) => Response::err(kind_of(&e), e.to_string()),
                        };
                        if reply(&mut writer, response).is_err() {
                            break;
                        }
                    }
                }
            }
        }
    }

    // The shim died without saying goodbye — a crash, a kill -9, a closed laptop. Close
    // the run with no exit code so it reads as an unexpected death rather than staying
    // "still running" forever.
    if let Some((handle, live)) = ingest {
        let _ = daemon.finish_run(&handle, &live, None, None);
    }
}

fn kind_of(e: &Error) -> &'static str {
    match e {
        Error::NoSuchRun(_) => "no_such_run",
        Error::NoSuchProject(_) => "no_such_project",
        Error::QueryRejected(_) => "query_rejected",
        Error::QueryTimeout(_) => "query_timeout",
        _ => "error",
    }
}

fn reply(w: &mut BufWriter<UnixStream>, response: Response) -> Result<()> {
    Frame::json(&response)?.write_to(w)?;
    w.flush()?;
    Ok(())
}

/// How recently a hook session must have been seen for a read receipt to apply to it.
const RECEIPT_WINDOW_MICROS: i64 = 30 * 60 * 1_000_000;

/// Best-effort by design: a receipt failure must never fail the read that produced it.
fn receipt(h: &ProjectHandle, fps: Vec<FingerprintId>) {
    if fps.is_empty() {
        return;
    }
    if let Err(e) = read::record_seen_for_active(&h.db, &fps, RECEIPT_WINDOW_MICROS) {
        eprintln!("logbeard: read receipt failed ({e})");
    }
}

fn error_fps(views: &[EntryView]) -> Vec<FingerprintId> {
    views
        .iter()
        .filter(|v| v.entry.is_error)
        .filter_map(|v| v.entry.fingerprint_id)
        .collect()
}

fn handle(daemon: &Arc<Daemon>, request: Request) -> Result<serde_json::Value> {
    use serde_json::to_value;

    Ok(match request {
        Request::Ping => to_value("pong")?,

        Request::Status => {
            let projects = daemon.index.list()?;
            let total: i64 = daemon.open_projects().iter().map(|p| p.db.size_bytes()).sum();
            to_value(DaemonStatus {
                version: env!("CARGO_PKG_VERSION").to_string(),
                pid: std::process::id(),
                started_at: daemon.started_at,
                socket: logbear_core::paths::socket().to_string_lossy().into_owned(),
                projects: projects.len(),
                active_runs: daemon.active_run_count(),
                total_db_bytes: total,
                spool_pending: count_spool(),
            })?
        }

        Request::ListProjects => to_value(daemon.index.list()?)?,

        Request::ProjectStatus { project } => {
            let h = daemon.resolve(&project)?;
            let hour_ago = logbear_core::now_micros() - 3_600_000_000;
            to_value(ProjectStatus {
                active_runs: read::active_runs(&h.db)?,
                last_run: read::list_runs(&h.db, 1, 0)?.into_iter().next(),
                db_bytes: h.db.size_bytes(),
                errors_last_hour: read::errors_since(&h.db, hour_ago)?,
                unacked_errors: read::errors_since(&h.db, hour_ago)?,
                project: h.project.clone(),
            })?
        }

        Request::ListRuns {
            project,
            limit,
            offset,
        } => {
            let h = daemon.resolve(&project)?;
            to_value(read::list_runs(&h.db, limit, offset)?)?
        }

        Request::GetRun { project, run_id } => {
            let h = daemon.resolve(&project)?;
            to_value(read::get_run(&h.db, run_id)?)?
        }

        Request::Entries { project, filter } => {
            let h = daemon.resolve(&project)?;
            let views = read::entries(&h.db, &filter)?;
            if !filter.unobserved {
                receipt(&h, error_fps(&views));
            }
            to_value(views)?
        }

        Request::Fingerprints {
            project,
            limit,
            errors_only,
            unobserved,
        } => {
            let h = daemon.resolve(&project)?;
            let fps = read::fingerprints(&h.db, limit, errors_only)?;
            if !unobserved {
                let ids: Vec<FingerprintId> = fps
                    .iter()
                    .filter(|f| matches!(f.level, Level::Error))
                    .map(|f| f.id)
                    .collect();
                receipt(&h, ids);
            }
            to_value(fps)?
        }

        Request::FingerprintHistories {
            project,
            fingerprint_ids,
            limit,
        } => {
            let h = daemon.resolve(&project)?;
            to_value(read::fingerprint_histories(&h.db, &fingerprint_ids, limit.min(500))?)?
        }

        Request::RunErrorCounts { project, run_ids } => {
            let h = daemon.resolve(&project)?;
            to_value(read::run_error_counts(&h.db, &run_ids)?)?
        }

        Request::Query {
            project,
            sql,
            params,
            max_rows,
        } => {
            let h = daemon.resolve(&project)?;
            // Runs on its own read-only connection, so an agent's SQL cannot touch the
            // daemon's write handle or any other project.
            let result = sandbox::query(h.db.path(), &sql, &params, max_rows)?;
            match read::query_receipt_fps(&h.db, &result.columns, &result.rows) {
                Ok(fps) => receipt(&h, fps),
                Err(e) => eprintln!("logbeard: query receipt failed ({e})"),
            }
            to_value(result)?
        }

        Request::Schema { project } => {
            let h = daemon.resolve(&project)?;
            let cap = daemon.retention_cap(&h.project);
            let state = retention::state(&h.db, cap)?;
            let mode = read::list_runs(&h.db, 1, 0)?
                .first()
                .map(|r| r.delimiter_mode)
                .unwrap_or(DelimiterMode::Fallback);
            to_value(format!(
                "{}\n{}\n{}\n\n## This project right now\n\n\
                 Delimiter mode: {} (entry boundaries are {})\n\
                 Storage: {} of {} used, {} runs recorded, {} with their raw log evicted.\n",
                schema::PROJECT_SCHEMA,
                schema::PROJECT_VIEWS,
                schema::AGENT_NOTES,
                mode.as_str(),
                mode.boundaries().as_str(),
                human_bytes(state.bytes_used),
                human_bytes(state.bytes_cap),
                state.runs_total,
                state.runs_evicted,
            ))?
        }

        Request::Diff { project, a, b } => {
            let h = daemon.resolve(&project)?;
            let d = diff::diff(&h.db, a, b)?;
            let fps: Vec<FingerprintId> = d
                .new_signatures
                .iter()
                .chain(d.resolved_signatures.iter())
                .chain(d.changed_counts.iter())
                .filter(|s| matches!(s.level, Level::Error))
                .map(|s| s.fingerprint_id)
                .collect();
            receipt(&h, fps);
            to_value(d)?
        }

        Request::Wait {
            project,
            timeout_ms,
            level,
            after_entry,
        } => {
            let h = daemon.resolve(&project)?;
            // Anything that landed while the caller was deciding to wait still counts, or
            // a fast-failing server would be missed by every wait that just started.
            let already = read::entries(
                &h.db,
                &EntryFilter {
                    level,
                    after_entry,
                    errors_only: level.is_none(),
                    ascending: true,
                    limit: 50,
                    ..Default::default()
                },
            )?;
            let served = if !already.is_empty() && after_entry.is_some() {
                already
            } else {
                daemon.wait(h.project.id, timeout_ms.min(600_000), level, after_entry)
            };
            receipt(&h, error_fps(&served));
            to_value(served)?
        }

        Request::Unread {
            project,
            session_id,
            harness,
        } => {
            let h = daemon.resolve(&project)?;
            let s = daemon.settings();
            let mut state = read::unread(
                &h.db,
                &session_id,
                &harness,
                s.block_budget,
                s.change_block_budget,
                20,
            )?;
            state.push_enabled = h.project.push_enabled && s.push_enabled;
            to_value(state)?
        }

        Request::Ack {
            project,
            session_id,
            harness,
            entry_id,
            kind,
        } => {
            let h = daemon.resolve(&project)?;
            let s = daemon.settings();
            if kind.is_block() {
                // Record the reported signatures before advancing, so a repeat of the same
                // one can never block this session twice.
                let reported = read::unread(
                    &h.db,
                    &session_id,
                    &harness,
                    s.block_budget,
                    s.change_block_budget,
                    50,
                )?;
                let fps: Vec<FingerprintId> = reported
                    .entries
                    .iter()
                    .filter_map(|e| e.entry.fingerprint_id)
                    .collect();
                read::record_block(&h.db, &session_id, &fps)?;
            }
            read::ack(&h.db, &session_id, &harness, entry_id, kind)?;
            let mut state = read::unread(
                &h.db,
                &session_id,
                &harness,
                s.block_budget,
                s.change_block_budget,
                20,
            )?;
            state.push_enabled = h.project.push_enabled && s.push_enabled;
            to_value(state)?
        }

        Request::GetSettings => to_value(daemon.settings())?,

        Request::UpdateSettings { settings } => {
            daemon.set_settings(settings)?;
            to_value(())?
        }

        Request::UpdateProject {
            project,
            retention_bytes,
            push_enabled,
            notify_enabled,
        } => {
            let h = daemon.resolve(&project)?;
            daemon.index.update_config(
                h.project.id,
                retention_bytes,
                push_enabled,
                notify_enabled,
            )?;
            daemon.refresh_project(h.project.id)?;
            daemon.broadcast(Event::ProjectsChanged);
            to_value(())?
        }

        Request::Vacuum { project } => {
            let h = daemon.resolve(&project)?;
            let cap = daemon.retention_cap(&h.project);
            let sweep = retention::enforce(&h.db, cap)?;
            retention::reclaim(&h.db)?;
            to_value(sweep)?
        }

        Request::Reparse { project, run_id } => {
            let h = daemon.resolve(&project)?;
            let n = match run_id {
                Some(id) => retention::reparse_run(&h.db, id)?,
                None => retention::reparse_all(&h.db)?,
            };
            to_value(n)?
        }

        Request::Squash { project, run_id } => {
            let h = daemon.resolve(&project)?;
            let report = match run_id {
                Some(id) => retention::squash_one(&h.db, id)?,
                None => retention::squash_all(&h.db)?,
            };
            if report.runs > 0 {
                daemon.broadcast(Event::ProjectsChanged);
            }
            to_value(report)?
        }

        Request::DeleteRun { project, run_id } => {
            let h = daemon.resolve(&project)?;
            retention::delete_run(&h.db, run_id)?;
            daemon.index.forget_run(h.project.id, run_id)?;
            to_value(())?
        }

        Request::IngestSpool { path } => to_value(crate::spool::ingest(daemon, Path::new(&path))?)?,

        // Handled by the caller before dispatch.
        Request::StartRun { .. }
        | Request::FinishRun { .. }
        | Request::Resize { .. }
        | Request::RawStream { .. }
        | Request::Subscribe { .. }
        | Request::Shutdown => to_value(())?,
    })
}

fn stream_raw(
    daemon: &Arc<Daemon>,
    writer: &mut BufWriter<UnixStream>,
    project: &ProjectRef,
    run_id: RunId,
    start_line: Option<i64>,
    end_line: Option<i64>,
    tail_lines: Option<i64>,
) -> Result<()> {
    let h = daemon.resolve(project)?;
    let run = read::get_run(&h.db, run_id)?;
    if run.chunks_evicted {
        return Err(Error::other(
            "the raw log for this run was evicted by the retention cap; its summary and error signatures are still available",
        ));
    }
    let end = end_line.unwrap_or(i64::MAX);
    let start = match tail_lines {
        Some(n) => (read::line_count(&h.db, run_id)?.min(end.saturating_add(1)) - n.max(0)).max(0),
        None => start_line.unwrap_or(0),
    };

    // Paged rather than one enormous frame: a long dev-server run is hundreds of megabytes
    // and the reader wants to start painting before it has all of it.
    const PAGE: i64 = 2000;
    let mut at = start;
    loop {
        let stop = at.saturating_add(PAGE).min(end);
        let bytes = h.db.read(|c| logbear_core::store::raw_range(c, run_id, at, stop))?;
        if bytes.is_empty() {
            break;
        }
        Frame::Data {
            captured_at: 0,
            bytes,
        }
        .write_to(writer)?;
        if stop >= end {
            break;
        }
        at = stop + 1;
    }
    writer.flush()?;
    Ok(())
}

fn stream_events(
    daemon: &Arc<Daemon>,
    writer: &mut BufWriter<UnixStream>,
    project_id: Option<ProjectId>,
) {
    let (id, rx) = daemon.subscribe(project_id);
    loop {
        match rx.recv_timeout(std::time::Duration::from_secs(30)) {
            Ok(event) => {
                if Frame::json(&event)
                    .and_then(|f| {
                        f.write_to(writer)?;
                        writer.flush()?;
                        Ok(())
                    })
                    .is_err()
                {
                    break;
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                // Writing zero bytes still detects a peer that has gone away, which is how
                // a closed app window is noticed.
                if writer.flush().is_err() {
                    break;
                }
                if daemon.shutdown.load(std::sync::atomic::Ordering::SeqCst) {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    daemon.unsubscribe(id);
}

fn count_spool() -> usize {
    std::fs::read_dir(logbear_core::paths::spool_dir())
        .map(|d| d.filter_map(|e| e.ok()).count())
        .unwrap_or(0)
}

fn human_bytes(n: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_sizes_read_like_a_person_wrote_them() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(1024 * 1024 * 1024), "1.0 GB");
    }
}
