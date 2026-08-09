//! Git hook integration for graphify.
//!
//! Installs/uninstalls post-commit and post-checkout hooks that trigger
//! incremental graph rebuilds. Port of Python `hooks.py`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use thiserror::Error;

/// Marker delimiters used to identify the graphify hook block.
const HOOK_MARKER_START: &str = "# graphify-rs-hook-start";
const HOOK_MARKER_END: &str = "# graphify-rs-hook-end";

/// The hook script block injected into git hooks.
const HOOK_SCRIPT: &str = r"
# graphify-rs-hook-start
# Auto-run graphify-rs AST extraction on commit (code-only, no LLM)
if command -v graphify-rs >/dev/null 2>&1; then
  graphify-rs build --code-only --output graphify-rs-out &
fi
# graphify-rs-hook-end
";

/// Hook names that graphify manages.
const MANAGED_HOOKS: &[&str] = &["post-commit", "post-checkout"];

/// Errors from hook management.
#[derive(Debug, Error)]
pub enum HookError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("not a git repository (missing .git/hooks): {0}")]
    NotGitRepo(String),
}

/// Install graphify git hooks in the repository at `repo_root`.
///
/// Installs post-commit and post-checkout hooks. If the hook files already
/// exist, the graphify block is appended (or replaced if already present).
pub fn install_hooks(repo_root: &Path) -> Result<String, HookError> {
    let hooks_dir = repo_root.join(".git/hooks");
    if !hooks_dir.exists() {
        return Err(HookError::NotGitRepo(repo_root.display().to_string()));
    }

    for hook_name in MANAGED_HOOKS {
        install_single_hook(&hooks_dir, hook_name)?;
    }

    Ok("Git hooks installed (post-commit, post-checkout)".to_string())
}

/// Install a single hook file, preserving any existing content.
fn install_single_hook(hooks_dir: &Path, name: &str) -> Result<(), HookError> {
    let hook_path = hooks_dir.join(name);

    let mut content = if hook_path.exists() {
        fs::read_to_string(&hook_path)?
    } else {
        "#!/bin/sh\n".to_string()
    };

    content = strip_marker_block(&content);

    content.push_str(HOOK_SCRIPT);

    fs::write(&hook_path, &content)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o755))?;
    }

    Ok(())
}

/// Uninstall graphify git hooks from the repository at `repo_root`.
///
/// Removes the graphify marker block from each managed hook file. If the
/// resulting file contains only the shebang line (or is empty), the hook
/// file is deleted.
pub fn uninstall_hooks(repo_root: &Path) -> Result<String, HookError> {
    let hooks_dir = repo_root.join(".git/hooks");
    if !hooks_dir.exists() {
        return Err(HookError::NotGitRepo(repo_root.display().to_string()));
    }

    for hook_name in MANAGED_HOOKS {
        uninstall_single_hook(&hooks_dir, hook_name)?;
    }

    Ok("Git hooks removed (post-commit, post-checkout)".to_string())
}

/// Remove the graphify block from a single hook file.
fn uninstall_single_hook(hooks_dir: &Path, name: &str) -> Result<(), HookError> {
    let hook_path = hooks_dir.join(name);
    if !hook_path.exists() {
        return Ok(());
    }

    let content = fs::read_to_string(&hook_path)?;
    let cleaned = strip_marker_block(&content);
    let trimmed = cleaned.trim();

    if trimmed.is_empty() || trimmed == "#!/bin/sh" || trimmed == "#!/bin/bash" {
        fs::remove_file(&hook_path)?;
    } else {
        fs::write(&hook_path, &cleaned)?;
    }

    Ok(())
}

