// Modified by alfypops, 2026: daemon_repo_roots allow-list coverage.
//! `daemon_repo_roots`: with a list, the daemon ignores trace2 events from
//! repositories outside it. Unlisted repositories reach the daemon only
//! through raw traced git, the way an agent's test fixtures under `$TMPDIR` do.

use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::{
    DaemonTestScope, TestRepo, configure_raw_traced_git_env, configure_raw_traced_git_env_for,
    new_daemon_test_sync_session_id, real_git_executable,
};
use git_ai::daemon::{ControlRequest, send_control_request_with_timeout};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Writes `daemon_repo_roots` into the daemon's config file, guaranteeing a
/// new mtime so the daemon reloads it.
fn set_daemon_repo_roots(repo: &TestRepo, roots: Value) {
    let path = repo.daemon_home_path().join(".git-ai/config.json");
    let previous_mtime = fs::metadata(&path).and_then(|m| m.modified()).ok();
    let mut config: Value = fs::read(&path)
        .map(|data| serde_json::from_slice(&data).unwrap())
        .unwrap_or_else(|_| json!({}));
    config["daemon_repo_roots"] = roots;
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
    if let Some(previous) = previous_mtime
        && fs::metadata(&path).unwrap().modified().unwrap() <= previous
    {
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(previous + Duration::from_secs(1))
            .unwrap();
    }
}

/// Waits until the daemon has drained trace ingest and side effects for every
/// family, failing if anything (such as an open root's fence) holds it.
fn await_daemon_drained(repo: &TestRepo, timeout_secs: u64) {
    let response = send_control_request_with_timeout(
        &repo.daemon_control_socket_path(),
        &ControlRequest::Await { timeout_secs },
        Duration::from_secs(timeout_secs + 5),
    )
    .expect("await");
    assert_eq!(
        response
            .data
            .as_ref()
            .and_then(|data| data.get("timed_out")),
        Some(&json!(false)),
        "the daemon must drain without waiting on an unlisted root: {response:?}"
    );
}

