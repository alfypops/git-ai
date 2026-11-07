// Modified by alfypops, 2026: isolated authorship notes namespace coverage with sense samples.
use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::{DaemonTestScope, TestRepo};
use serde_json::{Value, json};

fn configure(repo: &mut TestRepo, settings: Value) {
    repo.sync_daemon_force();
    repo.patch_git_ai_config(|patch| {
        if let Some(value) = settings.get("notes_ref") {
            patch.notes_ref = Some(value.clone());
        }
        if let Some(value) = settings.get("notes_push") {
            patch.notes_push = Some(serde_json::from_value(value.clone()).unwrap());
        }
        if let Some(value) = settings.get("notes_backend") {
            patch.notes_backend = Some(serde_json::from_value(value.clone()).unwrap());
        }
    });
    for home in [repo.test_home_path().clone(), repo.daemon_home_path()] {
        let path = home.join(".git-ai/config.json");
        let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        for (key, value) in settings.as_object().unwrap() {
            config[key] = value.clone();
        }
        std::fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
    }
    if let Some(expected) = settings.get("notes_ref") {
        repo.restart_dedicated_daemon_for_test();
        for home in [repo.test_home_path().clone(), repo.daemon_home_path()] {
            let actual: Value =
                serde_json::from_slice(&std::fs::read(home.join(".git-ai/config.json")).unwrap())
                    .unwrap();
            assert_eq!(
                &actual["notes_ref"], expected,
                "restart must retain the actual file setting"
            );
        }
    }
}

fn note(repo: &TestRepo, name: &str, sha: &str) -> Option<String> {
    repo.git(&["notes", &format!("--ref={name}"), "show", sha])
        .ok()
}

fn tip(repo: &TestRepo, name: &str) -> Option<String> {
    repo.git(&["rev-parse", "--verify", &format!("refs/notes/{name}")])
        .ok()
}

fn capture(repo: &TestRepo, path: &str, content: &str) -> String {
    let mut file = repo.filename(path);
    file.set_contents(vec![content.ai()]);
    repo.git(&["add", path]).unwrap();
    repo.git(&["commit", "-m", path]).unwrap();
    let sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    file.assert_committed_lines(vec![content.ai()]);
    sha
}

fn isolated_rewrite(operation: &str) {
    let mut repo = TestRepo::new_dedicated_daemon();
    let old = capture(&repo, "old.rs", "fn upstream() {}");
    let original_tip = tip(&repo, "ai").unwrap();
    configure(
        &mut repo,
        json!({"notes_ref":"sense", "notes_push":"never"}),
    );
    assert!(
        note(&repo, "sense", &old).is_none(),
        "existing notes must stay put"
    );
    let new = capture(&repo, "local.rs", "fn isolated() {}");
    assert!(note(&repo, "sense", &new).is_some());
    match operation {
        "commit" => {}
        "amend" => {
            repo.git(&["commit", "--amend", "-m", "amended"]).unwrap();
        }
        "cherry-pick" | "rebase" => {
            repo.git(&["checkout", "-b", "target", &old]).unwrap();
            repo.git(&["commit", "--allow-empty", "-m", "target divergence"])
                .unwrap();
            repo.filename("old.rs")
                .assert_committed_lines(vec!["fn upstream() {}".unattributed_human()]);
            if operation == "cherry-pick" {
                repo.git(&["cherry-pick", &new]).unwrap();
            } else {
                repo.git(&["checkout", "-b", "source", &new]).unwrap();
                repo.git(&["rebase", "target"]).unwrap();
            }
        }
        _ => unreachable!(),
    }
    repo.filename("local.rs")
        .assert_committed_lines(vec!["fn isolated() {}".ai()]);
    let head = repo.git(&["rev-parse", "HEAD"]).unwrap();
    assert!(note(&repo, "sense", head.trim()).is_some());
    assert_eq!(tip(&repo, "ai").unwrap(), original_tip);
    assert!(note(&repo, "ai", &new).is_none());
    assert!(
        note(&repo, "sense", &old).is_none(),
        "changing namespaces must not copy old notes"
    );
}

#[test]
fn configured_commit_isolated() {
    isolated_rewrite("commit");
}
#[test]
fn configured_amend_isolated() {
    isolated_rewrite("amend");
}
#[test]
fn configured_cherry_pick_isolated() {
    isolated_rewrite("cherry-pick");
}
#[test]
fn configured_rebase_isolated() {
    isolated_rewrite("rebase");
}

