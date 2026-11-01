//! The query subcommands.
//!
//! Human output is the default and `--json` is for scripts. When the daemon is down the
//! answer is one sentence saying so and how to start it, never a Rust error dump.

use crate::cli::Cmd;
use crate::ui::{self, Cell};
use logbear_core::client::Client;
use logbear_core::model::*;
use logbear_core::paths;
use logbear_core::proto::{EntryFilter, ProjectRef, QueryResult, RunDiff, RunRef};
use std::io::Write;

pub fn dispatch(cmd: Cmd) -> Result<i32, String> {
    match cmd {
        Cmd::Cat { run_id } => cat(run_id),
        Cmd::Logs {
            number,
            level,
            errors,
            new,
            since,
            search,
            run,
            json,
        } => logs(number, level, errors, new, since, search, run, json),
        Cmd::Runs { number, json } => runs(number, json),
        Cmd::Diff { a, b, json } => diff(a, b, json),
        Cmd::Status { json } => status(json),
        Cmd::Doctor { json } => crate::doctor::run(json),
        Cmd::Query { sql, json } => query(&sql, json),
        Cmd::SqlSchema => sql_schema(),
    }
}

/// Connects to the daemon without starting it: `logbear status` must report whether it is
/// running, not change the answer by asking.
fn connect() -> Result<Client, String> {
    Client::connect().map_err(|_| ui::daemon_down(&paths::socket()))
}

fn failed(e: logbear_core::Error) -> String {
    format!("logbear: {e}")
}

fn print_json<T: serde::Serialize>(value: &T) -> Result<i32, String> {
    let text = serde_json::to_string_pretty(value).map_err(|e| format!("logbear: {e}"))?;
    println!("{text}");
    Ok(0)
}

// ---------------------------------------------------------------------------
// cat
// ---------------------------------------------------------------------------

fn cat(run_id: RunId) -> Result<i32, String> {
    let mut client = connect()?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    // Byte-exact, ANSI intact: `logbear cat` is a replay, not a rendering. Stripping or
    // reflowing here would break the guarantee the capture side makes.
    client
        .raw_stream(ProjectRef::cwd(), run_id, |bytes| out.write_all(bytes))
        .map_err(failed)?;
    out.flush().map_err(|e| format!("logbear: {e}"))?;
    Ok(0)
}

// ---------------------------------------------------------------------------
// logs
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn logs(
    number: usize,
    level: Option<String>,
    errors: bool,
    new: bool,
    since: Option<String>,
    search: Option<String>,
    run: Option<RunId>,
    json: bool,
) -> Result<i32, String> {
    let since_micros = match since.as_deref() {
        None => None,
        Some(s) => {
            let d = ui::parse_duration(s)
                .ok_or_else(|| format!("logbear: `{s}` is not a duration (try 30s, 10m, 2h, 1d)"))?;
            Some(logbear_core::now_micros() - d.as_micros() as i64)
        }
    };

    let filter = EntryFilter {
        run_id: run,
        level: level.as_deref().map(Level::parse),
        errors_only: errors,
        novel_only: new,
        since: since_micros,
        after_entry: None,
        search,
        limit: number,
        offset: 0,
        ascending: false,
        ..Default::default()
    };

    let mut client = connect()?;
    let mut entries = client
        .entries(ProjectRef::cwd(), filter)
        .map_err(failed)?;

    if json {
        return print_json(&entries);
    }

    // Fetched newest-first so the limit takes the recent end, printed oldest-first so it
    // reads like a log.
    entries.reverse();
    if entries.is_empty() {
        println!("{}", ui::ash("no entries"));
        return Ok(0);
    }
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for view in &entries {
        write_entry(&mut out, view);
    }
    Ok(0)
}

/// Width of the widest level name, so the text column does not shift between lines.
const LEVEL_WIDTH: usize = 7;

fn write_entry(out: &mut impl Write, view: &EntryView) {
    let time = ui::local_hms(view.entry.captured_at);
    let level = view.entry.level;
    let tag = format!("{:<LEVEL_WIDTH$}", level.as_str());
    let marker = if view.is_new_signature && view.entry.is_error {
        ui::flare(" NEW")
    } else {
        String::new()
    };
    // Continuation lines line up under the text rather than the gutter, so a stack trace
    // does not re-indent itself on every line.
    let indent = " ".repeat("00:00:00".len() + 1 + LEVEL_WIDTH + 1);
    let mut lines = view.text.lines();
    let first = lines.next().unwrap_or("");
    let _ = writeln!(
        out,
        "{} {} {}{}",
        ui::ash(&time),
        ui::paint(ui::level_colour(level), &tag),
        first,
        marker
    );
    for line in lines {
        let _ = writeln!(out, "{indent}{line}");
    }
}

// ---------------------------------------------------------------------------
// runs
// ---------------------------------------------------------------------------