/// A traced `git add` + `git commit` run from inside `dir` (no `-C`), so the
/// `start` frame names no repository and `def_repo` does. Returns the
/// commit's test-sync session.
fn traced_commit_in(dir: &Path, daemon_repo: &TestRepo, name: &str) -> String {
    fs::write(dir.join(name), format!("{name}\n")).unwrap();
    let session = new_daemon_test_sync_session_id();
    let sync_session = format!("git-ai.testSyncSession={session}");
    for args in [
        vec!["add", name],
        vec!["-c", &sync_session, "commit", "-q", "-m", name],
    ] {
        let mut command = Command::new(real_git_executable());
        command.args(&args).current_dir(dir);
        configure_raw_traced_git_env(&mut command, daemon_repo);
        let output = command.output().expect("run traced git");
        assert!(
            output.status.success(),
            "traced git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    session
}

/// A plain repository under `$TMPDIR` that reports to `daemon_repo`'s daemon
/// only through trace2.
struct TraceOnlyRepo {
    dir: tempfile::TempDir,
}

impl TraceOnlyRepo {
    fn new(daemon_repo: &TestRepo) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("daemon-repo-roots-")
            .tempdir()
            .unwrap();
        let repo = Self { dir };
        for args in [
            &["init", "-q"][..],
            &["config", "user.name", "Fixture"],
            &["config", "user.email", "fixture@example.com"],
        ] {
            repo.run_git(daemon_repo, args, "0");
        }
        repo
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn run_git(&self, daemon_repo: &TestRepo, args: &[&str], trace2_event_target: &str) -> String {
        let mut command = Command::new(real_git_executable());
        command.args(args).current_dir(self.path());
        configure_raw_traced_git_env_for(
            &mut command,
            daemon_repo.test_home_path(),
            trace2_event_target,
        );
        let output = command.output().expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn traced_commit(&self, daemon_repo: &TestRepo, name: &str) -> String {
        traced_commit_in(self.path(), daemon_repo, name)
    }

    fn canonical_path(&self) -> PathBuf {
        self.path().canonicalize().unwrap()
    }

    /// No authorship note, working log or completion entry for this repo, and
    /// no daemon log line naming it.
    fn assert_untouched_by(&self, daemon_repo: &TestRepo) {
        await_daemon_drained(daemon_repo, 30);
        // First: the completion-log lookup below opens git-ai storage here.
        assert!(
            !self.path().join(".git/ai").exists(),
            "an unlisted repo must not get git-ai storage or working logs"
        );
        assert!(
            daemon_repo
                .daemon_completion_entries_for_repo(self.path())
                .is_empty(),
            "an unlisted repo must not reach the completion log"
        );
        assert_eq!(
            self.run_git(daemon_repo, &["notes", "--ref=ai", "list"], "0"),
            "",
            "an unlisted repo must not get authorship notes"
        );
        let logs = daemon_repo.daemon_stderr_contents();
        let canonical = self.canonical_path();
        for needle in [self.path().to_string_lossy(), canonical.to_string_lossy()] {
            assert!(
                !logs.contains(needle.as_ref()),
                "the daemon must not log about an unlisted repo:\n{logs}"
            );
        }
    }
}

fn commit_ai_line(repo: &TestRepo, name: &str) {
    let mut file = repo.filename(name);
    file.set_contents(crate::lines![format!("{name} by ai").ai()]);
    repo.stage_all_and_commit(name).unwrap();
    file.assert_committed_lines(crate::lines![format!("{name} by ai").ai()]);
}

/// Row 1: an empty list tracks every repository, as upstream does.
#[test]
fn empty_daemon_repo_roots_tracks_every_repo() {
    let repo = TestRepo::new_dedicated_daemon();
    set_daemon_repo_roots(&repo, json!([]));
    let other = TraceOnlyRepo::new(&repo);

    let session = other.traced_commit(&repo, "fixture.txt");

    repo.wait_for_daemon_completion_session_for_repo(other.path(), &session);
    commit_ai_line(&repo, "listed.txt");
}

/// Rows 2, 4, 6 and 8: the listed repo (named through the `$TMPDIR` symlink)
/// is attributed; an unlisted `$TMPDIR` repo's commit leaves no trace.
#[test]
fn listed_repo_is_attributed_and_unlisted_repo_is_ignored() {
    let repo = TestRepo::new_dedicated_daemon();
    set_daemon_repo_roots(&repo, json!([repo.path().join(".git/..")]));
    let unlisted = TraceOnlyRepo::new(&repo);

    unlisted.traced_commit(&repo, "fixture.txt");
    commit_ai_line(&repo, "listed.txt");

    unlisted.assert_untouched_by(&repo);
}

/// Row 3 (and row 10's listed linked-worktree case): a linked worktree shares
/// the listed repo's common dir, so its checkpoints and commit are recorded.
#[test]
fn linked_worktree_of_a_listed_repo_is_attributed() {
    let repo = TestRepo::new_worktree_with_daemon_scope(DaemonTestScope::Dedicated);
    let base = repo
        .base_repo_path()
        .expect("linked worktree")
        .to_path_buf();
    set_daemon_repo_roots(&repo, json!([base]));

    commit_ai_line(&repo, "worktree.txt");
}

/// Row 5 / I3: an unlisted root held open after writing refs never fences
/// the daemon's work; a listed commit meanwhile is attributed.
#[test]
#[cfg(not(windows))]
fn open_unlisted_root_never_delays_listed_work() {
    use crate::repos::test_repo::BlockedTracedGit;

    let repo = TestRepo::new_dedicated_daemon();
    set_daemon_repo_roots(&repo, json!([repo.path()]));
    let unlisted = TraceOnlyRepo::new(&repo);
    fs::write(unlisted.path().join("held.txt"), "held\n").unwrap();
    unlisted.run_git(&repo, &["add", "held.txt"], "0");
    let blocked = BlockedTracedGit::spawn_commit_blocked_in(
        &repo,
        unlisted.path(),
        "post-commit",
        &new_daemon_test_sync_session_id(),
        "held open",
    );

    await_daemon_drained(&repo, 5);
    commit_ai_line(&repo, "listed.txt");

    blocked.release();
    unlisted.assert_untouched_by(&repo);
}

/// Row 7: a root added while the daemon runs is picked up once the config
/// file's mtime changes.
#[test]
fn root_added_at_runtime_is_attributed_after_the_config_mtime_changes() {
    let repo = TestRepo::new_dedicated_daemon();
    let listed = TraceOnlyRepo::new(&repo);
    set_daemon_repo_roots(&repo, json!([listed.path()]));

    let completions_before = repo.daemon_completion_entries().len();
    traced_commit_in(repo.path(), &repo, "before.txt");
    let session = listed.traced_commit(&repo, "barrier.txt");
    repo.wait_for_daemon_completion_session_for_repo(listed.path(), &session);
    await_daemon_drained(&repo, 30);
    assert_eq!(
        repo.daemon_completion_entries().len(),
        completions_before,
        "the repo is not listed yet"
    );

    set_daemon_repo_roots(&repo, json!([listed.path(), repo.path()]));
    commit_ai_line(&repo, "after.txt");
}

/// Row 10: a control-socket checkpoint for an unlisted repo is acknowledged
/// and leaves nothing; a listed repo's checkpoints still record.
#[test]
fn checkpoint_for_an_unlisted_repo_is_ignored_and_listed_checkpoints_record() {
    let repo = TestRepo::new_dedicated_daemon();
    set_daemon_repo_roots(&repo, json!([repo.path()]));
    let unlisted = TraceOnlyRepo::new(&repo);
    fs::write(unlisted.path().join("ai.txt"), "ai line\n").unwrap();

    let output = repo
        .git_ai_command_without_pre_sync_for_test(&["checkpoint", "mock_ai", "ai.txt"], &[])
        .current_dir(unlisted.path())
        .env("GIT_AI_TEST_DB_PATH", repo.test_db_path())
        .env("GITAI_TEST_DB_PATH", repo.test_db_path())
        .output()
        .expect("run git-ai checkpoint");
    assert!(
        output.status.success(),
        "a user hook must not error for an unlisted repo: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout
            .lines()
            .any(|line| line.trim() == "checkpoint_requests=1"),
        "the CLI must have sent the checkpoint to the daemon: {stdout}"
    );

    commit_ai_line(&repo, "listed.txt");
    unlisted.assert_untouched_by(&repo);
}

/// Row 11: a family the daemon knew before the list was set gets no
/// untraced-commit fixup once it is outside the list.
#[test]
fn untraced_fixup_skips_a_known_family_outside_the_list() {
    let repo = TestRepo::new_with_daemon_env(&[("GIT_AI_DAEMON_UNTRACED_FIXUP_MIN_AGE_MS", "0")]);
    fs::write(repo.path().join("plain.txt"), "base\n").unwrap();
    repo.stage_all_and_commit("traced base").unwrap();
    repo.request_untraced_fixup_scan();

    let elsewhere = TraceOnlyRepo::new(&repo);
    set_daemon_repo_roots(&repo, json!([elsewhere.path()]));
    fs::write(repo.path().join("plain.txt"), "base\nby hand\n").unwrap();
    let untraced_env = [("GIT_TRACE2_EVENT", "0"), ("GIT_TRACE2", "0")];
    repo.git_og_with_env(&["commit", "-qam", "by hand"], &untraced_env)
        .unwrap();
    let untraced = repo
        .git_og_with_env(&["rev-parse", "HEAD"], &untraced_env)
        .unwrap()
        .trim()
        .to_string();

    repo.request_untraced_fixup_scan_all();
    repo.request_untraced_fixup_scan();

    assert!(
        repo.read_authorship_note(&untraced).is_none(),
        "the fixup must not write notes outside daemon_repo_roots"
    );
}

/// A traced `git clone` into a listed root, run from an unlisted directory, is
/// decided by its target repository and handled.
#[test]
fn traced_clone_into_a_listed_root_is_handled() {
    let repo = TestRepo::new_dedicated_daemon();
    let clones = tempfile::Builder::new()
        .prefix("daemon-repo-roots-clones-")
        .tempdir()
        .unwrap();
    set_daemon_repo_roots(&repo, json!([repo.path(), clones.path()]));
    commit_ai_line(&repo, "source.txt");
    let unlisted = TraceOnlyRepo::new(&repo);
    let target = clones.path().join("clone");

    let mut command = Command::new(real_git_executable());
    command
        .arg("-C")
        .arg(unlisted.path())
        .args(["clone", "-q"])
        .arg(repo.path())
        .arg(&target);
    configure_raw_traced_git_env(&mut command, &repo);
    assert!(command.status().unwrap().success());

    // Clones carry no test-sync session; drain, then look for the completion.
    await_daemon_drained(&repo, 30);
    assert!(
        !repo.daemon_completion_entries_for_repo(&target).is_empty(),
        "the clone into a listed root must be handled"
    );
    unlisted.assert_untouched_by(&repo);
}

/// A traced `git clone file://<listed repo>` into an unlisted `$TMPDIR` path
/// runs `pack-objects` in the listed source under the clone's root. The clone
/// is ignored, so neither it nor that child may fence the listed family.
#[test]
fn ignored_file_clone_from_a_listed_repo_never_fences_it() {
    let repo = TestRepo::new_dedicated_daemon();
    set_daemon_repo_roots(&repo, json!([repo.path()]));
    commit_ai_line(&repo, "source.txt");
    let scratch = tempfile::Builder::new()
        .prefix("daemon-repo-roots-scratch-")
        .tempdir()
        .unwrap();
    let target = scratch.path().join("x");

    let mut command = Command::new(real_git_executable());
    command
        .args(["clone", "-q"])
        .arg(format!("file://{}", repo.path().display()))
        .arg(&target)
        .current_dir(scratch.path());
    configure_raw_traced_git_env(&mut command, &repo);
    assert!(command.status().unwrap().success());

    await_daemon_drained(&repo, 5);
    commit_ai_line(&repo, "after.txt");
    assert!(
        !target.join(".git/ai").exists(),
        "the ignored clone must leave no git-ai state"
    );
    assert!(repo.daemon_completion_entries_for_repo(&target).is_empty());
}

/// Row 9 (daemon load): invalid entries are logged once and dropped; the
/// valid ones keep being handled and every other repo stays ignored.
#[test]
fn invalid_entries_fail_closed_for_unlisted_repos_and_log_once() {
    let repo = TestRepo::new_dedicated_daemon();
    set_daemon_repo_roots(&repo, json!([repo.path(), "relative/clone"]));
    let unlisted = TraceOnlyRepo::new(&repo);

    unlisted.traced_commit(&repo, "first.txt");
    commit_ai_line(&repo, "first.txt");
    unlisted.traced_commit(&repo, "second.txt");
    commit_ai_line(&repo, "second.txt");

    unlisted.assert_untouched_by(&repo);
    let logs = repo.daemon_stderr_contents();
    assert_eq!(
        logs.matches("Invalid daemon_repo_roots entry 'relative/clone'")
            .count(),
        1,
        "{logs}"
    );
}

/// Row 9 (CLI): `config set` refuses relative paths; set/--add/get/unset
/// round-trip absolute ones.
#[test]
fn config_cli_manages_daemon_repo_roots_and_refuses_relative_paths() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let first = repo.path().join("first");
    let second = repo.path().join("second");
    let get = |repo: &TestRepo| -> Value {
        serde_json::from_str(&repo.git_ai(&["config", "daemon_repo_roots"]).unwrap()).unwrap()
    };

    let error = repo
        .git_ai(&["config", "set", "daemon_repo_roots", "relative/clone"])
        .unwrap_err();
    assert!(error.contains("expected an absolute path"), "{error}");
    assert_eq!(get(&repo), json!([]));

    repo.git_ai(&[
        "config",
        "set",
        "daemon_repo_roots",
        first.to_str().unwrap(),
    ])
    .unwrap();
    repo.git_ai(&[
        "config",
        "set",
        "daemon_repo_roots",
        second.to_str().unwrap(),
        "--add",
    ])
    .unwrap();
    let error = repo
        .git_ai(&["config", "--add", "daemon_repo_roots", "relative"])
        .unwrap_err();
    assert!(error.contains("expected an absolute path"), "{error}");
    assert_eq!(get(&repo), json!([first, second]));
    let all: Value = serde_json::from_str(&repo.git_ai(&["config"]).unwrap()).unwrap();
    assert_eq!(all["daemon_repo_roots"], json!([first, second]));

    repo.git_ai(&["config", "unset", "daemon_repo_roots"])
        .unwrap();
    assert_eq!(get(&repo), json!([]));

    // `--add` won't build on a stored value that isn't a list of paths.
    set_daemon_repo_roots(&repo, json!("not-a-list"));
    let error = repo
        .git_ai(&[
            "config",
            "--add",
            "daemon_repo_roots",
            first.to_str().unwrap(),
        ])
        .unwrap_err();
    assert!(error.contains("not an array of paths"), "{error}");
    assert_eq!(get(&repo), json!("not-a-list"));
}