#[test]
fn configured_push_policy_and_upstream_coexistence() {
    let (mut repo, remote) =
        TestRepo::new_with_remote_with_daemon_scope(DaemonTestScope::Dedicated);
    capture(&repo, "old.rs", "fn upstream() {}");
    configure(
        &mut repo,
        json!({"notes_ref":"sense", "notes_push":"never"}),
    );
    let sha = capture(&repo, "local.rs", "fn isolated() {}");
    repo.git(&["push", "origin", "HEAD"]).unwrap();
    repo.sync_daemon_force();
    assert!(note(&remote, "sense", &sha).is_none());
    repo.git_og(&["push", "origin", "refs/notes/ai:refs/notes/ai"])
        .unwrap();
    assert!(note(&remote, "ai", &sha).is_none());
    configure(&mut repo, json!({"notes_push":"always"}));
    repo.git_ai(&[
        "push-authorship-notes",
        "--json",
        r#"{"remote_name":"origin"}"#,
    ])
    .unwrap();
    assert!(note(&remote, "sense", &sha).is_some());
    assert!(note(&remote, "ai", &sha).is_none());
}

#[test]
fn configured_fetch_merges_and_tracks_only_own_namespace() {
    let (mut repo, remote) =
        TestRepo::new_with_remote_with_daemon_scope(DaemonTestScope::Dedicated);
    capture(&repo, "old.rs", "fn upstream() {}");
    let original_tip = tip(&repo, "ai").unwrap();
    configure(
        &mut repo,
        json!({"notes_ref":"sense", "notes_push":"always"}),
    );
    repo.git_ai(&["fetch-notes", "origin"]).unwrap();
    let sha = capture(&repo, "local.rs", "fn isolated() {}");
    repo.git(&["push", "origin", "HEAD"]).unwrap();
    repo.sync_daemon_force();
    assert!(note(&remote, "sense", &sha).is_some());
    repo.git_og(&["update-ref", "-d", "refs/notes/sense"])
        .unwrap();
    repo.git_ai(&["fetch-notes", "origin"]).unwrap();
    assert!(note(&repo, "sense", &sha).is_some());
    assert!(tip(&repo, "sense-remote/origin").is_some());
    assert!(tip(&repo, "ai-remote/origin").is_none());
    repo.filename("local.rs")
        .assert_committed_lines(vec!["fn isolated() {}".ai()]);
    // Fetch again with a divergent local notes tip to exercise merge as well as copy.
    let second = capture(&repo, "next.rs", "fn next() {}");
    repo.git_ai(&["fetch-notes", "origin"]).unwrap();
    assert!(note(&repo, "sense", &second).is_some());
    assert!(note(&repo, "sense", &sha).is_some());
    assert_eq!(tip(&repo, "ai").unwrap(), original_tip);
}

#[test]
fn notes_ref_cli_round_trip_and_validation() {
    let repo = TestRepo::new_dedicated_daemon();
    assert_eq!(
        repo.git_ai(&["config", "notes_ref"]).unwrap().trim(),
        "\"ai\""
    );
    for valid in ["sense", "custom-notes", "custom_01", "team.notes"] {
        repo.git_ai(&["config", "set", "notes_ref", valid]).unwrap();
        assert_eq!(
            repo.git_ai(&["config", "notes_ref"]).unwrap().trim(),
            format!("\"{valid}\"")
        );
    }
    for invalid in [
        "-local",
        "",
        "a/b",
        "..",
        ".hidden",
        "trailing.",
        "foo.lock",
        "a b",
        "a@{b",
        "a:b",
        "a?b",
        "a[b",
        "a\\b",
        "a*b",
        "a~b",
        "a^b",
    ] {
        let result = repo.git_ai(&["config", "set", "notes_ref", invalid]);
        assert!(result.is_err(), "accepted {invalid:?}");
        assert!(result.unwrap_err().contains("notes_ref"));
        assert_eq!(
            repo.git_ai(&["config", "notes_ref"]).unwrap().trim(),
            "\"team.notes\""
        );
    }
    repo.git_ai(&["config", "unset", "notes_ref"]).unwrap();
    assert_eq!(
        repo.git_ai(&["config", "notes_ref"]).unwrap().trim(),
        "\"ai\""
    );
}