/// Check whether graphify hooks are installed in the repository at `repo_root`.
///
/// Returns a human-readable status string.
pub fn hook_status(repo_root: &Path) -> Result<String, HookError> {
    let hooks_dir = repo_root.join(".git/hooks");
    if !hooks_dir.exists() {
        return Err(HookError::NotGitRepo(repo_root.display().to_string()));
    }

    let mut installed = Vec::new();
    let mut missing = Vec::new();

    for hook_name in MANAGED_HOOKS {
        let hook_path = hooks_dir.join(hook_name);
        if hook_path.exists() {
            let content = fs::read_to_string(&hook_path)?;
            if content.contains(HOOK_MARKER_START) {
                installed.push(*hook_name);
            } else {
                missing.push(*hook_name);
            }
        } else {
            missing.push(*hook_name);
        }
    }

    if missing.is_empty() {
        Ok(format!("All hooks installed: {}", installed.join(", ")))
    } else if installed.is_empty() {
        Ok("No graphify hooks installed".to_string())
    } else {
        Ok(format!(
            "Installed: {}; Missing: {}",
            installed.join(", "),
            missing.join(", ")
        ))
    }
}

/// Whether at least one managed hook currently carries the graphify block.
///
/// Exposed so callers that sweep every integration (the top-level `uninstall`
/// command) can tell "never installed" from "removed" without having to parse
/// the human-readable output of [`hook_status`].
pub fn hooks_installed(repo_root: &Path) -> bool {
    let hooks_dir = repo_root.join(".git/hooks");
    MANAGED_HOOKS.iter().any(|name| {
        fs::read_to_string(hooks_dir.join(name)).is_ok_and(|c| c.contains(HOOK_MARKER_START))
    })
}

/// Health of a single managed hook, as reported by [`hook_check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookHealth {
    /// Present, executable, and byte-identical to what `install_hooks` writes.
    Current,
    /// No hook file at all.
    Absent,
    /// A hook file exists but carries no readable graphify block.
    NotInstalled,
    /// Our block is there, but the file lost its exec bit so git skips it.
    NotExecutable,
    /// Our block is there but differs from the template `install_hooks` writes.
    Stale,
}

impl HookHealth {
    /// Short, actionable description used in the [`hook_check`] report.
    fn describe(self) -> &'static str {
        match self {
            HookHealth::Current => "ok",
            HookHealth::Absent => "missing (no hook file)",
            HookHealth::NotInstalled => "missing (hook file exists, no graphify block)",
            HookHealth::NotExecutable => "not executable (git will skip it)",
            HookHealth::Stale => "stale (block differs from the installed template)",
        }
    }
}

/// Verify installed hooks are present, executable, and current.
///
/// [`hook_status`] only answers "is the block there?". This goes further and
/// surfaces the drift that silently stops a hook from ever firing: a file that
/// lost its exec bit, or a block left behind by an older version of the
/// template. Drift is *reported*, not raised — the caller prints the report and
/// exits 0, so `hook check` stays usable as a diagnostic in scripts. Only a
/// genuinely missing `.git/hooks` is an error, matching [`hook_status`].
pub fn hook_check(repo_root: &Path) -> Result<String, HookError> {
    let hooks_dir = repo_root.join(".git/hooks");
    if !hooks_dir.exists() {
        return Err(HookError::NotGitRepo(repo_root.display().to_string()));
    }

    let mut lines = Vec::with_capacity(MANAGED_HOOKS.len() + 1);
    let mut problems = 0usize;

    for hook_name in MANAGED_HOOKS {
        let health = check_single_hook(&hooks_dir, hook_name);
        if health != HookHealth::Current {
            problems += 1;
        }
        lines.push(format!("  {hook_name}: {}", health.describe()));
    }

    let header = if problems == 0 {
        "All graphify hooks are current.".to_string()
    } else {
        format!("{problems} hook(s) need attention - run `graphify-rs hook install` to repair.")
    };
    lines.insert(0, header);

    Ok(lines.join("\n"))
}

/// Classify one managed hook file.
///
/// An unreadable file (missing permissions, non-UTF-8 content) is reported as
/// `NotInstalled`: we cannot see our block in it, and the fix is the same.
fn check_single_hook(hooks_dir: &Path, name: &str) -> HookHealth {
    let hook_path = hooks_dir.join(name);
    if !hook_path.exists() {
        return HookHealth::Absent;
    }

    let Ok(content) = fs::read_to_string(&hook_path) else {
        return HookHealth::NotInstalled;
    };
    let Some(block) = extract_marker_block(&content) else {
        return HookHealth::NotInstalled;
    };

    // Reported before staleness on purpose: git ignores a non-executable hook
    // outright, so the contents of the block are moot until that is fixed.
    if !is_executable(&hook_path) {
        return HookHealth::NotExecutable;
    }

    if block == expected_block() {
        HookHealth::Current
    } else {
        HookHealth::Stale
    }
}

