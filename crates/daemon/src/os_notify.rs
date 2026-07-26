//! Posting a notification to the operating system.
//!
//! The daemon *decides* (see `runlog_core::notify`) so the behaviour is identical whether
//! or not the app is open. It only *posts* here when no app is subscribed — when the app
//! is running it posts them itself through `tauri-plugin-notification`, which is the path
//! that gets a real application identity, an icon, and click-through to the right screen.
//!
//! This fallback is deliberately shell-based. Linking a notification framework into a
//! headless daemon buys a dependency, a bundle-identity requirement on macOS, and a dbus
//! connection on Linux, in exchange for a banner the user usually sees from the app
//! anyway.

use runlog_core::proto::Notice;
use std::process::{Command, Stdio};

pub fn post(notice: &Notice) {
    #[cfg(target_os = "macos")]
    post_macos(notice);
    #[cfg(target_os = "linux")]
    post_linux(notice);
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let _ = notice;
}

#[cfg(target_os = "macos")]
fn post_macos(notice: &Notice) {
    // `terminal-notifier` supports replacing a banner in place by group id, which is what
    // makes a recurring error tick a counter instead of stacking. osascript cannot do
    // that, so it is only the fallback.
    let group = notice.replaces_key.clone().unwrap_or_default();
    let ran = Command::new("terminal-notifier")
        .args([
            "-title",
            &notice.title,
            "-message",
            &body(notice),
            "-group",
            &group,
            "-sender",
            "sh.runlog.app",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok();

    if !ran {
        let script = format!(
            "display notification {} with title {}",
            applescript_string(&body(notice)),
            applescript_string(&notice.title),
        );
        let _ = Command::new("osascript")
            .args(["-e", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(target_os = "linux")]
fn post_linux(notice: &Notice) {
    let urgency = if notice.level == runlog_core::model::Level::Error {
        "critical"
    } else {
        "normal"
    };
    let _ = Command::new("notify-send")
        .args([
            "--app-name=runlog",
            &format!("--urgency={urgency}"),
            &notice.title,
            &body(notice),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn body(notice: &Notice) -> String {
    if notice.count > 1 {
        format!("{} (×{})", notice.body, notice.count)
    } else {
        notice.body.clone()
    }
}

/// Quote a string for AppleScript. Log lines contain quotes and backslashes constantly,
/// and an unescaped one turns a notification into a syntax error at best.
#[cfg(target_os = "macos")]
fn applescript_string(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use runlog_core::model::Level;

    fn notice(count: i64) -> Notice {
        Notice {
            project_id: 1,
            project_name: "my_app".into(),
            title: "Error in my_app".into(),
            body: "** (RuntimeError) boom".into(),
            level: Level::Error,
            at: 0,
            replaces_key: Some("fp:1".into()),
            count,
        }
    }

    #[test]
    fn a_repeat_shows_its_count() {
        assert_eq!(body(&notice(1)), "** (RuntimeError) boom");
        assert_eq!(body(&notice(42)), "** (RuntimeError) boom (×42)");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn applescript_quoting_survives_a_hostile_log_line() {
        let s = applescript_string(r#"he said "hi" \ then left"#);
        assert_eq!(s, r#""he said \"hi\" \\ then left""#);
    }
}