fn runs(number: usize, json: bool) -> Result<i32, String> {
    let mut client = connect()?;
    let runs = client
        .runs(ProjectRef::cwd(), number, 0)
        .map_err(failed)?;

    if json {
        return print_json(&runs);
    }
    if runs.is_empty() {
        println!(
            "{}",
            ui::ash("no runs recorded for this project yet — try `logbear mix phx.server`")
        );
        return Ok(0);
    }

    let new_counts = new_signature_counts(&mut client);
    let rows: Vec<Vec<Cell>> = runs
        .iter()
        .map(|run| {
            let git = match (&run.git_sha, &run.git_branch) {
                (None, _) => "-".to_string(),
                (sha, branch) => format!(
                    "{}{} {}",
                    ui::short_sha(sha.as_ref()),
                    if run.git_dirty_hash.is_some() { "*" } else { "" },
                    branch.as_deref().unwrap_or("")
                )
                .trim_end()
                .to_string(),
            };
            let duration = match run.duration_micros() {
                Some(d) => ui::duration(d),
                None => "running".into(),
            };
            let (exit, exit_colour) = match (&run.signal, run.exit_code) {
                (Some(sig), _) => (sig.clone(), ui::FLARE),
                (None, Some(0)) => ("0".to_string(), ui::CLEAR),
                (None, Some(c)) => (c.to_string(), ui::FLARE),
                (None, None) => ("-".to_string(), ui::ASH),
            };
            let (errors, error_colour) = if run.counters.n_error > 0 {
                let new = new_counts
                    .as_ref()
                    .and_then(|m| m.get(&run.id).copied())
                    .unwrap_or(0);
                let suffix = if new > 0 { format!(" ({new} new)") } else { String::new() };
                (
                    format!("{} error{}{suffix}", run.counters.n_error, plural(run.counters.n_error)),
                    ui::FLARE,
                )
            } else if run.is_clean() {
                ("clean".to_string(), ui::CLEAR)
            } else {
                ("-".to_string(), ui::ASH)
            };

            vec![
                Cell::coloured(format!("#{}", run.id), ui::ASH),
                Cell::plain(ui::rel_time(run.started_at)),
                Cell::plain(truncate(&run.command_line(), 38)),
                Cell::coloured(git, ui::ASH),
                Cell::plain(duration),
                Cell::coloured(exit, exit_colour),
                Cell::coloured(errors, error_colour),
            ]
        })
        .collect();

    print!(
        "{}",
        ui::table(
            &["RUN", "STARTED", "COMMAND", "GIT", "TOOK", "EXIT", "ERRORS"],
            &rows
        )
    );
    Ok(0)
}

/// Per-run count of error signatures first seen in that run, for the "(M new)" suffix.
///
/// Asked as SQL because there is no request for it; if the daemon refuses, the column
/// loses its parenthetical rather than the command failing.
fn new_signature_counts(client: &mut Client) -> Option<std::collections::HashMap<RunId, i64>> {
    const SQL: &str = "SELECT rf.run_id, COUNT(*) \
                         FROM run_fingerprints rf \
                         JOIN fingerprints f ON f.id = rf.fingerprint_id \
                        WHERE f.level = 'error' AND f.first_seen >= rf.first_at \
                        GROUP BY rf.run_id";
    let result: QueryResult = client
        .query(ProjectRef::cwd(), SQL, Vec::new(), Some(2000))
        .ok()?;
    Some(
        result
            .rows
            .iter()
            .filter_map(|row| Some((row.first()?.as_i64()?, row.get(1)?.as_i64()?)))
            .collect(),
    )
}

fn plural(n: i64) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

// ---------------------------------------------------------------------------
// diff
// ---------------------------------------------------------------------------

/// Parses a run id, or one of `latest`, `last-good`, `last-bad`.
pub fn parse_run_ref(s: &str) -> Result<RunRef, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "latest" | "last" | "current" => Ok(RunRef::Latest),
        "last-good" | "lastgood" | "good" => Ok(RunRef::LastGood),
        "last-bad" | "lastbad" | "bad" => Ok(RunRef::LastBad),
        other => other
            .trim_start_matches('#')
            .parse::<i64>()
            .map(RunRef::Id)
            .map_err(|_| {
                format!("logbear: `{s}` is not a run id or one of latest, last-good, last-bad")
            }),
    }
}

fn diff(a: Option<String>, b: Option<String>, json: bool) -> Result<i32, String> {
    let a = a.as_deref().map(parse_run_ref).transpose()?;
    let b = b.as_deref().map(parse_run_ref).transpose()?;

    let mut client = connect()?;
    let diff = client.diff(ProjectRef::cwd(), a, b).map_err(failed)?;
    if json {
        return print_json(&diff);
    }
    print_diff(&diff);
    // Non-zero when something regressed, so `logbear diff && deploy` means what it looks
    // like.
    Ok(if diff.new_signatures.is_empty() { 0 } else { 1 })
}

