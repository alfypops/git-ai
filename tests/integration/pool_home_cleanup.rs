// Modified by alfypops, 2026: shared test daemon pool homes are cleaned up and swept.
//
// The pool's `git-ai-shared-daemon-*` homes live in `temp_dir()`. Each probe runs this test
// binary as a child with `TMPDIR` pointed at a private directory, so the assertions only see
// the homes of that one run.
use crate::repos::test_repo::TestRepo;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const PROBE_ENV: &str = "CDE57_POOL_PROBE";
const PROBE_TEST: &str = "pool_home_cleanup::pool_probe";

/// Child half of every test below: starts the shared pool from two threads (so two shards are
/// populated when the pool has room), checks the homes exist, then exits normally.
#[test]
fn pool_probe() {
    if std::env::var(PROBE_ENV).is_err() {
        return;
    }
    // Each thread is assigned its own shard; the daemons stay in statics until atexit.
    let repos: Vec<_> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..2).map(|_| scope.spawn(TestRepo::new)).collect();
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect()
    });
    let expected = std::env::var("GIT_AI_TEST_SHARED_DAEMON_POOL_SIZE")
        .unwrap()
        .parse::<usize>()
        .unwrap()
        .min(2);
    assert_eq!(
        own_pool_homes(&std::env::temp_dir()).len(),
        expected,
        "pool homes while running"
    );
    drop(repos);
}

/// A short private `TMPDIR` (daemon socket paths have a tight length limit).
fn private_tmp() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("gp")
        .tempdir_in("/tmp")
        .unwrap()
}

fn pool_probe_command(tmp: &Path, pool_size: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", PROBE_TEST, "--nocapture"])
        .env(PROBE_ENV, "1")
        .env("TMPDIR", tmp)
        .env("GIT_AI_TEST_SHARED_DAEMON_POOL_SIZE", pool_size);
    command
}

fn run_pool_probe(tmp: &Path, pool_size: &str) {
    let output = pool_probe_command(tmp, pool_size).output().unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Every pool entry (home and db dir) in `tmp`.
fn pool_dirs(tmp: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<_> = fs::read_dir(tmp)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("git-ai-shared-daemon-"))
        })
        .collect();
    dirs.sort();
    dirs
}

/// Pool homes whose marker names this process (pre-made homes of other owners don't count).
fn own_pool_homes(tmp: &Path) -> Vec<PathBuf> {
    let own = std::process::id().to_string();
    let mut homes = pool_dirs(tmp);
    homes.retain(|dir| {
        dir.to_string_lossy().ends_with("-home")
            && fs::read_to_string(dir.join("owner.pid")).is_ok_and(|pid| pid == own)
    });
    homes
}

/// Pre-makes a pool home (and db dir) as a previous run would have left it.
fn make_pool_home(tmp: &Path, id: &str, owner: Option<&str>) -> (PathBuf, PathBuf) {
    let home = tmp.join(format!("git-ai-shared-daemon-{id}-pool-0-home"));
    let db = tmp.join(format!("git-ai-shared-daemon-{id}-pool-0-db"));
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&db).unwrap();
    if let Some(owner) = owner {
        fs::write(home.join("owner.pid"), owner).unwrap();
    }
    (home, db)
}

fn dead_pid() -> u32 {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

#[test]
fn pool_homes_are_removed_when_the_test_process_exits() {
    for pool_size in ["8", "1"] {
        let tmp = private_tmp();
        run_pool_probe(tmp.path(), pool_size);
        assert_eq!(
            pool_dirs(tmp.path()),
            Vec::<PathBuf>::new(),
            "pool size {pool_size} left homes behind"
        );
    }
}

#[test]
fn pool_start_sweeps_homes_owned_by_a_dead_process() {
    let tmp = private_tmp();
    let (home, db) = make_pool_home(tmp.path(), "1", Some(&dead_pid().to_string()));
    run_pool_probe(tmp.path(), "8");
    assert!(!home.exists(), "stale home should be swept");
    assert!(!db.exists(), "stale db dir should be swept with its home");
}

#[test]
fn pool_start_keeps_homes_owned_by_a_live_process() {
    let tmp = private_tmp();
    let (home, db) = make_pool_home(tmp.path(), "2", Some(&std::process::id().to_string()));
    run_pool_probe(tmp.path(), "8");
    assert!(home.exists(), "live-owned home must not be touched");
    assert!(db.exists(), "live-owned db dir must not be touched");
}

#[test]
fn pool_start_keeps_homes_without_an_owner_marker() {
    let tmp = private_tmp();
    let (home, db) = make_pool_home(tmp.path(), "3", None);
    run_pool_probe(tmp.path(), "8");
    assert!(home.exists(), "unmarked home must not be touched");
    assert!(db.exists(), "unmarked db dir must not be touched");
}

#[test]
fn pool_start_keeps_homes_with_an_invalid_owner_pid() {
    let tmp = private_tmp();
    // 2147483648 does not fit a positive pid_t; 0 would address the whole process group.
    let homes: Vec<_> = ["2147483648", "0"]
        .iter()
        .enumerate()
        .map(|(i, owner)| make_pool_home(tmp.path(), &format!("4{i}"), Some(owner)))
        .collect();
    run_pool_probe(tmp.path(), "8");
    for (home, db) in homes {
        assert!(home.exists(), "invalid-owner home must not be touched");
        assert!(db.exists(), "invalid-owner db dir must not be touched");
    }
}

#[test]
fn failed_pool_start_removes_its_home() {
    let tmp = private_tmp();
    // A daemon binary that exits at once makes the readiness wait fail.
    let output = pool_probe_command(tmp.path(), "8")
        .env("GIT_AI_TEST_BINARY_PATH", "/usr/bin/false")
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "probe should fail to start the pool"
    );
    assert_eq!(pool_dirs(tmp.path()), Vec::<PathBuf>::new());
}