/// The graphify block exactly as `install_single_hook` writes it.
///
/// `HOOK_SCRIPT` is stored with surrounding newlines so it appends cleanly, so
/// the comparison target is its trimmed form.
fn expected_block() -> &'static str {
    HOOK_SCRIPT.trim()
}

/// Return the graphify block (both markers included) from hook content.
///
/// The mirror image of [`strip_marker_block`]: that one drops the span, this
/// one hands it back so [`hook_check`] can diff it against the template.
fn extract_marker_block(content: &str) -> Option<&str> {
    let start = content.find(HOOK_MARKER_START)?;
    let end = content[start..].find(HOOK_MARKER_END)? + start + HOOK_MARKER_END.len();
    Some(&content[start..end])
}

/// Whether the file carries an exec bit for anyone.
#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

/// Git for Windows ignores POSIX permissions, so an existing hook always runs.
#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    true
}

/// Output directories a build may have written the graph to.
///
/// The installed hook writes `graphify-rs-out`; `graphify-out` is also accepted
/// so a repo carrying a graph from the Python tool is still guarded.
const GRAPH_DIRS: &[&str] = &["graphify-rs-out", "graphify-out"];

/// Extensions whose contents can actually change the code graph.
///
/// Staging a lockfile, an image, or a binary can never invalidate the graph, so
/// those must not produce a stale warning in somebody's commit path.
const SOURCE_EXTS: &[&str] = &[
    "astro", "c", "cc", "cpp", "cs", "css", "dart", "ex", "exs", "go", "h", "hpp", "java", "js",
    "jsx", "kt", "kts", "lua", "md", "mdx", "php", "pl", "py", "r", "rb", "rs", "rst", "scala",
    "sh", "sql", "svelte", "swift", "ts", "tsx", "vue", "zig",
];

/// Wall-clock ceiling for any git call made from the commit path.
const GIT_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the timeout loop wakes to check on the git child process.
const GIT_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// How many stale files to name before collapsing into "... and N more".
const MAX_LISTED_STALE: usize = 5;

/// Pre-commit guard: report whether the knowledge graph is stale.
///
/// This runs inside somebody's commit, so every uncertain path degrades to an
/// informational message rather than an error: no graph, no git, an unreadable
/// timestamp, or a slow git all return `Ok` and the caller exits 0. It never
/// blocks a commit, and the git calls are bounded by [`GIT_TIMEOUT`] so a
/// wedged repository cannot stall one either.
pub fn hook_guard(repo_root: &Path) -> Result<String, HookError> {
    let Some(graph_path) = find_graph(repo_root) else {
        return Ok("graphify: no knowledge graph found - nothing to check.".to_string());
    };

    let Some(graph_mtime) = modified_at(&graph_path) else {
        return Ok(format!(
            "graphify: cannot read the timestamp of {} - skipping staleness check.",
            display_relative(repo_root, &graph_path)
        ));
    };

    let Some(work_tree) = git_top_level(repo_root) else {
        return Ok("graphify: no git work tree here - skipping staleness check.".to_string());
    };

    let Some(staged) = staged_files(repo_root, &work_tree) else {
        return Ok(
            "graphify: could not read the git index - skipping staleness check.".to_string(),
        );
    };

    let sources: Vec<PathBuf> = staged.into_iter().filter(|path| is_source(path)).collect();
    if sources.is_empty() {
        return Ok("graphify: no staged source files - graph is unaffected.".to_string());
    }

    // A staged file newer than the graph means the graph was built before that
    // edit. Files we cannot stat are treated as fine: better a missed warning
    // than a false alarm on every commit.
    let stale: Vec<&Path> = sources
        .iter()
        .map(PathBuf::as_path)
        .filter(|path| modified_at(path).is_some_and(|mtime| mtime > graph_mtime))
        .collect();

    if stale.is_empty() {
        return Ok(format!(
            "graphify: graph is current for {} staged source file(s).",
            sources.len()
        ));
    }

    let mut report = format!(
        "graphify: graph may be stale - {} of {} staged source file(s) changed after {} was built:",
        stale.len(),
        sources.len(),
        display_relative(repo_root, &graph_path)
    );
    for path in stale.iter().take(MAX_LISTED_STALE) {
        report.push_str(&format!("\n  {}", display_relative(&work_tree, path)));
    }
    if stale.len() > MAX_LISTED_STALE {
        report.push_str(&format!(
            "\n  ... and {} more",
            stale.len() - MAX_LISTED_STALE
        ));
    }
    report.push_str("\nRun `graphify-rs build --code-only` to refresh (commit not blocked).");

    Ok(report)
}

