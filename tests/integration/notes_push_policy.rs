// Modified by alfypops, 2026: local-only notes push option
use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::{DaemonTestScope, TestRepo};
use serde_json::{Value, json};

fn repos() -> (TestRepo, TestRepo) {
    TestRepo::new_with_remote_with_daemon_scope(DaemonTestScope::Dedicated)
}

// Keep the normal test config patch, and change only these keys on disk so the
// already-running daemon must observe them without a restart.
fn configure(repo: &TestRepo, policy: Value) {
    for home in [repo.test_home_path().clone(), repo.daemon_home_path()] {
        let path = home.join(".git-ai/config.json");
        let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        for (key, value) in policy.as_object().unwrap() {
            config[key] = value.clone();
        }
        std::fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
    }
}

fn commit_ai(repo: &TestRepo) -> String {
    let mut file = repo.filename("policy.rs");
    file.set_contents(vec!["fn local_capture() {}".ai()]);
    let commit = repo.stage_all_and_commit("local capture").unwrap();
    file.assert_committed_lines(vec!["fn local_capture() {}".ai()]);
    assert!(repo.read_authorship_note(&commit.commit_sha).is_some());
    commit.commit_sha
}

fn assert_remote(repo: &TestRepo, remote: &TestRepo, sha: &str, expected: bool) {
    assert_eq!(
        repo.read_authorship_note_in_git_dir(remote.path(), sha)
            .is_some(),
        expected,
        "unexpected remote note for {sha}"
    );
}

#[test]
fn never_normal_push_keeps_local_capture() {
    let (repo, upstream) = repos();
    configure(&repo, json!({"notes_push": "never"}));
    let sha = commit_ai(&repo);
    repo.git(&["push", "origin", "HEAD"]).unwrap();
    assert_remote(&repo, &upstream, &sha, false);
}

#[test]
fn never_push_with_set_upstream() {
    let (repo, upstream) = repos();
    configure(&repo, json!({"notes_push": "never"}));
    let sha = commit_ai(&repo);
    repo.git(&["push", "-u", "origin", "HEAD"]).unwrap();
    assert_remote(&repo, &upstream, &sha, false);
}

#[test]
fn never_cli_reports_skip_and_preserves_machine_json() {
    let (repo, upstream) = repos();
    let sha = commit_ai(&repo);
    configure(&repo, json!({"notes_push": "never"}));
    let output = repo
        .git_ai_command_without_pre_sync_for_test(
            &[
                "push-authorship-notes",
                "--json",
                r#"{"remote_name":"origin"}"#,
            ],
            &[],
        )
        .output()
        .unwrap();
    assert!(output.status.success());
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["ok"], true);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("skipped:") && stderr.contains("notes_push"),
        "{stderr}"
    );
    assert_remote(&repo, &upstream, &sha, false);
}

#[test]
fn never_opt_in_matches_only_destination() {
    let (repo, upstream) = repos();
    let other = TestRepo::new_bare_with_daemon_scope(DaemonTestScope::NoDaemon);
    repo.git_og(&["remote", "add", "other", other.path().to_str().unwrap()])
        .unwrap();
    configure(
        &repo,
        json!({"notes_push":"never", "notes_push_repositories":[format!("{}*", upstream.path().display())]}),
    );
    let sha = commit_ai(&repo);
    repo.git(&["push", "origin", "HEAD"]).unwrap();
    assert_remote(&repo, &upstream, &sha, true);
    repo.git(&["push", "other", "HEAD"]).unwrap();
    assert_remote(&repo, &other, &sha, false);
    // A URL passed directly also goes through the same policy.
    let request = json!({"remote_name": other.path().to_str().unwrap()}).to_string();
    let out = repo
        .git_ai(&["push-authorship-notes", "--json", &request])
        .unwrap();
    assert!(out.contains("skipped:"), "{out}");
    assert_remote(&repo, &other, &sha, false);
}

#[test]
fn exclude_repositories_blocks_existing_notes() {
    let (repo, upstream) = repos();
    let sha = commit_ai(&repo);
    configure(
        &repo,
        json!({"exclude_repositories": [format!("{}*", upstream.path().display())]}),
    );
    repo.git(&["push", "origin", "HEAD"]).unwrap();
    assert_remote(&repo, &upstream, &sha, false);
}

