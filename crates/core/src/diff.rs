//! Run diffing — the demo, and the reason the git stamp exists.
//!
//! The question is "it worked two days ago, what changed?", so the answer is organised as
//! an answer rather than as a dump: what is failing now that was not failing then, what
//! stopped failing, and the three things that could plausibly have caused it — the commits
//! in between, the lockfiles that moved, and the environment keys that appeared.

use crate::error::Result;
use crate::model::*;
use crate::proto::{DiffSignature, GitDiff, LockfileChange, RunDiff, RunRef};
use crate::store::{read, Db};
use std::collections::BTreeMap;
use std::path::Path;

/// Diff two runs. `a` defaults to the last known good run and `b` to the latest, which is
/// the comparison people actually want and almost never the one they would type out.
pub fn diff(db: &Db, a: Option<RunRef>, b: Option<RunRef>) -> Result<RunDiff> {
    let a_run = read::resolve_run(db, &a.unwrap_or(RunRef::LastGood))?;
    let b_run = read::resolve_run(db, &b.unwrap_or(RunRef::Latest))?;

    // Only error and warn signatures participate. "What changed" means "what broke", and
    // a project that logged a new info line this run has not broken.
    let sigs_a = signature_map(db, a_run.as_ref())?;
    let sigs_b = signature_map(db, b_run.as_ref())?;

    let mut new_signatures = Vec::new();
    let mut resolved_signatures = Vec::new();
    let mut changed_counts = Vec::new();

    for (id, (template, level, count_b)) in &sigs_b {
        match sigs_a.get(id) {
            None => new_signatures.push(DiffSignature {
                fingerprint_id: *id,
                template: template.clone(),
                level: *level,
                count_a: 0,
                count_b: *count_b,
                sample: read::sample_for_fingerprint(db, *id, b_run.as_ref().map(|r| r.id))?,
            }),
            Some((_, _, count_a)) if count_a != count_b => changed_counts.push(DiffSignature {
                fingerprint_id: *id,
                template: template.clone(),
                level: *level,
                count_a: *count_a,
                count_b: *count_b,
                sample: None,
            }),
            _ => {}
        }
    }

    for (id, (template, level, count_a)) in &sigs_a {
        if !sigs_b.contains_key(id) {
            resolved_signatures.push(DiffSignature {
                fingerprint_id: *id,
                template: template.clone(),
                level: *level,
                count_a: *count_a,
                count_b: 0,
                sample: None,
            });
        }
    }

    // Errors first, then loudest — the top of this list should be the answer.
    let rank = |s: &DiffSignature| (s.level != Level::Error, -s.count_b, -s.count_a);
    new_signatures.sort_by_key(rank);
    resolved_signatures.sort_by_key(rank);
    changed_counts.sort_by_key(|s| (s.level != Level::Error, -(s.count_b - s.count_a).abs()));

    let (env_added, env_removed) = env_delta(a_run.as_ref(), b_run.as_ref());

    Ok(RunDiff {
        git: git_diff(a_run.as_ref(), b_run.as_ref()),
        lockfiles_changed: lockfile_delta(a_run.as_ref(), b_run.as_ref()),
        command_changed: match (&a_run, &b_run) {
            (Some(x), Some(y)) => x.command != y.command,
            _ => false,
        },
        env_added,
        env_removed,
        new_signatures,
        resolved_signatures,
        changed_counts,
        a: a_run,
        b: b_run,
    })
}

fn signature_map(
    db: &Db,
    run: Option<&Run>,
) -> Result<BTreeMap<FingerprintId, (String, Level, i64)>> {
    let Some(run) = run else {
        return Ok(BTreeMap::new());
    };
    Ok(read::signatures_for_run(db, run.id)?
        .into_iter()
        .filter(|(_, _, level, _)| matches!(level, Level::Error | Level::Warn))
        .map(|(id, template, level, count)| (id, (template, level, count)))
        .collect())
}

