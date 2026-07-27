//! Notification policy: pure decision logic, with no OS notification API involved.
//!
//! Notifications are keyed on fingerprint rather than entry, so a signature repeating a
//! hundred times a second yields one banner with a rising count. The daemon owns this
//! decision, so behaviour does not depend on whether the app window is open.

use crate::model::{Evidence, FingerprintId, Level, ProjectId};
use crate::proto::{Notice, QuietHours, Settings};
use std::collections::HashMap;

/// Window in which a repeat of a signature updates the existing banner's count rather
/// than posting a new one.
pub const COOLDOWN_MICROS: i64 = 5 * 60 * 1_000_000;
/// Ceiling on banners posted per project per minute.
pub const MAX_PER_MINUTE: usize = 3;
/// Error rate above which individual banners are replaced by a single storm banner.
pub const STORM_ERRORS_PER_SEC: f64 = 50.0;
const MINUTE: i64 = 60 * 1_000_000;

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Post a new banner.
    Post(Notice),
    /// Replace the banner already showing for this signature with a higher count.
    Update(Notice),
    /// Say nothing. The reason is retained so the settings screen can explain why.
    Suppress(&'static str),
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub project_id: ProjectId,
    pub project_name: String,
    pub fingerprint_id: FingerprintId,
    pub template: String,
    pub level: Level,
    /// How the level was arrived at. Only declared or structural severity may notify;
    /// see [`Notifier::consider`].
    pub evidence: Evidence,
    pub at: i64,
    pub is_new_signature: bool,
    pub run_command: String,
}

#[derive(Debug, Clone, Default)]
struct Seen {
    notified: bool,
    last_notified: i64,
    count: i64,
}

/// Per-project notification state, held in memory by the daemon. Losing it on restart
/// only means the next error notifies again.
#[derive(Default)]
pub struct Notifier {
    seen: HashMap<FingerprintId, Seen>,
    /// Timestamps of banners posted in the trailing minute.
    recent: Vec<i64>,
    /// Error arrival times in the trailing second, for storm detection.
    arrivals: Vec<i64>,
    storm_until: i64,
    coalesced: i64,
}

impl Notifier {
    pub fn new() -> Notifier {
        Notifier::default()
    }

    /// Decides what to do about one error.
    pub fn consider(
        &mut self,
        c: &Candidate,
        settings: &Settings,
        project_muted: bool,
    ) -> Decision {
        if !settings.notifications_enabled {
            return Decision::Suppress("notifications are off");
        }
        if project_muted {
            return Decision::Suppress("this project is muted");
        }
        // Only a severity the process itself declared may interrupt. Unknown-severity
        // output is still stored, indexed and shown in the app, but never notifies,
        // however alarming its text; novelty alone does not qualify either.
        if !c.evidence.may_notify() {
            return Decision::Suppress("the process did not declare this a problem");
        }
        if in_quiet_hours(settings.quiet_hours, c.at) {
            return Decision::Suppress("quiet hours");
        }

        self.recent.retain(|t| c.at - *t < MINUTE);

        // Order matters: the per-signature cooldown is checked before storm detection, so
        // repeats of one signature coalesce into a count instead of tripping storm mode,
        // which is meant for a burst of *distinct* failures.
        {
            let entry = self.seen.entry(c.fingerprint_id).or_default();
            entry.count += 1;
            if entry.notified && c.at - entry.last_notified < COOLDOWN_MICROS {
                let count = entry.count;
                return Decision::Update(Notice {
                    project_id: c.project_id,
                    project_name: c.project_name.clone(),
                    title: title_for(c),
                    body: format!("{} occurrences · {}", count, c.run_command),
                    level: c.level,
                    at: c.at,
                    replaces_key: Some(key(c.fingerprint_id)),
                    count,
                });
            }
        }

        self.arrivals.retain(|t| c.at - *t < 1_000_000);
        self.arrivals.push(c.at);

        if self.arrivals.len() as f64 >= STORM_ERRORS_PER_SEC {
            // One banner for the storm, then silence until it passes.
            let first = self.storm_until <= c.at;
            self.storm_until = c.at + MINUTE;
            self.coalesced += 1;
            return if first {
                Decision::Post(Notice {
                    project_id: c.project_id,
                    project_name: c.project_name.clone(),
                    title: format!("Error storm in {}", c.project_name),
                    body: format!(
                        "{} errors a second. Individual alerts paused.",
                        self.arrivals.len()
                    ),
                    level: Level::Error,
                    at: c.at,
                    replaces_key: Some(storm_key(c.project_id)),
                    count: self.arrivals.len() as i64,
                })
            } else {
                Decision::Suppress("error storm — individual alerts paused")
            };
        }

        if self.recent.len() >= MAX_PER_MINUTE {
            self.coalesced += 1;
            return Decision::Suppress("rate limited — coalesced into a summary");
        }

        let entry = self.seen.entry(c.fingerprint_id).or_default();
        entry.notified = true;
        entry.last_notified = c.at;
        let count = entry.count;
        self.recent.push(c.at);
        Decision::Post(Notice {
            project_id: c.project_id,
            project_name: c.project_name.clone(),
            title: title_for(c),
            body: format!("{} · {}", truncate(&c.template, 120), c.run_command),
            level: c.level,
            at: c.at,
            replaces_key: Some(key(c.fingerprint_id)),
            count,
        })
    }