fn print_diff(diff: &RunDiff) {
    let describe = |run: &Option<Run>| match run {
        Some(r) => match &r.git_sha {
            Some(_) => format!(
                "#{} {} ({})",
                r.id,
                ui::short_sha(r.git_sha.as_ref()),
                ui::rel_time(r.started_at)
            ),
            None => format!("#{} ({})", r.id, ui::rel_time(r.started_at)),
        },
        None => "nothing".to_string(),
    };
    println!(
        "{} {} {} {}",
        ui::ash("comparing"),
        describe(&diff.a),
        ui::ash("→"),
        describe(&diff.b)
    );

    if diff.new_signatures.is_empty()
        && diff.resolved_signatures.is_empty()
        && diff.changed_counts.is_empty()
    {
        println!("{}", ui::clear("no change in error signatures"));
    }

    if !diff.new_signatures.is_empty() {
        println!("\n{}", ui::bold(&format!("new ({})", diff.new_signatures.len())));
        for sig in &diff.new_signatures {
            println!(
                "  {} {} {}",
                ui::flare("✗"),
                ui::ash(&format!("[{}]", sig.level.as_str())),
                truncate(&sig.template, 110)
            );
            if let Some(sample) = &sig.sample {
                for line in sample.lines().take(3) {
                    println!("      {}", ui::ash(line));
                }
            }
        }
    }

    if !diff.resolved_signatures.is_empty() {
        println!(
            "\n{}",
            ui::bold(&format!("gone ({})", diff.resolved_signatures.len()))
        );
        for sig in &diff.resolved_signatures {
            println!("  {} {}", ui::clear("✓"), truncate(&sig.template, 110));
        }
    }

    if !diff.changed_counts.is_empty() {
        println!("\n{}", ui::bold("counts changed"));
        for sig in &diff.changed_counts {
            println!(
                "  {} {}",
                ui::ash(&format!("{} → {}", sig.count_a, sig.count_b)),
                truncate(&sig.template, 100)
            );
        }
    }

    if !diff.git.commits.is_empty() || !diff.git.files_changed.is_empty() {
        println!(
            "\n{}",
            ui::bold(&format!(
                "git ({} commit{}, {} file{})",
                diff.git.commits.len(),
                plural(diff.git.commits.len() as i64),
                diff.git.files_changed.len(),
                plural(diff.git.files_changed.len() as i64)
            ))
        );
        for commit in diff.git.commits.iter().take(10) {
            println!(
                "  {}  {}  {}",
                ui::ash(&commit.sha.chars().take(7).collect::<String>()),
                truncate(&commit.subject, 70),
                ui::ash(&commit.author)
            );
        }
    }
    if !diff.lockfiles_changed.is_empty() {
        println!("\n{}", ui::bold("lockfiles"));
        for lock in &diff.lockfiles_changed {
            println!("  {}", lock.file);
        }
    }
    if !diff.env_added.is_empty() || !diff.env_removed.is_empty() {
        println!("\n{}", ui::bold("environment"));
        for key in &diff.env_added {
            println!("  {} {key}", ui::clear("+"));
        }
        for key in &diff.env_removed {
            println!("  {} {key}", ui::flare("-"));
        }
    }
    if diff.command_changed {
        println!("\n{}", ui::ash("the command itself changed between these runs"));
    }
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

fn status(json: bool) -> Result<i32, String> {
    let mut client = match Client::connect() {
        Ok(c) => c,
        Err(_) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "daemon": "stopped",
                        "socket": paths::socket().to_string_lossy(),
                    })
                );
                return Ok(1);
            }
            println!("{}", ui::daemon_down(&paths::socket()));
            return Ok(1);
        }
    };

    let daemon = client.status().map_err(failed)?;
    // A project the daemon has never seen is not an error, just one nobody has run
    // anything in yet.
    let project = client.project_status(ProjectRef::cwd()).ok();

    if json {
        return print_json(&serde_json::json!({
            "daemon": daemon,
            "project": project,
        }));
    }

    let mut rows = vec![
        vec![
            Cell::plain("daemon"),
            Cell::coloured(
                format!(
                    "running · v{} · pid {} · up {}",
                    daemon.version,
                    daemon.pid,
                    ui::duration(logbear_core::now_micros() - daemon.started_at)
                ),
                ui::CLEAR,
            ),
        ],
        vec![Cell::plain("socket"), Cell::plain(daemon.socket.clone())],
        vec![
            Cell::plain("tracking"),
            Cell::plain(format!(
                "{} project{} · {} active run{}",
                daemon.projects,
                plural(daemon.projects as i64),
                daemon.active_runs,
                plural(daemon.active_runs as i64)
            )),
        ],
    ];
    if daemon.spool_pending > 0 {
        rows.push(vec![
            Cell::plain("spool"),
            Cell::coloured(
                format!("{} file(s) waiting to be ingested", daemon.spool_pending),
                ui::EMBER,
            ),
        ]);
    }

    match project {
        Some(p) => {
            let cap = p
                .project
                .retention_bytes
                .map(ui::bytes)
                .unwrap_or_else(|| "the global default".into());
            rows.push(vec![
                Cell::plain("project"),
                Cell::plain(format!("{}  {}", p.project.name, ui::ash(&p.project.root))),
            ]);
            rows.push(vec![
                Cell::plain("storage"),
                Cell::plain(format!("{} of {}", ui::bytes(p.db_bytes), cap)),
            ]);
            if let Some(last) = &p.last_run {
                rows.push(vec![
                    Cell::plain("last run"),
                    Cell::plain(format!(
                        "#{} {} · {}",
                        last.id,
                        truncate(&last.command_line(), 40),
                        ui::rel_time(last.started_at)
                    )),
                ]);
            }
            let colour = if p.errors_last_hour > 0 { ui::FLARE } else { ui::CLEAR };
            rows.push(vec![
                Cell::plain("errors"),
                Cell::coloured(
                    format!(
                        "{} in the last hour · {} unacknowledged",
                        p.errors_last_hour, p.unacked_errors
                    ),
                    colour,
                ),
            ]);
        }
        None => rows.push(vec![
            Cell::plain("project"),
            Cell::coloured("nothing recorded here yet", ui::ASH),
        ]),
    }
    rows.push(vec![
        Cell::plain("total"),
        Cell::plain(format!("{} on disk", ui::bytes(daemon.total_db_bytes))),
    ]);

    for row in &rows {
        println!("{:<10}{}", row[0].text, row[1].render_alone());
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// query / sql-schema
// ---------------------------------------------------------------------------

fn query(sql: &str, json: bool) -> Result<i32, String> {
    let mut client = connect()?;
    let result: QueryResult = client
        .query(ProjectRef::cwd(), sql, Vec::new(), None)
        .map_err(failed)?;

    if json {
        return print_json(&result);
    }
    if result.rows.is_empty() {
        println!("{}", ui::ash("no rows"));
        return Ok(0);
    }

    let headers: Vec<&str> = result.columns.iter().map(String::as_str).collect();
    let rows: Vec<Vec<Cell>> = result
        .rows
        .iter()
        .map(|row| row.iter().map(|v| Cell::plain(render_value(v))).collect())
        .collect();
    print!("{}", ui::table(&headers, &rows));

    let mut footer = format!(
        "{} row{} in {}ms",
        result.row_count,
        plural(result.row_count as i64),
        result.elapsed_ms
    );
    if result.truncated {
        footer.push_str(" · truncated");
    }
    println!("{}", ui::ash(&footer));
    Ok(0)
}

/// Strings print bare: this is a viewer, not a serialiser. `--json` is there when the
/// escaping matters.
fn render_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(s) => s.replace('\n', "⏎"),
        other => other.to_string(),
    }
}

