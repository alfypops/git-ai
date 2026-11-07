// Modified by alfypops, 2026: families outside daemon_repo_roots are ignored too.
//! Repositories the untraced-commit fixup leaves alone. AI agents create
//! scratch repositories under the OS temp directory for their own work; the
//! fixup must not remember, scan, persist or backfill them. The temp roots and
//! `untraced_fixup_ignored_paths` globs are scoped to the fixup worker and its
//! repo-family store: traced commands and checkpoints in such repositories keep
//! their normal behaviour. `daemon_repo_roots` is the exception: it is
//! daemon-wide (the trace and checkpoint gates apply it too), and here it
//! makes the fixup leave families outside the list alone as well.

use crate::config::{Config, DaemonRepoRoots};
use glob::Pattern;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Where temporary repositories live, operator-configured path globs, and the
/// current `daemon_repo_roots`.
#[derive(Debug, Clone, Default)]
pub struct UntracedFixupIgnore {
    /// Canonical temp roots; a family whose common dir is under one is ignored.
    temp_roots: Vec<PathBuf>,
    /// `untraced_fixup_ignored_paths` globs, matched against the POSIX form of
    /// the family's canonical common dir.
    patterns: Vec<Pattern>,
    /// `daemon_repo_roots`: with a non-empty list, families outside it are
    /// ignored like everything else the daemon ignores.
    repo_roots: Arc<DaemonRepoRoots>,
}

impl UntracedFixupIgnore {
    pub fn from_config(config: &Config) -> Self {
        let temp_roots = if config.get_feature_flags().untraced_fixup_ignore_temp_repos {
            default_temp_roots()
        } else {
            Vec::new()
        };
        Self::new(temp_roots, config.untraced_fixup_ignored_paths().to_vec())
    }

    /// `temp_roots` are canonicalized and de-duplicated once here; roots that
    /// do not exist are dropped.
    pub fn new(temp_roots: Vec<PathBuf>, patterns: Vec<Pattern>) -> Self {
        let mut roots: Vec<PathBuf> = temp_roots
            .into_iter()
            .filter_map(|root| root.canonicalize().ok())
            .collect();
        roots.sort();
        roots.dedup();
        Self {
            temp_roots: roots,
            patterns,
            repo_roots: Arc::default(),
        }
    }

    pub fn with_repo_roots(mut self, repo_roots: Arc<DaemonRepoRoots>) -> Self {
        self.repo_roots = repo_roots;
        self
    }

    /// Whether the fixup must leave this family (a canonical common dir) alone.
    /// A component-wise prefix test per root and a glob match per pattern; no
    /// filesystem access.
    pub fn ignores(&self, common_dir: &str) -> bool {
        !self.repo_roots.allows(Path::new(common_dir)) || self.ignores_by_path(common_dir)
    }

    /// Whether maintenance may forget this family's stored rows. A list that
    /// fails closed without naming any root (an invalid value) only pauses the
    /// fixup: forgetting every family then would lose their cursors for good.
    pub fn forgets(&self, common_dir: &str) -> bool {
        if self.repo_roots.is_active() && !self.repo_roots.has_entries() {
            return self.ignores_by_path(common_dir);
        }
        self.ignores(common_dir)
    }

    fn ignores_by_path(&self, common_dir: &str) -> bool {
        let path = Path::new(common_dir);
        if self.temp_roots.iter().any(|root| path.starts_with(root)) {
            return true;
        }
        if self.patterns.is_empty() {
            return false;
        }
        let posix = crate::utils::normalize_to_posix(common_dir);
        self.patterns.iter().any(|pattern| pattern.matches(&posix))
    }

    pub fn temp_roots(&self) -> &[PathBuf] {
        &self.temp_roots
    }
}