#[test]
fn invalid_loaded_notes_ref_blocks_writes_and_pushes() {
    for invalid in [
        json!("-local"),
        json!("bad/name"),
        json!(123),
        json!(false),
        json!([]),
        json!({}),
        json!(null),
    ] {
        let (mut repo, remote) =
            TestRepo::new_with_remote_with_daemon_scope(DaemonTestScope::Dedicated);
        capture(&repo, "old.rs", "fn upstream() {}");
        let original_tip = tip(&repo, "ai").unwrap();
        configure(
            &mut repo,
            json!({"notes_ref":invalid, "notes_push":"always"}),
        );
        std::fs::write(repo.path().join("invalid.rs"), "fn invalid() {}\n").unwrap();
        repo.git_ai(&["checkpoint", "mock_ai", "invalid.rs"])
            .unwrap();
        repo.sync_daemon_force();
        repo.git_og(&["add", "invalid.rs"]).unwrap();
        let baseline = repo.daemon_total_completion_count();
        // An invalid namespace is an expected asynchronous processing error.
        // Reuse the completion waiter to observe it, then stop this dedicated
        // daemon so ordinary read assertions do not reject that expected error.
        repo.git_without_test_sync_for_test(&["commit", "-m", "invalid config"], &[])
            .unwrap();
        let failure = std::panic::catch_unwind(|| {
            repo.wait_for_daemon_total_completion_count(baseline, baseline + 1)
        })
        .expect_err("invalid notes_ref must reject the authorship write");
        let reason = failure
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| failure.downcast_ref::<&str>().copied())
            .unwrap_or("");
        assert!(reason.contains("notes_ref"), "{reason}");
        repo.shutdown_dedicated_daemon_for_test();
        let sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
        repo.filename("invalid.rs")
            .assert_committed_lines(vec!["fn invalid() {}".unattributed_human()]);
        assert_eq!(tip(&repo, "ai").unwrap(), original_tip);
        assert!(note(&repo, "ai", &sha).is_none());
        let output = repo
            .git_ai(&[
                "push-authorship-notes",
                "--json",
                r#"{"remote_name":"origin"}"#,
            ])
            .unwrap();
        assert!(
            output.contains("skipped:") && output.contains("notes_ref"),
            "{output}"
        );
        assert!(tip(&remote, "ai").is_none());
    }
}

#[test]
fn configured_log_reads_own_notes() {
    let mut repo = TestRepo::new_dedicated_daemon();
    configure(&mut repo, json!({"notes_ref":"sense"}));
    capture(&repo, "local.rs", "fn isolated() {}");
    for mode in ["--plain", "--raw"] {
        let output = repo
            .git_ai(&["log", "--no-pager", mode, "-n", "1"])
            .unwrap();
        assert!(output.contains("schema_version"), "{output}");
        if mode == "--plain" {
            assert!(output.contains("Notes (sense):"), "{output}");
        }
    }
}

#[test]
fn configured_ci_imports_fork_notes_into_own_namespace() {
    let mut repo = TestRepo::new_dedicated_daemon();
    let base = capture(&repo, "old.rs", "fn upstream() {}");
    repo.git_og(&["branch", "-M", "main"]).unwrap();
    let original_tip = tip(&repo, "ai").unwrap();
    configure(
        &mut repo,
        json!({"notes_ref":"sense", "notes_push":"never"}),
    );
    let mut fork = TestRepo::new_dedicated_daemon();
    configure(
        &mut fork,
        json!({"notes_ref":"sense", "notes_push":"never"}),
    );
    fork.git_og(&["remote", "add", "upstream", repo.path().to_str().unwrap()])
        .unwrap();
    fork.git_og(&["fetch", "upstream"]).unwrap();
    fork.git_og(&["checkout", "-b", "feature", "upstream/main"])
        .unwrap();
    let head = capture(&fork, "local.rs", "fn isolated() {}");
    repo.git_og(&["remote", "add", "fork", fork.path().to_str().unwrap()])
        .unwrap();
    repo.git_og(&["fetch", "fork", "feature:feature"]).unwrap();
    repo.git_og(&["merge", "--squash", "feature"]).unwrap();
    repo.git_og(&["commit", "-m", "squashed fork"]).unwrap();
    repo.filename("local.rs")
        .assert_committed_lines(vec!["fn isolated() {}".unattributed_human()]);
    let merge = repo.git_og(&["rev-parse", "HEAD"]).unwrap();
    let output = repo
        .git_ai(&[
            "ci",
            "local",
            "merge",
            "--merge-commit-sha",
            merge.trim(),
            "--base-ref",
            "main",
            "--head-ref",
            "feature",
            "--head-sha",
            &head,
            "--base-sha",
            &base,
            "--fork-clone-url",
            fork.path().to_str().unwrap(),
            "--skip-fetch-notes",
            "--skip-fetch-base",
            "--skip-push",
        ])
        .unwrap();
    assert!(output.contains("Imported"), "{output}");
    assert!(tip(&repo, "sense-remote/fork").is_some());
    assert!(tip(&repo, "ai-remote/fork").is_none());
    repo.filename("local.rs")
        .assert_committed_lines(vec!["fn isolated() {}".ai()]);
    assert!(note(&repo, "sense", merge.trim()).is_some());
    assert_eq!(tip(&repo, "ai").unwrap(), original_tip);
}