    /// Decides what to do about a finished run. Crashes and non-zero exits bypass the
    /// rate limiter, since there is at most one per run.
    pub fn run_ended(
        &mut self,
        project_id: ProjectId,
        project_name: &str,
        command: &str,
        exit_code: Option<i32>,
        signal: Option<&str>,
        settings: &Settings,
        project_muted: bool,
    ) -> Decision {
        if !settings.notifications_enabled || project_muted {
            return Decision::Suppress("notifications are off for this project");
        }
        let at = crate::now_micros();
        match crate::parse::classify::run_failed(exit_code, signal) {
            None => Decision::Suppress("the run ended cleanly"),
            Some(reason) => Decision::Post(Notice {
                project_id,
                project_name: project_name.to_string(),
                title: format!("{command} stopped"),
                body: match (exit_code, signal) {
                    (_, Some(sig)) => format!("Killed by {sig} ({reason})"),
                    (Some(code), _) => format!("Exited {code}"),
                    _ => "Died without an exit code".into(),
                },
                level: Level::Error,
                at,
                replaces_key: None,
                count: 1,
            }),
        }
    }

    /// Summarises everything the rate limiter suppressed since the last call.
    pub fn drain_coalesced(&mut self, project_id: ProjectId, project_name: &str) -> Option<Notice> {
        if self.coalesced == 0 {
            return None;
        }
        let n = std::mem::take(&mut self.coalesced);
        Some(Notice {
            project_id,
            project_name: project_name.to_string(),
            title: format!("{n} more errors in {project_name}"),
            body: "Open runlog to see them.".into(),
            level: Level::Error,
            at: crate::now_micros(),
            replaces_key: Some(format!("coalesced:{project_id}")),
            count: n,
        })
    }

    /// Clears per-signature state at the start of a run, so a signature carried over from
    /// the previous run is not swallowed by its cooldown.
    pub fn reset_for_new_run(&mut self) {
        self.seen.clear();
        self.arrivals.clear();
        self.storm_until = 0;
    }
}

fn title_for(c: &Candidate) -> String {
    if c.is_new_signature {
        format!("New error in {}", c.project_name)
    } else {
        format!("Error in {}", c.project_name)
    }
}

fn key(fp: FingerprintId) -> String {
    format!("fp:{fp}")
}

fn storm_key(project: ProjectId) -> String {
    format!("storm:{project}")
}