fn git_diff(a: Option<&Run>, b: Option<&Run>) -> GitDiff {
    let mut out = GitDiff {
        sha_a: a.and_then(|r| r.git_sha.clone()),
        sha_b: b.and_then(|r| r.git_sha.clone()),
        branch_a: a.and_then(|r| r.git_branch.clone()),
        branch_b: b.and_then(|r| r.git_branch.clone()),
        dirty_changed: a.and_then(|r| r.git_dirty_hash.clone())
            != b.and_then(|r| r.git_dirty_hash.clone()),
        ..Default::default()
    };

    // Walking history needs a working tree, and the run may have been captured in a
    // directory that has since moved or been deleted. Best effort, never an error.
    if let (Some(sha_a), Some(sha_b), Some(run_b)) = (&out.sha_a, &out.sha_b, b) {
        if sha_a != sha_b {
            let root = Path::new(&run_b.cwd);
            if root.exists() {
                out.commits = crate::project::commits_between(root, sha_a, sha_b);
                out.files_changed = crate::project::files_changed(root, sha_a, sha_b);
            }
        }
    }
    out
}

fn lockfile_delta(a: Option<&Run>, b: Option<&Run>) -> Vec<LockfileChange> {
    let empty = BTreeMap::new();
    let ha = a.map(|r| &r.lockfile_hashes).unwrap_or(&empty);
    let hb = b.map(|r| &r.lockfile_hashes).unwrap_or(&empty);
    let mut files: Vec<&String> = ha.keys().chain(hb.keys()).collect();
    files.sort();
    files.dedup();
    files
        .into_iter()
        .filter(|f| ha.get(*f) != hb.get(*f))
        .map(|f| LockfileChange {
            file: f.clone(),
            hash_a: ha.get(f).cloned(),
            hash_b: hb.get(f).cloned(),
        })
        .collect()
}