/// The OS temp locations agents put scratch repositories in: the process temp
/// dir (`TMPDIR` / `TEMP` / `TMP`) and the platform's well-known ones.
pub fn default_temp_roots() -> Vec<PathBuf> {
    let mut roots = vec![std::env::temp_dir()];
    if cfg!(unix) {
        roots.push(PathBuf::from("/tmp"));
        roots.push(PathBuf::from("/var/tmp"));
    }
    if cfg!(target_os = "macos") {
        roots.push(PathBuf::from("/private/tmp"));
        roots.push(PathBuf::from("/var/folders"));
        roots.push(PathBuf::from("/private/var/folders"));
    }
    if cfg!(windows) {
        for var in ["TEMP", "TMP"] {
            if let Some(value) = std::env::var_os(var) {
                roots.push(PathBuf::from(value));
            }
        }
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            roots.push(PathBuf::from(local).join("Temp"));
        }
    }
    roots
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn canonical(path: &Path) -> String {
        path.canonicalize().unwrap().to_string_lossy().to_string()
    }

    #[test]
    fn ignores_families_under_a_temp_root_but_not_siblings_or_lookalikes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tmp");
        let inside = root.join("scratch-repo").join(".git");
        let lookalike = temp.path().join("tmpfoo").join("repo").join(".git");
        let sibling = temp.path().join("projects").join("real").join(".git");
        for dir in [&inside, &lookalike, &sibling] {
            fs::create_dir_all(dir).unwrap();
        }
        let ignore = UntracedFixupIgnore::new(vec![root.clone()], Vec::new());

        assert!(ignore.ignores(&canonical(&inside)));
        assert!(
            !ignore.ignores(&canonical(&lookalike)),
            "prefix matching is by path component, not by string"
        );
        assert!(!ignore.ignores(&canonical(&sibling)));
    }

    #[test]
    fn families_outside_daemon_repo_roots_are_ignored() {
        let temp = tempfile::tempdir().unwrap();
        let listed = temp.path().join("listed").join(".git");
        let unlisted = temp.path().join("unlisted").join(".git");
        for dir in [&listed, &unlisted] {
            fs::create_dir_all(dir).unwrap();
        }
        let (roots, _) = crate::config::parse_daemon_repo_roots(Some(&serde_json::json!([temp
            .path()
            .join("listed")])));
        let ignore = UntracedFixupIgnore::new(Vec::new(), Vec::new());
        assert!(!ignore.ignores(&canonical(&unlisted)), "no list: tracked");

        let ignore = ignore.with_repo_roots(Arc::new(roots));
        assert!(!ignore.ignores(&canonical(&listed)));
        assert!(ignore.ignores(&canonical(&unlisted)));
    }

    #[test]
    fn maintenance_forgets_unlisted_families_only_when_the_list_names_a_root() {
        let temp = tempfile::tempdir().unwrap();
        let scratch_root = temp.path().join("tmp");
        let scratch = scratch_root.join("scratch").join(".git");
        let listed = temp.path().join("listed").join(".git");
        let unlisted = temp.path().join("unlisted").join(".git");
        for dir in [&scratch, &listed, &unlisted] {
            fs::create_dir_all(dir).unwrap();
        }
        let base = UntracedFixupIgnore::new(vec![scratch_root], Vec::new());
        let with_list = |value: serde_json::Value| {
            let (roots, _) = crate::config::parse_daemon_repo_roots(Some(&value));
            base.clone().with_repo_roots(Arc::new(roots))
        };

        let valid = with_list(serde_json::json!([temp.path().join("listed")]));
        assert!(valid.forgets(&canonical(&unlisted)));
        assert!(!valid.forgets(&canonical(&listed)));

        // An invalid value fails closed (nothing is scanned) but names no
        // root, so maintenance keeps every family's rows.
        let fail_closed = with_list(serde_json::json!("not-a-list"));
        assert!(fail_closed.ignores(&canonical(&listed)));
        assert!(fail_closed.ignores(&canonical(&unlisted)));
        assert!(!fail_closed.forgets(&canonical(&listed)));
        assert!(!fail_closed.forgets(&canonical(&unlisted)));
        assert!(
            fail_closed.forgets(&canonical(&scratch)),
            "path rules still forget scratch repos"
        );
    }

    #[test]
    fn temp_roots_are_canonicalized_deduplicated_and_pruned_of_missing_ones() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tmp");
        fs::create_dir_all(&root).unwrap();
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut roots = vec![
            root.clone(),
            root.join(".").join("..").join("tmp"),
            temp.path().join("does-not-exist"),
        ];
        #[cfg(unix)]
        {
            let link = temp.path().join("tmp-link");
            std::os::unix::fs::symlink(&root, &link).unwrap();
            roots.push(link);
        }
        let ignore = UntracedFixupIgnore::new(roots, Vec::new());

        assert_eq!(ignore.temp_roots(), &[root.canonicalize().unwrap()]);
    }

    #[test]
    fn configured_globs_match_the_posix_form_of_the_common_dir() {
        let temp = tempfile::tempdir().unwrap();
        let scratch = temp
            .path()
            .join("work")
            .join("agent-scratch-42")
            .join(".git");
        let real = temp.path().join("work").join("real").join(".git");
        fs::create_dir_all(&scratch).unwrap();
        fs::create_dir_all(&real).unwrap();
        let ignore = UntracedFixupIgnore::new(
            Vec::new(),
            vec![Pattern::new("*/agent-scratch-*/.git").unwrap()],
        );

        assert!(ignore.ignores(&canonical(&scratch)));
        assert!(!ignore.ignores(&canonical(&real)));
    }

    #[test]
    fn without_temp_roots_only_globs_apply() {
        let temp = tempfile::tempdir().unwrap();
        let inside = temp.path().join("tmp").join("repo").join(".git");
        fs::create_dir_all(&inside).unwrap();
        let ignore = UntracedFixupIgnore::new(Vec::new(), Vec::new());
        assert!(!ignore.ignores(&canonical(&inside)));
    }

    #[test]
    fn default_temp_roots_include_the_process_temp_dir() {
        let roots = default_temp_roots();
        assert!(roots.contains(&std::env::temp_dir()));
        let ignore = UntracedFixupIgnore::new(roots, Vec::new());
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo").join(".git");
        fs::create_dir_all(&repo).unwrap();
        assert!(ignore.ignores(&canonical(&repo)));
    }
}