// Keep even documentation and test fixtures honest: the default namespace is
// defined only in the accessor module. This follows the source-policy test pattern.
#[test]
fn authorship_ref_literals_stay_in_accessor() {
    fn visit(dir: &std::path::Path, violations: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(&path, violations);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                for (number, line) in std::fs::read_to_string(&path).unwrap().lines().enumerate() {
                    let default_declarations = [
                        r#"pub const AI_AUTHORSHIP_REFNAME: &str = "ai";"#,
                        r#"pub const AI_AUTHORSHIP_FULL_REF: &str = "refs/notes/ai";"#,
                        r#"pub const AI_AUTHORSHIP_FORK_TRACKING_REF: &str = "refs/notes/ai-remote/fork";"#,
                        r#"pub const AI_AUTHORSHIP_PUSH_REFSPEC: &str = "refs/notes/ai:refs/notes/ai";"#,
                    ];
                    if path.ends_with("git/refs.rs") && default_declarations.contains(&line.trim())
                    {
                        continue;
                    }
                    if [
                        "refs/notes/ai",
                        "--ref=ai",
                        "--notes=ai",
                        "ai-display",
                        "ai-remote/",
                    ]
                    .iter()
                    .any(|literal| line.contains(literal))
                    {
                        violations.push(format!("{}:{}", path.display(), number + 1));
                    }
                }
            }
        }
    }
    let mut violations = Vec::new();
    visit(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut violations,
    );
    assert!(
        violations.is_empty(),
        "Use authorship_notes_ref(), including in descriptions:\n{}",
        violations.join("\n")
    );
}

#[test]
fn configured_review_search_reads_only_own_namespace() {
    use git_ai::authorship::authorship_log_serialization::AuthorshipLog;
    let mut repo = TestRepo::new_dedicated_daemon();
    let upstream = capture(&repo, "upstream.rs", "fn upstream_search() {}");
    configure(&mut repo, json!({"notes_ref":"sense"}));
    let configured = capture(&repo, "configured.rs", "fn configured_search() {}");
    let prompt_id = "namespace_search";
    // Reuse captured authorship logs, adding the same searchable prompt ID
    // with distinguishable records in the two namespaces.
    for (name, sha, marker) in [
        ("ai", upstream.as_str(), "upstream search record"),
        (
            "sense",
            configured.as_str(),
            "configured search record",
        ),
    ] {
        let mut log =
            AuthorshipLog::deserialize_from_string(&note(&repo, name, sha).unwrap()).unwrap();
        let mut prompt = log
            .metadata
            .sessions
            .values()
            .next()
            .unwrap()
            .to_prompt_record();
        prompt.human_author = Some(marker.to_string());
        log.metadata.prompts.insert(prompt_id.to_string(), prompt);
        repo.git_og(&[
            "notes",
            &format!("--ref={name}"),
            "add",
            "-f",
            "-m",
            &log.serialize_to_string().unwrap(),
            sha,
        ])
        .unwrap();
    }
    // show-prompt's history path calls search_notes -> grep_ai_notes.
    let output = repo.git_ai(&["show-prompt", prompt_id]).unwrap();
    let found: Value = serde_json::from_str(output.trim()).unwrap();
    assert_eq!(found["commit"], configured);
    assert_eq!(found["prompt"]["human_author"], "configured search record");
    assert!(!output.contains("upstream search record"));
    assert!(
        repo.git_ai(&["show-prompt", prompt_id, "--offset", "1"])
            .is_err()
    );
}