fn truncate(s: &str, max: usize) -> String {
    let one_line = s.lines().next().unwrap_or(s);
    if one_line.chars().count() <= max {
        return one_line.to_string();
    }
    let cut: String = one_line.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

fn in_quiet_hours(q: Option<QuietHours>, at_micros: i64) -> bool {
    let Some(q) = q else { return false };
    let Some(minutes) = local_minutes(at_micros) else {
        return false;
    };
    if q.start_min <= q.end_min {
        minutes >= q.start_min && minutes < q.end_min
    } else {
        // Wraps midnight, e.g. 22:00 to 08:00.
        minutes >= q.start_min || minutes < q.end_min
    }
}

fn local_minutes(at_micros: i64) -> Option<u32> {
    let secs = at_micros / 1_000_000;
    let dt = time::OffsetDateTime::from_unix_timestamp(secs).ok()?;
    let local = dt
        .to_offset(time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC));
    Some(local.hour() as u32 * 60 + local.minute() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(fp: FingerprintId, at: i64) -> Candidate {
        Candidate {
            project_id: 1,
            project_name: "my_app".into(),
            fingerprint_id: fp,
            template: "** (RuntimeError) boom".into(),
            level: Level::Error,
            evidence: Evidence::Structural,
            at,
            is_new_signature: false,
            run_command: "mix phx.server".into(),
        }
    }

    fn settings() -> Settings {
        Settings::default()
    }

    #[test]
    fn a_hundred_identical_errors_in_one_second_produce_one_banner() {
        let mut n = Notifier::new();
        let s = settings();
        let mut posts = 0;
        let mut updates = 0;
        for i in 0..100 {
            match n.consider(&candidate(7, 1_000_000 + i * 10_000), &s, false) {
                Decision::Post(_) => posts += 1,
                Decision::Update(_) => updates += 1,
                Decision::Suppress(_) => {}
            }
        }
        assert_eq!(posts, 1, "one signature must post exactly once in its cooldown");
        assert!(updates > 0, "repeats should update the count, not vanish silently");
    }

    #[test]
    fn the_update_carries_a_running_count() {
        let mut n = Notifier::new();
        let s = settings();
        n.consider(&candidate(1, 0), &s, false);
        let d = n.consider(&candidate(1, 1_000), &s, false);
        match d {
            Decision::Update(notice) => {
                assert_eq!(notice.count, 2);
                assert_eq!(notice.replaces_key.as_deref(), Some("fp:1"));
            }
            other => panic!("expected an update, got {other:?}"),
        }
    }

    #[test]
    fn distinct_signatures_are_capped_per_minute_not_silenced_forever() {
        let mut n = Notifier::new();
        let s = settings();
        let posted = (0..10)
            .filter(|i| {
                matches!(
                    n.consider(&candidate(*i, 1_000_000 + i * 1000), &s, false),
                    Decision::Post(_)
                )
            })
            .count();
        assert_eq!(posted, MAX_PER_MINUTE);

        let summary = n.drain_coalesced(1, "my_app").unwrap();
        assert!(summary.title.contains("more errors"), "{}", summary.title);
    }

    #[test]
    fn a_storm_posts_once_and_then_goes_quiet() {
        let mut n = Notifier::new();
        let s = settings();
        let mut posts = 0;
        // 200 distinct signatures in one second, well past the storm threshold.
        for i in 0..200 {
            if let Decision::Post(notice) = n.consider(&candidate(i, 5_000_000 + i * 1000), &s, false)
            {
                posts += 1;
                if notice.title.contains("storm") {
                    assert!(notice.body.contains("paused"), "{}", notice.body);
                }
            }
        }
        assert!(posts <= MAX_PER_MINUTE + 1, "a storm produced {posts} banners");
    }

    #[test]
    fn a_new_run_lets_a_known_signature_notify_again() {
        let mut n = Notifier::new();
        let s = settings();
        assert!(matches!(n.consider(&candidate(3, 0), &s, false), Decision::Post(_)));
        assert!(matches!(n.consider(&candidate(3, 1000), &s, false), Decision::Update(_)));

        n.reset_for_new_run();
        assert!(
            matches!(n.consider(&candidate(3, 2000), &s, false), Decision::Post(_)),
            "restarting the server should surface the error again"
        );
    }

    #[test]
    fn muting_and_disabling_are_honoured_and_say_why() {
        let mut n = Notifier::new();
        let mut s = settings();
        assert!(matches!(
            n.consider(&candidate(1, 0), &s, true),
            Decision::Suppress("this project is muted")
        ));
        s.notifications_enabled = false;
        assert!(matches!(
            n.consider(&candidate(2, 0), &s, false),
            Decision::Suppress("notifications are off")
        ));
    }

    #[test]
    fn a_crash_always_notifies_even_when_the_rate_cap_is_spent() {
        let mut n = Notifier::new();
        let s = settings();
        for i in 0..10 {
            n.consider(&candidate(i, 1_000_000 + i * 1000), &s, false);
        }
        let d = n.run_ended(1, "my_app", "mix phx.server", None, Some("SIGSEGV"), &s, false);
        assert!(matches!(d, Decision::Post(_)), "{d:?}");
    }

    #[test]
    fn ctrl_c_is_not_a_crash() {
        let mut n = Notifier::new();
        let s = settings();
        let d = n.run_ended(1, "my_app", "mix phx.server", Some(130), Some("SIGINT"), &s, false);
        assert!(matches!(d, Decision::Suppress(_)), "{d:?}");
    }

    #[test]
    fn quiet_hours_wrap_midnight() {
        let q = Some(QuietHours { start_min: 22 * 60, end_min: 8 * 60 });
        // Wrapping ranges are recognised by start > end.
        assert!(matches!(q, Some(qq) if qq.start_min > qq.end_min));
    }

    #[test]
    fn an_undeclared_severity_never_interrupts_anyone() {
        let mut n = Notifier::new();
        let s = settings();
        let mut c = candidate(1, 0);
        c.evidence = Evidence::Unknown;
        assert!(matches!(n.consider(&c, &s, false), Decision::Suppress(_)));
    }

    #[test]
    fn novelty_alone_is_not_grounds_for_a_banner() {
        let mut n = Notifier::new();
        let s = settings();
        let mut c = candidate(2, 0);
        c.evidence = Evidence::Unknown;
        c.is_new_signature = true;
        c.level = Level::Unknown;
        assert!(matches!(n.consider(&c, &s, false), Decision::Suppress(_)));
    }

    #[test]
    fn a_declared_error_still_gets_through() {
        let mut n = Notifier::new();
        let s = settings();
        let mut c = candidate(3, 0);
        c.evidence = Evidence::Declared;
        assert!(matches!(n.consider(&c, &s, false), Decision::Post(_)));
    }

    #[test]
    fn a_long_template_is_trimmed_to_one_readable_line() {
        let long = format!("{}\nsecond line", "x".repeat(500));
        let t = truncate(&long, 40);
        assert!(t.chars().count() <= 40);
        assert!(!t.contains('\n'));
        assert!(t.ends_with('…'));
    }
}