#[test]
fn hot_reload_never_without_daemon_restart() {
    let (repo, upstream) = repos();
    let sha = commit_ai(&repo);
    repo.git(&["push", "origin", "HEAD"]).unwrap();
    assert_remote(&repo, &upstream, &sha, true);
    let socket = repo.daemon_control_socket_path();
    configure(&repo, json!({"notes_push":"never"}));
    let mut file = repo.filename("next.rs");
    file.set_contents(vec!["fn next() {}".ai()]);
    let next = repo.stage_all_and_commit("after config change").unwrap();
    file.assert_committed_lines(vec!["fn next() {}".ai()]);
    assert!(repo.read_authorship_note(&next.commit_sha).is_some());
    repo.git(&["push", "origin", "HEAD"]).unwrap();
    assert_eq!(socket, repo.daemon_control_socket_path());
    assert_remote(&repo, &upstream, &next.commit_sha, false);
}

#[test]
fn notes_push_config_cli_round_trip_and_env_precedence() {
    let repo = TestRepo::new_dedicated_daemon();
    let get = |key| -> Value {
        serde_json::from_str(repo.git_ai(&["config", key]).unwrap().trim()).unwrap()
    };
    assert_eq!(get("notes_push"), "always");
    repo.git_ai(&["config", "set", "notes_push", "never"])
        .unwrap();
    assert_eq!(get("notes_push"), "never");
    let overridden = repo
        .git_ai_with_env(
            &["config", "notes_push"],
            &[("GIT_AI_NOTES_PUSH", "always")],
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(overridden.trim()).unwrap(),
        "always"
    );
    repo.git_ai(&[
        "config",
        "set",
        "notes_push_repositories",
        "https://example.com/team/*",
    ])
    .unwrap();
    repo.git_ai(&[
        "config",
        "--add",
        "notes_push_repositories",
        "git@example.com:other/*",
    ])
    .unwrap();
    assert_eq!(
        get("notes_push_repositories"),
        json!(["https://example.com/team/*", "git@example.com:other/*"])
    );
    let effective: Value = serde_json::from_str(repo.git_ai(&["config"]).unwrap().trim()).unwrap();
    assert_eq!(effective["notes_push"], "never");
    assert_eq!(
        effective["notes_push_repositories"],
        get("notes_push_repositories")
    );
    repo.git_ai(&["config", "unset", "notes_push_repositories"])
        .unwrap();
    repo.git_ai(&["config", "unset", "notes_push"]).unwrap();
    assert_eq!(get("notes_push_repositories"), json!([]));
    assert_eq!(get("notes_push"), "always");
}

#[test]
fn never_pushurl_uses_actual_destination() {
    let (repo, upstream) = repos();
    let other = TestRepo::new_bare_with_daemon_scope(DaemonTestScope::NoDaemon);
    repo.git_og(&[
        "remote",
        "set-url",
        "--push",
        "origin",
        other.path().to_str().unwrap(),
    ])
    .unwrap();
    configure(
        &repo,
        json!({"notes_push":"never", "notes_push_repositories":[format!("{}*", upstream.path().display())]}),
    );
    let sha = commit_ai(&repo);
    repo.git(&["push", "origin", "HEAD"]).unwrap();
    assert_remote(&repo, &other, &sha, false);
    configure(
        &repo,
        json!({"notes_push":"never", "notes_push_repositories":[format!("{}*", other.path().display())]}),
    );
    let out = repo
        .git_ai(&[
            "push-authorship-notes",
            "--json",
            r#"{"remote_name":"origin"}"#,
        ])
        .unwrap();
    assert!(!out.contains("skipped:"), "{out}");
    assert_remote(&repo, &other, &sha, true);
}