#[test]
fn configured_review_migration_uploads_and_caches_only_own_namespace() {
    use git_ai::notes::db::NotesDatabase;
    let mut repo = TestRepo::new_dedicated_daemon();
    let upstream = capture(&repo, "upstream.rs", "fn upstream_migration() {}");
    configure(&mut repo, json!({"notes_ref":"sense"}));
    let configured = capture(&repo, "configured.rs", "fn configured_migration() {}");
    let content = note(&repo, "sense", &configured).unwrap();
    assert_ne!(note(&repo, "ai", &upstream).unwrap(), content);
    // Reuse the migration fixtures' mock endpoint, response, and explicit DB
    // verification, while exercising the CLI in its TestRepo-owned HOME.
    let mut server = mockito::Server::new();
    let upload = server
        .mock("POST", "/worker/notes/upload")
        .match_body(mockito::Matcher::Json(
            json!({"entries":[{"commit_sha":configured,"content":content}]}),
        ))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"success_count":1,"failure_count":0}"#)
        .expect(1)
        .create();
    configure(
        &mut repo,
        json!({
            "notes_backend":{"kind":"http", "backend_url":server.url()},
            "api_key":"notes-ref-migration-test-key"
        }),
    );
    repo.git_ai(&["notes", "migrate"]).unwrap();
    upload.assert();
    let db = NotesDatabase::open_at_path(&repo.test_home_path().join(".git-ai/internal/notes-db"))
        .unwrap();
    assert_eq!(
        db.get_note(&configured).unwrap().as_deref(),
        Some(content.as_str())
    );
    assert!(db.get_note(&upstream).unwrap().is_none());
    assert_eq!(
        db.get_synced_shas(&[upstream.as_str(), configured.as_str()])
            .unwrap(),
        std::collections::HashSet::from([configured])
    );
}

#[test]
fn configured_review_ci_copy_keeps_captured_namespace_after_settings_change() {
    use git_ai::git::refs::{AuthorshipNotesRef, git_backend_for_tests};
    let mut repo = TestRepo::new_dedicated_daemon();
    let upstream = capture(&repo, "upstream.rs", "fn upstream_ci_copy() {}");
    let original_tip = tip(&repo, "ai").unwrap();
    configure(&mut repo, json!({"notes_ref":"sense"}));
    let configured = capture(&repo, "configured.rs", "fn configured_ci_copy() {}");
    let captured = AuthorshipNotesRef {
        name: "sense".to_string(),
        full: "refs/notes/sense".to_string(),
    };
    let tracking = captured.tracking("fork");
    repo.git_og(&[
        "update-ref",
        &tracking,
        tip(&repo, "sense").unwrap().trim(),
    ])
    .unwrap();
    repo.git_og(&["update-ref", "-d", &captured.full]).unwrap();
    // The setting changes while an operation still owns its captured namespace.
    configure(&mut repo, json!({"notes_ref":"ai"}));
    let handle = git_ai::git::find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let copied = git_backend_for_tests::copy_missing_notes_for_commits_from_ref_into(
        &handle,
        &tracking,
        std::slice::from_ref(&configured),
        &captured,
    )
    .unwrap();
    assert_eq!(copied, 1);
    assert!(note(&repo, "sense", &configured).is_some());
    assert!(note(&repo, "ai", &configured).is_none());
    assert!(note(&repo, "ai", &upstream).is_some());
    assert_eq!(tip(&repo, "ai").unwrap(), original_tip);
}

#[test]
fn configured_review_rewrite_keeps_namespace_between_read_and_write() {
    use git_ai::authorship::rewrite::shift_authorship_notes_with_after_read_for_test;
    use git_ai::git::refs::AuthorshipNotesRef;
    let mut repo = TestRepo::new_dedicated_daemon();
    capture(&repo, "upstream.rs", "fn upstream_rewrite() {}");
    let original_tip = tip(&repo, "ai").unwrap();
    configure(&mut repo, json!({"notes_ref":"sense"}));
    let source = capture(&repo, "configured.rs", "fn configured_rewrite() {}");
    repo.git_og(&["commit", "--allow-empty", "-m", "rewritten target"])
        .unwrap();
    repo.filename("configured.rs")
        .assert_committed_lines(vec!["fn configured_rewrite() {}".ai()]);
    let target = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    let captured = AuthorshipNotesRef {
        name: "sense".to_string(),
        full: "refs/notes/sense".to_string(),
    };
    let handle = git_ai::git::find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    shift_authorship_notes_with_after_read_for_test(
        &handle,
        &[(source, target.clone())],
        &captured,
        || {
            configure(&mut repo, json!({"notes_ref":"ai"}));
        },
    )
    .unwrap();
    assert!(note(&repo, "sense", &target).is_some());
    assert!(note(&repo, "ai", &target).is_none());
    assert_eq!(tip(&repo, "ai").unwrap(), original_tip);
}