/// Locate `graph.json` under any known output directory.
fn find_graph(project_root: &Path) -> Option<PathBuf> {
    GRAPH_DIRS
        .iter()
        .map(|dir| project_root.join(dir).join("graph.json"))
        .find(|path| path.is_file())
}

/// Modification time of `path`, or `None` when it cannot be read.
fn modified_at(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|meta| meta.modified()).ok()
}

/// Does this path look like a source file the graph would cover?
fn is_source(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .is_some_and(|ext| SOURCE_EXTS.contains(&ext.as_str()))
}

/// Render `path` relative to `base` when possible, to keep hook output short.
fn display_relative(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// Absolute path of the git work tree containing `repo_root`.
///
/// Needed because `git diff --cached` reports paths relative to the work tree,
/// which is not necessarily the directory the command was invoked from.
fn git_top_level(repo_root: &Path) -> Option<PathBuf> {
    let stdout = run_git(repo_root, &["rev-parse", "--show-toplevel"])?;
    let text = String::from_utf8_lossy(&stdout);
    let line = text.lines().next()?.trim();
    if line.is_empty() {
        None
    } else {
        Some(PathBuf::from(line))
    }
}

/// Paths staged for the next commit, resolved against the work tree.
///
/// `--diff-filter=ACMR` drops deletions (nothing left to stat) and `-z` keeps
/// unusual filenames intact instead of git's quoted-and-escaped form.
fn staged_files(repo_root: &Path, work_tree: &Path) -> Option<Vec<PathBuf>> {
    let stdout = run_git(
        repo_root,
        &[
            "diff",
            "--cached",
            "--name-only",
            "--diff-filter=ACMR",
            "-z",
        ],
    )?;
    Some(
        String::from_utf8_lossy(&stdout)
            .split('\0')
            .filter(|entry| !entry.is_empty())
            .map(|entry| work_tree.join(entry))
            .collect(),
    )
}

/// Run a git command under `repo_root` and capture stdout.
///
/// Returns `None` on every failure mode — git missing, non-zero exit, timeout —
/// so callers in the commit path degrade to "skip the check" instead of
/// erroring. The child is killed once [`GIT_TIMEOUT`] elapses, and stdin is
/// closed with prompting disabled, so git can never sit waiting on a human.
fn run_git(repo_root: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("--no-optional-locks")
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let deadline = Instant::now() + GIT_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) => return None,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(GIT_POLL_INTERVAL),
            Err(_) => return None,
        }
    }

    child.wait_with_output().ok().map(|out| out.stdout)
}