#[test]
fn exclude_repository_blocks_push_to_another_remote() {
    let (repo, upstream) = repos();
    let other = TestRepo::new_bare_with_daemon_scope(DaemonTestScope::NoDaemon);
    repo.git_og(&["remote", "add", "other", other.path().to_str().unwrap()])
        .unwrap();
    let sha = commit_ai(&repo);
    configure(
        &repo,
        json!({"exclude_repositories":[format!("{}*", upstream.path().display())]}),
    );
    repo.git(&["push", "other", "HEAD"]).unwrap();
    assert_remote(&repo, &other, &sha, false);
}

#[test]
fn never_env_override_blocks_cli() {
    let (repo, upstream) = repos();
    let sha = commit_ai(&repo);
    configure(&repo, json!({"notes_push":"always"}));
    let out = repo
        .git_ai_with_env(
            &[
                "push-authorship-notes",
                "--json",
                r#"{"remote_name":"origin"}"#,
            ],
            &[("GIT_AI_NOTES_PUSH", "never")],
        )
        .unwrap();
    assert!(out.contains("skipped:"), "{out}");
    assert_remote(&repo, &upstream, &sha, false);
}

#[test]
fn never_multiple_pushurls_require_every_destination_to_match() {
    let (repo, upstream) = repos();
    let other = TestRepo::new_bare_with_daemon_scope(DaemonTestScope::NoDaemon);
    repo.git_og(&[
        "remote",
        "set-url",
        "--add",
        "--push",
        "origin",
        upstream.path().to_str().unwrap(),
    ])
    .unwrap();
    repo.git_og(&[
        "remote",
        "set-url",
        "--add",
        "--push",
        "origin",
        other.path().to_str().unwrap(),
    ])
    .unwrap();
    configure(
        &repo,
        json!({"notes_push":"never", "notes_push_repositories":[format!("{}*", upstream.path().display())]}),
    );
    let sha = commit_ai(&repo);
    repo.git(&["push", "origin", "HEAD"]).unwrap();
    assert_remote(&repo, &upstream, &sha, false);
    assert_remote(&repo, &other, &sha, false);
}