fn process_probe(repo: &TestRepo, test_name: &str, state: Value) {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture"])
        .env("CDE60_PROCESS_STATE", state.to_string())
        .env("HOME", repo.test_home_path())
        .env(
            "GIT_CONFIG_GLOBAL",
            repo.test_home_path().join(".gitconfig"),
        )
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("XDG_CONFIG_HOME", repo.test_home_path().join(".config"))
        .env_remove("GIT_AI_TEST_CONFIG_PATCH")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn process_raw_note(
    repo: &git_ai::git::repository::Repository,
    name: &str,
    sha: &str,
) -> Option<String> {
    let mut args = repo.global_args_for_exec();
    args.extend([
        "notes".to_string(),
        format!("--ref={name}"),
        "show".to_string(),
        sha.to_string(),
    ]);
    git_ai::git::repository::exec_git(&args)
        .ok()
        .map(|output| String::from_utf8(output.stdout).unwrap())
}

fn pending_process_commit() -> (TestRepo, String, String) {
    let mut repo = TestRepo::new_dedicated_daemon();
    configure(&mut repo, json!({"notes_ref":"sense"}));
    let base = capture(&repo, "base.rs", "fn process_base() {}");
    let mut file = repo.filename("carry.rs");
    file.set_contents(vec!["fn process_carry() {}".ai()]);
    repo.sync_daemon_force();
    repo.git_og(&["add", "carry.rs"]).unwrap();
    repo.git_og(&["commit", "-m", "pending carry-over"])
        .unwrap();
    file.assert_committed_lines(vec!["fn process_carry() {}".unattributed_human()]);
    let target = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    (repo, base, target)
}

#[test]
fn process_namespace_stays_fixed_through_transitive_seams() {
    use git_ai::git::refs::authorship_notes_ref;
    if let Ok(state) = std::env::var("CDE60_PROCESS_STATE") {
        assert!(std::env::var_os("GIT_AI_TEST_CONFIG_PATCH").is_none());
        let state: Value = serde_json::from_str(&state).unwrap();
        assert_eq!(authorship_notes_ref().unwrap().name, "sense");
        let config = git_ai::config::config_file_path_public().unwrap();
        let mut value: Value = serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
        value["notes_ref"] = json!("ai");
        std::fs::write(&config, serde_json::to_vec(&value).unwrap()).unwrap();
        let main = git_ai::git::find_repository_in_path(state["main"].as_str().unwrap()).unwrap();
        let attrs = git_ai::tokio_runtime::block_on(
            git_ai::authorship::virtual_attribution::VirtualAttributions::new_for_base_commit(
                main.clone(),
                state["left"].as_str().unwrap().to_string(),
                &["left.rs".to_string()],
                None,
            ),
        );
        let blame_ok = attrs.as_ref().is_ok_and(|attrs| {
            attrs
                .get_line_attributions("left.rs")
                .is_some_and(|lines| !lines.is_empty())
        });
        let mappings = git_ai::authorship::rewrite::derive_merge_commit_mappings_for_test(
            &main,
            state["base"].as_str().unwrap(),
            state["old_merge"].as_str().unwrap(),
            state["new_merge"].as_str().unwrap(),
            &[
                (
                    state["left"].as_str().unwrap().to_string(),
                    state["left"].as_str().unwrap().to_string(),
                ),
                (
                    state["right"].as_str().unwrap().to_string(),
                    state["right"].as_str().unwrap().to_string(),
                ),
            ],
        );
        let mappings_ok = mappings.as_ref().is_ok_and(|pairs| {
            pairs
                == &vec![(
                    state["old_merge"].as_str().unwrap().to_string(),
                    state["new_merge"].as_str().unwrap().to_string(),
                )]
        });
        let conflict = &state["conflict"];
        let conflict_repo =
            git_ai::git::find_repository_in_path(conflict["repo"].as_str().unwrap()).unwrap();
        let conflict_result = git_ai::daemon::process_conflict_resolution_working_logs_for_test(
            &conflict_repo,
            conflict["target"].as_str().unwrap(),
            conflict["base"].as_str().unwrap(),
        );
        let conflict_ok = conflict_result.is_ok()
            && process_raw_note(
                &conflict_repo,
                "sense",
                conflict["target"].as_str().unwrap(),
            )
            .is_some()
            && process_raw_note(&conflict_repo, "ai", conflict["target"].as_str().unwrap())
                .is_none();
        let commit = &state["commit"];
        let commit_repo =
            git_ai::git::find_repository_in_path(commit["repo"].as_str().unwrap()).unwrap();
        let commit_result = git_ai::authorship::post_commit::post_commit_from_working_log(
            &commit_repo,
            Some(commit["base"].as_str().unwrap().to_string()),
            commit["target"].as_str().unwrap().to_string(),
            "Test User <test@example.com>".to_string(),
            true,
        );
        let commit_ok = commit_result.is_ok()
            && process_raw_note(
                &commit_repo,
                "sense",
                commit["target"].as_str().unwrap(),
            )
            .is_some()
            && process_raw_note(&commit_repo, "ai", commit["target"].as_str().unwrap()).is_none();
        assert!(
            blame_ok && mappings_ok && conflict_ok && commit_ok,
            "amend blame={blame_ok}, merge mapping={mappings_ok} {mappings:?}, daemon conflict={conflict_ok} {conflict_result:?}, post-commit={commit_ok} {commit_result:?}"
        );
        assert_eq!(authorship_notes_ref().unwrap().name, "sense");
        value["notes_ref"] = json!("-invalid-new-setting");
        value["notes_push"] = json!("never");
        std::fs::write(&config, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(
            matches!(git_ai::git::sync_authorship::notes_push_allowed(&main,main.path().to_str().unwrap()).unwrap(),git_ai::git::sync_authorship::NotesPushDecision::Skipped(reason) if reason.contains("notes_push"))
        );
        value["notes_push"] = json!("always");
        std::fs::write(&config, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(matches!(
            git_ai::git::sync_authorship::notes_push_allowed(&main, main.path().to_str().unwrap())
                .unwrap(),
            git_ai::git::sync_authorship::NotesPushDecision::Allowed
        ));
        std::fs::write(config, b"{").unwrap();
        assert!(matches!(
            git_ai::git::sync_authorship::notes_push_allowed(&main, main.path().to_str().unwrap())
                .unwrap(),
            git_ai::git::sync_authorship::NotesPushDecision::Skipped("config unreadable")
        ));
        return;
    }
    let mut repo = TestRepo::new_dedicated_daemon();
    let base = capture(&repo, "upstream.rs", "fn upstream_process() {}");
    configure(&mut repo, json!({"notes_ref":"sense"}));
    let left = capture(&repo, "left.rs", "fn process_left() {}");
    repo.git(&["checkout", "-b", "right", &base]).unwrap();
    let right = capture(&repo, "right.rs", "fn process_right() {}");
    repo.git(&["checkout", "-b", "left", &left]).unwrap();
    repo.git(&["merge", "--no-ff", "right", "-m", "old merge"])
        .unwrap();
    repo.sync_daemon_force();
    repo.filename("left.rs")
        .assert_committed_lines(vec!["fn process_left() {}".ai()]);
    repo.filename("right.rs")
        .assert_committed_lines(vec!["fn process_right() {}".ai()]);
    let old_merge = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    let content = note(&repo, "sense", &left).unwrap();
    repo.git_og(&[
        "notes",
        "--ref=sense",
        "add",
        "-f",
        "-m",
        &content,
        &old_merge,
    ])
    .unwrap();
    let tree = repo
        .git_og(&["rev-parse", "HEAD^{tree}"])
        .unwrap()
        .trim()
        .to_string();
    let new_merge = repo
        .git_og(&[
            "commit-tree",
            &tree,
            "-p",
            &left,
            "-p",
            &right,
            "-m",
            "new merge",
        ])
        .unwrap()
        .trim()
        .to_string();
    let (conflict, conflict_base, conflict_target) = pending_process_commit();
    let (commit, commit_base, commit_target) = pending_process_commit();
    process_probe(
        &repo,
        "notes_ref::process_namespace_stays_fixed_through_transitive_seams",
        json!({
            "main":repo.path(),"base":base,"left":left,"right":right,"old_merge":old_merge,"new_merge":new_merge,
            "conflict":{"repo":conflict.path(),"base":conflict_base,"target":conflict_target},
            "commit":{"repo":commit.path(),"base":commit_base,"target":commit_target}
        }),
    );
}

#[test]
fn process_namespace_failure_stays_cached_after_file_repair() {
    if let Ok(state) = std::env::var("CDE60_PROCESS_STATE") {
        assert!(std::env::var_os("GIT_AI_TEST_CONFIG_PATCH").is_none());
        let state: Value = serde_json::from_str(&state).unwrap();
        let first = git_ai::git::refs::authorship_notes_ref()
            .unwrap_err()
            .to_string();
        let path = git_ai::config::config_file_path_public().unwrap();
        let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        config["notes_ref"] = json!("ai");
        std::fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
        let repo = git_ai::git::find_repository_in_path(state["repo"].as_str().unwrap()).unwrap();
        assert!(
            git_ai::git::notes_api::write_note(
                &repo,
                state["sha"].as_str().unwrap(),
                "must not replace existing note"
            )
            .is_err()
        );
        assert_eq!(
            git_ai::git::refs::authorship_notes_ref()
                .unwrap_err()
                .to_string(),
            first
        );
        assert!(matches!(
            git_ai::git::sync_authorship::notes_push_allowed(&repo, repo.path().to_str().unwrap())
                .unwrap(),
            git_ai::git::sync_authorship::NotesPushDecision::Skipped("invalid notes_ref")
        ));
        return;
    }
    let repo = TestRepo::new_dedicated_daemon();
    let sha = capture(&repo, "cached.rs", "fn cached_failure() {}");
    let tip_before = tip(&repo, "ai");
    let path = repo.test_home_path().join(".git-ai/config.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    config["notes_ref"] = json!("-invalid");
    std::fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
    process_probe(
        &repo,
        "notes_ref::process_namespace_failure_stays_cached_after_file_repair",
        json!({"repo":repo.path(),"sha":sha}),
    );
    assert_eq!(tip(&repo, "ai"), tip_before);
}

#[test]
fn notes_ref_cli_change_requires_daemon_restart() {
    let mut repo = TestRepo::new_dedicated_daemon();
    capture(&repo, "initial.rs", "fn initial_process() {}");
    let output = repo
        .git_ai(&["config", "set", "notes_ref", "sense"])
        .unwrap();
    // The CLI writes the client HOME; mirror the actual saved value into the
    // daemon HOME when they differ, and verify the daemon really sees the edit.
    let daemon_config = repo.daemon_home_path().join(".git-ai/config.json");
    let mut config: Value =
        serde_json::from_slice(&std::fs::read(&daemon_config).unwrap()).unwrap();
    config["notes_ref"] = json!("sense");
    std::fs::write(&daemon_config, serde_json::to_vec(&config).unwrap()).unwrap();
    let changed: Value = serde_json::from_slice(&std::fs::read(&daemon_config).unwrap()).unwrap();
    assert_eq!(changed["notes_ref"], "sense");
    let mut file = repo.filename("without-restart.rs");
    file.set_contents(vec!["fn existing_process() {}".ai()]);
    repo.git(&["add", "without-restart.rs"]).unwrap();
    repo.git(&["commit", "-m", "same daemon"]).unwrap();
    repo.sync_daemon_force();
    file.assert_committed_lines(vec!["fn existing_process() {}".unattributed_human()]);
    let before = repo.git_og(&["rev-parse", "HEAD"]).unwrap();
    assert!(note(&repo, "ai", before.trim()).is_some());
    assert!(note(&repo, "sense", before.trim()).is_none());
    assert!(
        output.contains("new git-ai processes") && output.contains("git-ai bg restart"),
        "{output}"
    );
    let old_tip = tip(&repo, "ai");
    repo.patch_git_ai_config(|patch| patch.notes_ref = Some(json!("sense")));
    repo.restart_dedicated_daemon_for_test();
    let restarted: Value = serde_json::from_slice(&std::fs::read(&daemon_config).unwrap()).unwrap();
    assert_eq!(restarted["notes_ref"], "sense");
    let after = capture(&repo, "after-restart.rs", "fn new_process() {}");
    assert!(note(&repo, "sense", &after).is_some());
    assert!(note(&repo, "ai", &after).is_none());
    assert_eq!(tip(&repo, "ai"), old_tip);
}

#[test]
fn notes_ref_resolution_is_confined_to_cached_accessor() {
    fn visit(path: &std::path::Path) {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(&path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let source = std::fs::read_to_string(&path).unwrap();
                assert!(
                    !source.contains(".notes_ref("),
                    "runtime notes_ref getter outside cached accessor: {}",
                    path.display()
                );
                if !path.ends_with("git/refs.rs") {
                    assert!(
                        !source.contains("Config::load_notes_ref_checked("),
                        "namespace resolution outside accessor: {}",
                        path.display()
                    );
                }
            }
        }
    }
    visit(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"));
    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/git/refs.rs"),
    )
    .unwrap();
    assert_eq!(source.matches("Config::load_notes_ref_checked(").count(), 1);
    let accessor = source
        .split("pub fn authorship_notes_ref()")
        .nth(1)
        .unwrap()
        .split("#[doc(hidden)]")
        .next()
        .unwrap();
    assert_eq!(
        accessor.matches("Config::load_notes_ref_checked(").count(),
        1
    );
    assert!(accessor.contains("OnceLock<Result<AuthorshipNotesRef, String>>"));
}