fn sql_schema() -> Result<i32, String> {
    let mut client = connect()?;
    let schema = client.schema(ProjectRef::cwd()).map_err(failed)?;
    println!("{schema}");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_refs_accept_the_names_people_actually_use() {
        assert!(matches!(parse_run_ref("latest"), Ok(RunRef::Latest)));
        assert!(matches!(parse_run_ref("LATEST"), Ok(RunRef::Latest)));
        assert!(matches!(parse_run_ref("last-good"), Ok(RunRef::LastGood)));
        assert!(matches!(parse_run_ref("lastgood"), Ok(RunRef::LastGood)));
        assert!(matches!(parse_run_ref("last-bad"), Ok(RunRef::LastBad)));
        assert!(matches!(parse_run_ref("12"), Ok(RunRef::Id(12))));
        assert!(matches!(parse_run_ref("#12"), Ok(RunRef::Id(12))));
    }

    #[test]
    fn a_bad_run_ref_explains_the_alternatives() {
        let err = parse_run_ref("yesterday").unwrap_err();
        assert!(err.contains("last-good"), "{err}");
    }

    #[test]
    fn long_commands_are_elided_not_wrapped() {
        assert_eq!(truncate("short", 10), "short");
        let long = truncate("aaaaaaaaaaaaaaaaaaaa", 5);
        assert_eq!(long.chars().count(), 5);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn sql_values_render_for_eyes_not_for_parsers() {
        assert_eq!(render_value(&serde_json::Value::Null), "");
        assert_eq!(render_value(&serde_json::json!("a\nb")), "a⏎b");
        assert_eq!(render_value(&serde_json::json!(12)), "12");
    }
}