fn env_delta(a: Option<&Run>, b: Option<&Run>) -> (Vec<String>, Vec<String>) {
    let ea: std::collections::BTreeSet<&String> =
        a.map(|r| r.env_keys.iter().collect()).unwrap_or_default();
    let eb: std::collections::BTreeSet<&String> =
        b.map(|r| r.env_keys.iter().collect()).unwrap_or_default();
    (
        eb.difference(&ea).map(|s| (*s).clone()).collect(),
        ea.difference(&eb).map(|s| (*s).clone()).collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RunStamp;
    use crate::redact::Redactor;
    use crate::store::{index::IndexDb, Writer};
    use std::sync::Arc;

    struct H {
        _tmp: tempfile::TempDir,
        db: Db,
        index: IndexDb,
    }

    fn h() -> H {
        let tmp = tempfile::tempdir().unwrap();
        let index = IndexDb::open(&tmp.path().join("index.db")).unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let p = index.register(&root, "proj", &tmp.path().join("p.db")).unwrap();
        let db = Db::open(&tmp.path().join("p.db"), p.id).unwrap();
        H { _tmp: tmp, db, index }
    }

    fn run_with(h: &H, output: &[u8], exit: i32, stamp: RunStamp) -> RunId {
        let mut w = Writer::start(&h.db, &h.index, &stamp, Arc::new(Redactor::disabled())).unwrap();
        if !output.is_empty() {
            w.push(output, crate::now_micros()).unwrap();
        }
        w.finish(&h.index, Some(exit), None).unwrap();
        w.run_id()
    }

    fn stamp(sha: &str, lock: &str) -> RunStamp {
        let mut s = RunStamp {
            command: vec!["mix".into(), "test".into()],
            git_sha: Some(sha.into()),
            git_branch: Some("main".into()),
            env_keys: vec!["PATH".into(), "HOME".into()],
            ..Default::default()
        };
        s.lockfile_hashes.insert("mix.lock".into(), lock.into());
        s
    }

    #[test]
    fn the_default_comparison_is_last_good_versus_latest() {
        let h = h();
        run_with(&h, b"all fine\n", 0, stamp("aaa", "l1"));
        run_with(
            &h,
            b"Error: connection refused\n",
            1,
            stamp("bbb", "l2"),
        );

        let d = diff(&h.db, None, None).unwrap();
        assert_eq!(d.a.as_ref().unwrap().git_sha.as_deref(), Some("aaa"));
        assert_eq!(d.b.as_ref().unwrap().git_sha.as_deref(), Some("bbb"));
        assert_eq!(d.new_signatures.len(), 1);
        assert!(d.new_signatures[0].template.contains("connection refused"));
        assert!(d.new_signatures[0].sample.is_some(), "the diff should show the error");
    }

    #[test]
    fn a_fixed_error_shows_up_as_resolved() {
        let h = h();
        run_with(&h, b"Error: boom\n", 1, stamp("aaa", "l1"));
        run_with(&h, b"all fine\n", 0, stamp("bbb", "l1"));

        let d = diff(
            &h.db,
            Some(RunRef::Id(1)),
            Some(RunRef::Latest),
        )
        .unwrap();
        assert_eq!(d.resolved_signatures.len(), 1);
        assert!(d.new_signatures.is_empty());
    }

    #[test]
    fn count_changes_are_reported_separately_from_new_signatures() {
        let h = h();
        let a = run_with(&h, b"Error: boom\n", 1, stamp("aaa", "l1"));
        let b = run_with(&h, b"Error: boom\nError: boom\n", 1, stamp("aaa", "l1"));
        let d = diff(&h.db, Some(RunRef::Id(a)), Some(RunRef::Id(b))).unwrap();
        assert!(d.new_signatures.is_empty(), "the signature is not new, only louder");
        assert_eq!(d.changed_counts.len(), 1);
        assert_eq!((d.changed_counts[0].count_a, d.changed_counts[0].count_b), (1, 2));
    }

    #[test]
    fn the_things_that_could_have_caused_it_are_surfaced() {
        let h = h();
        let a = run_with(&h, b"fine\n", 0, stamp("aaa", "lock-v1"));
        let mut later = stamp("bbb", "lock-v2");
        later.env_keys.push("NEW_FEATURE_FLAG".into());
        later.command = vec!["mix".into(), "test".into(), "--trace".into()];
        let b = run_with(&h, b"Error: boom\n", 1, later);

        let d = diff(&h.db, Some(RunRef::Id(a)), Some(RunRef::Id(b))).unwrap();
        assert_eq!(d.lockfiles_changed.len(), 1);
        assert_eq!(d.lockfiles_changed[0].file, "mix.lock");
        assert_eq!(d.env_added, vec!["NEW_FEATURE_FLAG".to_string()]);
        assert!(d.env_removed.is_empty());
        assert!(d.command_changed);
        assert_eq!(d.git.sha_a.as_deref(), Some("aaa"));
        assert_eq!(d.git.sha_b.as_deref(), Some("bbb"));
    }

    #[test]
    fn an_unchanged_lockfile_is_not_reported() {
        let h = h();
        let a = run_with(&h, b"fine\n", 0, stamp("aaa", "same"));
        let b = run_with(&h, b"fine\n", 0, stamp("aaa", "same"));
        let d = diff(&h.db, Some(RunRef::Id(a)), Some(RunRef::Id(b))).unwrap();
        assert!(d.lockfiles_changed.is_empty());
        assert!(!d.command_changed);
    }

    #[test]
    fn diffing_against_a_project_with_no_good_run_still_answers() {
        let h = h();
        run_with(&h, b"Error: boom\n", 1, stamp("aaa", "l1"));
        let d = diff(&h.db, None, None).unwrap();
        assert!(d.a.is_none(), "there is no last-good run to compare against");
        assert_eq!(d.new_signatures.len(), 1, "everything in b is new");
    }

    #[test]
    fn errors_sort_above_other_levels() {
        let h = h();
        let a = run_with(&h, b"fine\n", 0, stamp("aaa", "l1"));
        let b = run_with(
            &h,
            b"warning: deprecated thing\nError: real problem\n",
            1,
            stamp("bbb", "l1"),
        );
        let d = diff(&h.db, Some(RunRef::Id(a)), Some(RunRef::Id(b))).unwrap();
        assert_eq!(d.new_signatures[0].level, Level::Error);
    }
}