/// Strip the graphify marker block from hook content.
///
/// Removes everything between (and including) the start and end markers,
/// plus any surrounding blank lines.
fn strip_marker_block(content: &str) -> String {
    if let Some(start_idx) = content.find(HOOK_MARKER_START) {
        if let Some(end_marker_start) = content[start_idx..].find(HOOK_MARKER_END) {
            let end_idx = start_idx + end_marker_start + HOOK_MARKER_END.len();
            let end_idx = if content[end_idx..].starts_with('\n') {
                end_idx + 1
            } else {
                end_idx
            };
            let start_idx = if start_idx > 0 && content.as_bytes()[start_idx - 1] == b'\n' {
                start_idx - 1
            } else {
                start_idx
            };
            let mut result = String::with_capacity(content.len());
            result.push_str(&content[..start_idx]);
            result.push_str(&content[end_idx..]);
            result
        } else {
            content[..start_idx].to_string()
        }
    } else {
        content.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup_fake_repo(dir: &Path) {
        let hooks_dir = dir.join(".git/hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
    }

    #[test]
    fn test_strip_marker_block_empty() {
        assert_eq!(strip_marker_block("no markers here"), "no markers here");
    }

    #[test]
    fn test_strip_marker_block() {
        let input =
            "#!/bin/sh\n# graphify-rs-hook-start\nsome stuff\n# graphify-rs-hook-end\nother";
        let result = strip_marker_block(input);
        assert_eq!(result, "#!/bin/shother");

        let input2 =
            "#!/bin/sh\n\n# graphify-rs-hook-start\nsome stuff\n# graphify-rs-hook-end\nother";
        let result2 = strip_marker_block(input2);
        assert_eq!(result2, "#!/bin/sh\nother");
    }

    #[test]
    fn test_strip_marker_block_no_end() {
        let input = "#!/bin/sh\n# graphify-rs-hook-start\norphan";
        let result = strip_marker_block(input);
        assert_eq!(result, "#!/bin/sh\n");
    }

    #[test]
    fn test_install_not_git_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let result = install_hooks(tmp.path());
        assert!(matches!(result, Err(HookError::NotGitRepo(_))));
    }

    #[test]
    fn test_install_and_status() {
        let tmp = tempfile::tempdir().unwrap();
        setup_fake_repo(tmp.path());

        let msg = install_hooks(tmp.path()).unwrap();
        assert!(msg.contains("installed"));

        let post_commit = tmp.path().join(".git/hooks/post-commit");
        assert!(post_commit.exists());
        let content = fs::read_to_string(&post_commit).unwrap();
        assert!(content.contains(HOOK_MARKER_START));
        assert!(content.contains(HOOK_MARKER_END));
        assert!(content.starts_with("#!/bin/sh"));

        let status = hook_status(tmp.path()).unwrap();
        assert!(status.contains("All hooks installed"));
    }

    #[test]
    fn test_install_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        setup_fake_repo(tmp.path());

        install_hooks(tmp.path()).unwrap();
        install_hooks(tmp.path()).unwrap();

        let content = fs::read_to_string(tmp.path().join(".git/hooks/post-commit")).unwrap();
        let count = content.matches(HOOK_MARKER_START).count();
        assert_eq!(count, 1, "Hook block should not be duplicated");
    }

    #[test]
    fn test_install_preserves_existing() {
        let tmp = tempfile::tempdir().unwrap();
        setup_fake_repo(tmp.path());

        let hook_path = tmp.path().join(".git/hooks/post-commit");
        fs::write(&hook_path, "#!/bin/sh\necho 'existing'\n").unwrap();

        install_hooks(tmp.path()).unwrap();

        let content = fs::read_to_string(&hook_path).unwrap();
        assert!(content.contains("echo 'existing'"));
        assert!(content.contains(HOOK_MARKER_START));
    }

    #[test]
    fn test_uninstall() {
        let tmp = tempfile::tempdir().unwrap();
        setup_fake_repo(tmp.path());

        install_hooks(tmp.path()).unwrap();
        let msg = uninstall_hooks(tmp.path()).unwrap();
        assert!(msg.contains("removed"));

        let post_commit = tmp.path().join(".git/hooks/post-commit");
        assert!(!post_commit.exists());

        let status = hook_status(tmp.path()).unwrap();
        assert!(status.contains("No graphify hooks installed"));
    }

    #[test]
    fn test_uninstall_preserves_other_content() {
        let tmp = tempfile::tempdir().unwrap();
        setup_fake_repo(tmp.path());

        let hook_path = tmp.path().join(".git/hooks/post-commit");
        fs::write(&hook_path, "#!/bin/sh\necho 'keep me'\n").unwrap();

        install_hooks(tmp.path()).unwrap();
        uninstall_hooks(tmp.path()).unwrap();

        assert!(hook_path.exists());
        let content = fs::read_to_string(&hook_path).unwrap();
        assert!(content.contains("echo 'keep me'"));
        assert!(!content.contains(HOOK_MARKER_START));
    }

    #[test]
    fn test_hook_status_not_git_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let result = hook_status(tmp.path());
        assert!(matches!(result, Err(HookError::NotGitRepo(_))));
    }

    #[test]
    fn test_hooks_installed_flag() {
        let tmp = tempfile::tempdir().unwrap();
        setup_fake_repo(tmp.path());
        assert!(!hooks_installed(tmp.path()));

        install_hooks(tmp.path()).unwrap();
        assert!(hooks_installed(tmp.path()));

        uninstall_hooks(tmp.path()).unwrap();
        assert!(!hooks_installed(tmp.path()));
    }

    #[test]
    fn test_hook_check_not_git_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let result = hook_check(tmp.path());
        assert!(matches!(result, Err(HookError::NotGitRepo(_))));
    }

    #[test]
    fn test_hook_check_missing_hooks() {
        let tmp = tempfile::tempdir().unwrap();
        setup_fake_repo(tmp.path());

        let report = hook_check(tmp.path()).unwrap();
        assert!(report.contains("2 hook(s) need attention"), "{report}");
        assert!(
            report.contains("post-commit: missing (no hook file)"),
            "{report}"
        );
        assert!(
            report.contains("post-checkout: missing (no hook file)"),
            "{report}"
        );
    }

    #[test]
    fn test_hook_check_foreign_hook() {
        let tmp = tempfile::tempdir().unwrap();
        setup_fake_repo(tmp.path());
        fs::write(
            tmp.path().join(".git/hooks/post-commit"),
            "#!/bin/sh\necho 'someone else'\n",
        )
        .unwrap();

        let report = hook_check(tmp.path()).unwrap();
        assert!(
            report.contains("post-commit: missing (hook file exists"),
            "{report}"
        );
    }

    #[test]
    fn test_hook_check_valid() {
        let tmp = tempfile::tempdir().unwrap();
        setup_fake_repo(tmp.path());
        install_hooks(tmp.path()).unwrap();

        let report = hook_check(tmp.path()).unwrap();
        assert!(
            report.contains("All graphify hooks are current"),
            "{report}"
        );
        assert!(report.contains("post-commit: ok"), "{report}");
    }

    #[test]
    fn test_hook_check_stale_block() {
        let tmp = tempfile::tempdir().unwrap();
        setup_fake_repo(tmp.path());
        install_hooks(tmp.path()).unwrap();

        // Simulate a block written by an older template: markers intact, body drifted.
        let hook_path = tmp.path().join(".git/hooks/post-commit");
        let content = fs::read_to_string(&hook_path).unwrap();
        fs::write(&hook_path, content.replace("--code-only", "--legacy-flag")).unwrap();

        let report = hook_check(tmp.path()).unwrap();
        assert!(report.contains("post-commit: stale"), "{report}");
        assert!(report.contains("post-checkout: ok"), "{report}");
        assert!(report.contains("1 hook(s) need attention"), "{report}");
    }

    #[cfg(unix)]
    #[test]
    fn test_hook_check_not_executable() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        setup_fake_repo(tmp.path());
        install_hooks(tmp.path()).unwrap();

        let hook_path = tmp.path().join(".git/hooks/post-commit");
        fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o644)).unwrap();

        let report = hook_check(tmp.path()).unwrap();
        assert!(report.contains("post-commit: not executable"), "{report}");
    }

    /// Write `graph.json` under the default output dir, aged by `age`.
    fn write_graph(root: &Path, age: Duration) {
        let out_dir = root.join("graphify-rs-out");
        fs::create_dir_all(&out_dir).unwrap();
        let graph_path = out_dir.join("graph.json");
        fs::write(&graph_path, "{}").unwrap();
        set_mtime(&graph_path, SystemTime::now() - age);
    }

    /// Pin a file's modification time so staleness tests are deterministic.
    fn set_mtime(path: &Path, when: SystemTime) {
        let file = fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(when).unwrap();
    }

    /// Initialise a throwaway git repo; `false` when git is unavailable.
    fn init_git_repo(dir: &Path) -> bool {
        Command::new("git")
            .args(["init", "-q"])
            .arg(dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    /// Stage one path in a throwaway repo.
    fn git_add(dir: &Path, rel: &str) {
        let staged = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["add", "--", rel])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        assert!(staged, "git add {rel} failed");
    }

    #[test]
    fn test_hook_guard_without_graph() {
        let tmp = tempfile::tempdir().unwrap();
        let msg = hook_guard(tmp.path()).unwrap();
        assert!(msg.contains("no knowledge graph found"), "{msg}");
    }

    #[test]
    fn test_hook_guard_outside_git_work_tree() {
        let tmp = tempfile::tempdir().unwrap();
        write_graph(tmp.path(), Duration::from_secs(0));

        let msg = hook_guard(tmp.path()).unwrap();
        assert!(msg.contains("skipping staleness check"), "{msg}");
    }

    #[test]
    fn test_hook_guard_without_staged_changes() {
        let tmp = tempfile::tempdir().unwrap();
        if !init_git_repo(tmp.path()) {
            return; // git unavailable — nothing to assert
        }
        write_graph(tmp.path(), Duration::from_secs(0));

        let msg = hook_guard(tmp.path()).unwrap();
        assert!(msg.contains("no staged source files"), "{msg}");
    }

    #[test]
    fn test_hook_guard_reports_stale_graph() {
        let tmp = tempfile::tempdir().unwrap();
        if !init_git_repo(tmp.path()) {
            return;
        }
        // Graph built an hour ago; the staged source is brand new.
        write_graph(tmp.path(), Duration::from_secs(3600));
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/lib.rs"), "fn main() {}\n").unwrap();
        git_add(tmp.path(), "src/lib.rs");

        let msg = hook_guard(tmp.path()).unwrap();
        assert!(msg.contains("graph may be stale"), "{msg}");
        assert!(msg.contains("src/lib.rs"), "{msg}");
        assert!(msg.contains("commit not blocked"), "{msg}");
    }

    #[test]
    fn test_hook_guard_current_graph() {
        let tmp = tempfile::tempdir().unwrap();
        if !init_git_repo(tmp.path()) {
            return;
        }
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        let source = tmp.path().join("src/lib.rs");
        fs::write(&source, "fn main() {}\n").unwrap();
        git_add(tmp.path(), "src/lib.rs");
        set_mtime(&source, SystemTime::now() - Duration::from_secs(3600));
        write_graph(tmp.path(), Duration::from_secs(0));

        let msg = hook_guard(tmp.path()).unwrap();
        assert!(
            msg.contains("graph is current for 1 staged source file"),
            "{msg}"
        );
    }

    #[test]
    fn test_hook_guard_ignores_non_source_files() {
        let tmp = tempfile::tempdir().unwrap();
        if !init_git_repo(tmp.path()) {
            return;
        }
        write_graph(tmp.path(), Duration::from_secs(3600));
        fs::write(tmp.path().join("Cargo.lock"), "# lock\n").unwrap();
        git_add(tmp.path(), "Cargo.lock");

        let msg = hook_guard(tmp.path()).unwrap();
        assert!(msg.contains("no staged source files"), "{msg}");
    }

    #[test]
    fn test_is_source_extensions() {
        assert!(is_source(Path::new("src/main.rs")));
        assert!(is_source(Path::new("docs/README.MD")));
        assert!(!is_source(Path::new("Cargo.lock")));
        assert!(!is_source(Path::new("logo.png")));
        assert!(!is_source(Path::new("Makefile")));
    }

    #[test]
    fn test_extract_marker_block_roundtrip() {
        let content = format!("#!/bin/sh\n{HOOK_SCRIPT}");
        assert_eq!(extract_marker_block(&content), Some(expected_block()));
        assert_eq!(extract_marker_block("#!/bin/sh\n"), None);
        // A start marker without an end marker is not a usable block.
        assert_eq!(extract_marker_block("# graphify-rs-hook-start\n"), None);
    }
}