#[test]
fn notes_push_test_config_patch_is_respected() {
    let (repo, upstream) = repos();
    let sha = commit_ai(&repo);
    let out = repo
        .git_ai_with_env(
            &[
                "push-authorship-notes",
                "--json",
                r#"{"remote_name":"origin"}"#,
            ],
            &[("GIT_AI_TEST_CONFIG_PATCH", r#"{"notes_push":"never"}"#)],
        )
        .unwrap();
    assert!(out.contains("skipped:"), "{out}");
    assert_remote(&repo, &upstream, &sha, false);
    let out = repo
        .git_ai_with_env(
            &[
                "push-authorship-notes",
                "--json",
                r#"{"remote_name":"origin"}"#,
            ],
            &[
                ("GIT_AI_TEST_CONFIG_PATCH", r#"{"notes_push":"never"}"#),
                ("GIT_AI_NOTES_PUSH", "always"),
            ],
        )
        .unwrap();
    assert!(!out.contains("skipped:"), "{out}");
    assert_remote(&repo, &upstream, &sha, true);
}

fn url_rewrite_checks_resolved_destination(kind: &str, named: bool) {
    let (repo, upstream) = repos();
    let other = TestRepo::new_bare_with_daemon_scope(DaemonTestScope::NoDaemon);
    let alias = upstream.path().to_str().unwrap();
    // Both sides are local paths, including the fetch URL for pushInsteadOf.
    // The longer prefix must win over the deliberately different broad rule.
    repo.git_og(&[
        "config",
        &format!("url.{}.{}", upstream.path().display(), kind),
        upstream.path().parent().unwrap().to_str().unwrap(),
    ])
    .unwrap();
    repo.git_og(&[
        "config",
        &format!("url.{}.{}", other.path().display(), kind),
        alias,
    ])
    .unwrap();
    let sha = commit_ai(&repo);
    configure(
        &repo,
        json!({"notes_push":"never", "notes_push_repositories":[alias], "exclude_repositories":[other.path().to_str().unwrap()]}),
    );
    repo.git(&["push", if named { "origin" } else { alias }, "HEAD"])
        .unwrap();
    let request = json!({"remote_name": if named {"origin"} else {alias}}).to_string();
    let out = repo
        .git_ai(&["push-authorship-notes", "--json", &request])
        .unwrap();
    assert!(out.contains("skipped:"), "{out}");
    assert_remote(&repo, &other, &sha, false);
    configure(
        &repo,
        json!({"notes_push":"never", "notes_push_repositories":[other.path().to_str().unwrap()], "exclude_repositories":[]}),
    );
    let out = repo
        .git_ai(&["push-authorship-notes", "--json", &request])
        .unwrap();
    assert!(!out.contains("skipped:"), "{out}");
    assert_remote(&repo, &other, &sha, true);
}

#[test]
fn url_rewrite_literal_instead_of() {
    url_rewrite_checks_resolved_destination("insteadOf", false);
}

#[test]
fn url_rewrite_literal_push_instead_of() {
    url_rewrite_checks_resolved_destination("pushInsteadOf", false);
}

#[test]
fn url_rewrite_named_instead_of() {
    url_rewrite_checks_resolved_destination("insteadOf", true);
}

#[test]
fn url_rewrite_named_push_instead_of() {
    url_rewrite_checks_resolved_destination("pushInsteadOf", true);
}

fn observed_url_rewrite_policy(chained: bool, named: bool) {
    let (repo, instead_of) = repos();
    let push_instead_of = TestRepo::new_bare_with_daemon_scope(DaemonTestScope::NoDaemon);
    // Use the normal push path to seed real captured notes on both remotes.
    let sha = commit_ai(&repo);
    for destination in [&instead_of, &push_instead_of] {
        repo.git(&["push", destination.path().to_str().unwrap(), "HEAD"])
            .unwrap();
        assert_remote(&repo, destination, &sha, true);
    }
    let alias = format!("{}/overlapping-url-alias", repo.path().display());
    repo.git_og(&["remote", "set-url", "origin", &alias])
        .unwrap();
    for (destination, rule, prefix) in [
        (
            &instead_of,
            "insteadOf",
            if chained {
                push_instead_of.path().to_str().unwrap()
            } else {
                alias.as_str()
            },
        ),
        (&push_instead_of, "pushInsteadOf", alias.as_str()),
    ] {
        repo.git_og(&[
            "config",
            &format!("url.{}.{}", destination.path().display(), rule),
            prefix,
        ])
        .unwrap();
    }
    configure(&repo, json!({"notes_push":"never"}));
    // Observe Git itself rather than assuming precedence or rewrite chaining.
    let target = if named { "origin" } else { alias.as_str() };
    repo.git_og(&["push", target, "HEAD:refs/heads/rewrite-choice"])
        .unwrap();
    let branch_ref = "refs/heads/rewrite-choice";
    let received = |remote: &TestRepo| {
        remote
            .git_og(&["rev-parse", "--verify", branch_ref])
            .is_ok_and(|actual| actual.trim() == sha)
    };
    let observed = (received(&instead_of), received(&push_instead_of));
    let (chosen, nonchosen) = match observed {
        (true, false) => (&instead_of, &push_instead_of),
        (false, true) => (&push_instead_of, &instead_of),
        _ => panic!("expected exactly one Git push destination, observed {observed:?}"),
    };
    println!(
        "URL rewrite probe: chained={chained}, named={named}, receiver={}",
        if chosen.path() == push_instead_of.path() {
            "B"
        } else {
            "C"
        }
    );
    configure(
        &repo,
        json!({"notes_push":"never", "notes_push_repositories":[chosen.path().to_str().unwrap()]}),
    );
    let mut allowed_file = repo.filename("allowed-next.rs");
    allowed_file.set_contents(vec!["fn allowed_next() {}".ai()]);
    let allowed = repo.stage_all_and_commit("allowed destination").unwrap();
    allowed_file.assert_committed_lines(vec!["fn allowed_next() {}".ai()]);
    assert!(repo.read_authorship_note(&allowed.commit_sha).is_some());
    repo.git_og(&["push", target, "HEAD"]).unwrap();
    let request = json!({"remote_name":target}).to_string();
    let out = repo
        .git_ai(&["push-authorship-notes", "--json", &request])
        .unwrap();
    assert!(!out.contains("skipped:"), "{out}");
    assert_remote(&repo, chosen, &allowed.commit_sha, true);
    assert_remote(&repo, nonchosen, &allowed.commit_sha, false);

    configure(
        &repo,
        json!({"notes_push":"always", "exclude_repositories":[chosen.path().to_str().unwrap()]}),
    );
    let mut file = repo.filename("excluded-next.rs");
    file.set_contents(vec!["fn excluded_next() {}".ai()]);
    let next = repo.stage_all_and_commit("excluded destination").unwrap();
    file.assert_committed_lines(vec!["fn excluded_next() {}".ai()]);
    assert!(repo.read_authorship_note(&next.commit_sha).is_some());
    repo.git(&["push", target, "HEAD"]).unwrap();
    let out = repo
        .git_ai(&["push-authorship-notes", "--json", &request])
        .unwrap();
    assert!(
        out.contains("skipped:") && out.contains("exclude_repositories"),
        "{out}"
    );
    assert_remote(&repo, chosen, &next.commit_sha, false);
    assert_remote(&repo, nonchosen, &next.commit_sha, false);
}

#[test]
fn overlapping_url_rewrites_match_observed_git_push_destination() {
    observed_url_rewrite_policy(false, true);
}

#[test]
fn chained_url_rewrite_named_matches_observed_git_push_destination() {
    observed_url_rewrite_policy(true, true);
}

#[test]
fn chained_url_rewrite_literal_matches_observed_git_push_destination() {
    observed_url_rewrite_policy(true, false);
}

#[test]
fn never_cli_opt_in_accepts_literal_local_destination() {
    let (repo, upstream) = repos();
    let other = TestRepo::new_bare_with_daemon_scope(DaemonTestScope::NoDaemon);
    repo.git_ai(&["config", "set", "notes_push", "never"])
        .unwrap();
    repo.git_ai(&[
        "config",
        "set",
        "notes_push_repositories",
        upstream.path().to_str().unwrap(),
    ])
    .unwrap();
    repo.git_ai(&[
        "config",
        "--add",
        "notes_push_repositories",
        other.path().to_str().unwrap(),
    ])
    .unwrap();
    let list: Value = serde_json::from_str(
        repo.git_ai(&["config", "notes_push_repositories"])
            .unwrap()
            .trim(),
    )
    .unwrap();
    assert_eq!(
        list,
        json!([
            upstream.path().to_str().unwrap(),
            other.path().to_str().unwrap()
        ])
    );
    let sha = commit_ai(&repo);
    repo.git(&["push", "origin", "HEAD"]).unwrap();
    assert_remote(&repo, &upstream, &sha, true);
}

#[test]
fn url_rewrite_literal_honors_active_onbranch_include() {
    let (repo, upstream) = repos();
    let other = TestRepo::new_bare_with_daemon_scope(DaemonTestScope::NoDaemon);
    let sha = commit_ai(&repo);
    let alias = upstream.path().to_str().unwrap();
    let include = repo.path().join(".git/notes-push-urls.config");
    std::fs::write(
        &include,
        format!(
            "[url \"{}\"]\n    pushInsteadOf = {}\n",
            other.path().display(),
            alias
        ),
    )
    .unwrap();
    repo.git_og(&[
        "config",
        &format!("includeIf.onbranch:{}.path", repo.current_branch()),
        include.to_str().unwrap(),
    ])
    .unwrap();
    configure(
        &repo,
        json!({"notes_push":"never", "notes_push_repositories":[alias], "exclude_repositories":[other.path().to_str().unwrap()]}),
    );
    repo.git(&["push", alias, "HEAD"]).unwrap();
    let request = json!({"remote_name":alias}).to_string();
    let out = repo
        .git_ai(&["push-authorship-notes", "--json", &request])
        .unwrap();
    assert!(out.contains("skipped:"), "{out}");
    assert_remote(&repo, &other, &sha, false);
    configure(
        &repo,
        json!({"notes_push":"never", "notes_push_repositories":[other.path().to_str().unwrap()], "exclude_repositories":[]}),
    );
    let out = repo
        .git_ai(&["push-authorship-notes", "--json", &request])
        .unwrap();
    assert!(!out.contains("skipped:"), "{out}");
    assert_remote(&repo, &other, &sha, true);
}

fn excluded_origin_identity_blocks_opted_in_other(conditional: bool) {
    let (repo, origin) = repos();
    let other = TestRepo::new_bare_with_daemon_scope(DaemonTestScope::NoDaemon);
    let sha = commit_ai(&repo);
    let alias = format!("{}/notes-origin-alias", repo.path().display());
    repo.git_og(&["remote", "set-url", "origin", &alias])
        .unwrap();
    repo.git_og(&["remote", "add", "other", other.path().to_str().unwrap()])
        .unwrap();
    if conditional {
        let include = repo.path().join(".git/excluded-origin.config");
        std::fs::write(
            &include,
            format!(
                "[url \"{}\"]\n    pushInsteadOf = {}\n",
                origin.path().display(),
                alias
            ),
        )
        .unwrap();
        repo.git_og(&[
            "config",
            &format!("includeIf.onbranch:{}.path", repo.current_branch()),
            include.to_str().unwrap(),
        ])
        .unwrap();
    } else {
        repo.git_og(&[
            "config",
            &format!("url.{}.insteadOf", origin.path().display()),
            &alias,
        ])
        .unwrap();
    }
    configure(
        &repo,
        json!({"notes_push":"never", "notes_push_repositories":[other.path().to_str().unwrap()], "exclude_repositories":[origin.path().to_str().unwrap()]}),
    );
    repo.git_og(&["push", "other", "HEAD"]).unwrap();
    let out = repo
        .git_ai(&[
            "push-authorship-notes",
            "--json",
            r#"{"remote_name":"other"}"#,
        ])
        .unwrap();
    assert!(
        out.contains("skipped:") && out.contains("exclude_repositories"),
        "{out}"
    );
    assert_remote(&repo, &other, &sha, false);
}

#[test]
fn excluded_origin_alias_blocks_opted_in_other() {
    excluded_origin_identity_blocks_opted_in_other(false);
}

#[test]
fn excluded_origin_onbranch_alias_blocks_opted_in_other() {
    excluded_origin_identity_blocks_opted_in_other(true);
}

#[test]
fn excluded_raw_pushurl_alias_blocks_opted_in_other() {
    let (repo, origin) = repos();
    let other = TestRepo::new_bare_with_daemon_scope(DaemonTestScope::NoDaemon);
    let sha = commit_ai(&repo);
    let alias = format!("{}/notes-origin-push-alias", repo.path().display());
    repo.git_og(&["config", "remote.origin.pushurl", &alias])
        .unwrap();
    repo.git_og(&[
        "config",
        &format!("url.{}.insteadOf", origin.path().display()),
        &alias,
    ])
    .unwrap();
    repo.git_og(&["remote", "add", "other", other.path().to_str().unwrap()])
        .unwrap();
    configure(
        &repo,
        json!({"notes_push":"never", "notes_push_repositories":[other.path().to_str().unwrap()], "exclude_repositories":[alias]}),
    );
    repo.git_og(&["push", "other", "HEAD"]).unwrap();
    let out = repo
        .git_ai(&[
            "push-authorship-notes",
            "--json",
            r#"{"remote_name":"other"}"#,
        ])
        .unwrap();
    assert!(
        out.contains("skipped:") && out.contains("exclude_repositories"),
        "{out}"
    );
    assert_remote(&repo, &other, &sha, false);
}

#[test]
fn actual_destination_exclusion_wins_over_opt_in() {
    let (repo, upstream) = repos();
    let sha = commit_ai(&repo);
    configure(
        &repo,
        json!({"notes_push":"never", "notes_push_repositories":[upstream.path().to_str().unwrap()], "exclude_repositories":[upstream.path().to_str().unwrap()]}),
    );
    repo.git(&["push", "origin", "HEAD"]).unwrap();
    let out = repo
        .git_ai(&[
            "push-authorship-notes",
            "--json",
            r#"{"remote_name":"origin"}"#,
        ])
        .unwrap();
    assert!(
        out.contains("skipped:") && out.contains("exclude_repositories"),
        "{out}"
    );
    assert_remote(&repo, &upstream, &sha, false);
}

#[test]
fn incomplete_config_during_write_skips_notes_push() {
    let (repo, upstream) = repos();
    configure(&repo, json!({"notes_push":"never"}));
    let sha = commit_ai(&repo);
    repo.git_og(&["push", "origin", "HEAD"]).unwrap();
    let path = repo.test_home_path().join(".git-ai/config.json");
    for contents in ["", "{", "{\"notes_push\":"] {
        std::fs::write(&path, contents).unwrap();
        let out = repo
            .git_ai(&[
                "push-authorship-notes",
                "--json",
                r#"{"remote_name":"origin"}"#,
            ])
            .unwrap();
        assert!(out.contains("skipped: config unreadable"), "{out}");
        assert_remote(&repo, &upstream, &sha, false);
    }
}

#[test]
fn missing_config_preserves_default_notes_push() {
    let (repo, upstream) = repos();
    let sha = commit_ai(&repo);
    configure(&repo, json!({"notes_push":"never"}));
    repo.git_og(&["push", "origin", "HEAD"]).unwrap();
    std::fs::remove_file(repo.test_home_path().join(".git-ai/config.json")).unwrap();
    let out = repo
        .git_ai(&[
            "push-authorship-notes",
            "--json",
            r#"{"remote_name":"origin"}"#,
        ])
        .unwrap();
    assert!(!out.contains("skipped:"), "{out}");
    assert_remote(&repo, &upstream, &sha, true);
}

#[test]
fn config_cli_save_keeps_open_reader_snapshot_complete() {
    use std::io::Read;
    let repo = TestRepo::new_dedicated_daemon();
    let path = repo.test_home_path().join(".git-ai/config.json");
    let before = std::fs::read(&path).unwrap();
    let mut reader = std::fs::File::open(&path).unwrap();
    repo.git_ai(&["config", "set", "notes_push", "never"])
        .unwrap();
    let mut snapshot = Vec::new();
    reader.read_to_end(&mut snapshot).unwrap();
    assert_eq!(
        snapshot, before,
        "an open reader must retain a complete pre-save snapshot"
    );
    let after = std::fs::read(&path).unwrap();
    assert_ne!(after, before);
    let saved: Value = serde_json::from_slice(&after).unwrap();
    assert_eq!(saved["notes_push"], "never");
}

#[test]
fn ci_local_never_reports_skip_instead_of_pushed() {
    let (repo, upstream) = repos();
    configure(&repo, json!({"notes_push":"never"}));
    let base_sha = commit_ai(&repo);
    repo.git_og(&["branch", "-M", "main"]).unwrap();
    repo.git_og(&["push", "-u", "origin", "main"]).unwrap();
    repo.git_og(&["checkout", "-b", "feature"]).unwrap();
    let mut feat = repo.filename("feat.txt");
    feat.set_contents(vec!["// ai line".ai()]);
    let head_sha = repo.stage_all_and_commit("feature").unwrap().commit_sha;
    feat.assert_committed_lines(vec!["// ai line".ai()]);
    repo.git_og(&["push", "-u", "origin", "feature"]).unwrap();
    repo.git_og(&["checkout", "main"]).unwrap();
    repo.git_og(&["merge", "--squash", "feature"]).unwrap();
    repo.git_og(&["commit", "-m", "squash merge"]).unwrap();
    let merge_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    repo.git_og(&["push", "origin", "main"]).unwrap();
    let out = repo
        .git_ai(&[
            "ci",
            "local",
            "merge",
            "--merge-commit-sha",
            &merge_sha,
            "--base-ref",
            "main",
            "--head-ref",
            "feature",
            "--head-sha",
            &head_sha,
            "--base-sha",
            &base_sha,
            "--skip-fetch",
        ])
        .unwrap();
    assert!(
        out.contains("Skipping authorship push:") && out.contains("notes_push"),
        "{out}"
    );
    assert!(!out.contains("Pushed authorship. Done."), "{out}");
    feat.assert_committed_lines(vec!["// ai line".ai()]);
    assert!(repo.read_authorship_note(&merge_sha).is_some());
    assert_remote(&repo, &upstream, &merge_sha, false);
}
