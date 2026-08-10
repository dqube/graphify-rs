//! Diagnostics: environment health, update checks, and cache inspection.
//!
//! Three read-only commands, each answering a question the user can act on:
//!
//! - `diagnose` — is this project wired up correctly, and is the graph current?
//! - `check-update` — am I running the newest published release?
//! - `cache-check` — how much disk is the extraction cache wasting?
//!
//! None of them mutate the project. A health check that quietly repairs things
//! is a health check you can no longer trust to describe reality, so every
//! finding here is reported with the command that fixes it rather than fixed
//! in place.

use std::cmp::Ordering;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use anyhow::{Result, bail};
use colored::{ColoredString, Colorize};
use graphify_detect::ignore::IgnoreSet;
use rayon::prelude::*;

/// Version of the running binary. Used for both the diagnose banner and as the
/// left-hand side of the `check-update` comparison.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Crate name as published, i.e. the crates.io path segment.
const CRATE_NAME: &str = "graphify-rs";

/// Project URL, sent in the `check-update` User-Agent so crates.io operators
/// can identify the traffic (they reject requests without a real UA).
const REPO_URL: &str = "https://github.com/TtTRz/graphify-rs";

/// Wall-clock ceiling on the crates.io request, in seconds. Deliberately
/// short: `check-update` is a courtesy, never a reason to make the CLI hang.
const UPDATE_TIMEOUT_SECS: u32 = 5;

/// Label column width, so every status line's detail text starts in the same
/// place regardless of ANSI escapes (the padding is applied before coloring).
const LABEL_WIDTH: usize = 15;

// ---------------------------------------------------------------------------
// Report plumbing
// ---------------------------------------------------------------------------

/// Severity of one diagnostic line.
///
/// Only [`Status::Fail`] affects the exit code. Warnings describe things worth
/// fixing that do not stop graphify from working, and scripts that gate on the
/// exit status should not trip over them.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    fn marker(self) -> ColoredString {
        match self {
            Status::Ok => "✓".green(),
            Status::Warn => "!".yellow(),
            Status::Fail => "✗".red(),
        }
    }
}

/// Accumulates the counts that decide the summary line and the exit code.
#[derive(Default)]
struct Report {
    warnings: usize,
    failures: usize,
}

impl Report {
    fn section(&self, title: &str) {
        println!("\n{}", title.bold());
    }

    fn line(&mut self, status: Status, label: &str, detail: impl AsRef<str>) {
        let padded = format!("{label:<LABEL_WIDTH$}");
        println!(
            "  {} {} {}",
            status.marker(),
            padded.dimmed(),
            detail.as_ref()
        );
        match status {
            Status::Ok => {}
            Status::Warn => self.warnings += 1,
            Status::Fail => self.failures += 1,
        }
    }

    /// A continuation line under the previous finding, for remediation hints.
    fn hint(&self, text: impl AsRef<str>) {
        println!("    {} {}", " ".repeat(LABEL_WIDTH), text.as_ref().dimmed());
    }
}

// ---------------------------------------------------------------------------
// Pure formatting / comparison helpers
// ---------------------------------------------------------------------------

/// Format a byte count with binary units (1 KB = 1024 B), the convention the
/// rest of the CLI's size output uses.
fn human_size(bytes: u64) -> String {
    const STEP: f64 = 1024.0;
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let units = ["KB", "MB", "GB", "TB"];
    let mut value = bytes as f64 / STEP;
    let mut unit = units[0];
    for &next in &units[1..] {
        if value < STEP {
            break;
        }
        value /= STEP;
        unit = next;
    }
    format!("{value:.1} {unit}")
}

/// Format an elapsed duration as a coarse phrase ("3 hours ago").
///
/// Coarse on purpose: the question a reader is asking is "is this stale?", not
/// "exactly when did this happen?".
fn human_age(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    let plural = |n: u64| if n == 1 { "" } else { "s" };
    match secs {
        0..=59 => "just now".to_string(),
        60..=3_599 => {
            let n = secs / 60;
            format!("{n} minute{} ago", plural(n))
        }
        3_600..=86_399 => {
            let n = secs / 3_600;
            format!("{n} hour{} ago", plural(n))
        }
        _ => {
            let n = secs / 86_400;
            format!("{n} day{} ago", plural(n))
        }
    }
}

/// `""` or `"s"`, so counted nouns read correctly without a format hack.
fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// The one irregular plural this module needs.
fn entries_word(n: usize) -> &'static str {
    if n == 1 { "entry" } else { "entries" }
}

/// Group digits so five- and six-figure node counts stay readable.
pub(crate) fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Wall-clock age of a timestamp, saturating at zero for clock skew or files
/// with timestamps in the future (common on network mounts).
fn age_of(when: SystemTime) -> Duration {
    SystemTime::now().duration_since(when).unwrap_or_default()
}

/// A parsed `major.minor.patch` version with an optional pre-release tag.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Version {
    parts: [u64; 3],
    /// Pre-release tag, e.g. the `rc.1` of `1.0.0-rc.1`. Per semver a version
    /// carrying one sorts *before* the same version without one.
    pre: Option<String>,
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.parts
            .cmp(&other.parts)
            .then_with(|| match (&self.pre, &other.pre) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => a.cmp(b),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Parse a version string, tolerating a leading `v` and trailing build
/// metadata. Returns `None` for anything that is not `N[.N[.N]]`.
///
/// Hand-rolled rather than pulled from a semver crate: this is the only place
/// in the binary that needs version ordering, and the comparison rules that
/// matter here (numeric fields, pre-release sorts low) fit in a dozen lines.
fn parse_version(text: &str) -> Option<Version> {
    let text = text.trim().trim_start_matches('v');
    // Build metadata never participates in precedence.
    let text = text.split('+').next()?;
    let (core, pre) = match text.split_once('-') {
        Some((core, pre)) if !pre.is_empty() => (core, Some(pre.to_string())),
        _ => (text, None),
    };

    let mut parts = [0u64; 3];
    let mut seen = 0usize;
    for (i, component) in core.split('.').enumerate() {
        if i >= parts.len() {
            return None;
        }
        parts[i] = component.parse().ok()?;
        seen += 1;
    }
    if seen == 0 {
        return None;
    }
    Some(Version { parts, pre })
}

/// Pull the newest published version out of a crates.io `GET /crates/{name}`
/// body.
///
/// Prefers `max_stable_version` so a published pre-release never nags someone
/// running a stable build, and falls back through the other fields because
/// brand-new crates with no stable release leave it empty.
fn latest_from_crates_io_json(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let krate = value.get("crate")?;
    for field in ["max_stable_version", "max_version", "newest_version"] {
        if let Some(text) = krate.get(field).and_then(serde_json::Value::as_str)
            && !text.is_empty()
        {
            return Some(text.to_string());
        }
    }
    None
}

/// Source files modified after the graph was built.
///
/// Kept pure over a slice so the staleness rule can be tested without touching
/// the filesystem. Returns how many files are newer and which one is newest.
fn newer_than(built_at: SystemTime, sources: &[SourceFile]) -> (usize, Option<&Path>) {
    let mut count = 0usize;
    let mut newest: Option<&SourceFile> = None;
    for file in sources {
        if file.modified <= built_at {
            continue;
        }
        count += 1;
        if newest.is_none_or(|current| file.modified > current.modified) {
            newest = Some(file);
        }
    }
    (count, newest.map(|f| f.path.as_path()))
}

/// Split cache entries by whether their content hash still matches a file that
/// exists today. Returns `(fresh_count, fresh_bytes, stale_count, stale_bytes)`.
fn split_by_liveness(
    entries: &[CacheEntry],
    live_hashes: &HashSet<&str>,
) -> (usize, u64, usize, u64) {
    let mut fresh = (0usize, 0u64);
    let mut stale = (0usize, 0u64);
    for entry in entries {
        let bucket = if live_hashes.contains(entry.key.as_str()) {
            &mut fresh
        } else {
            &mut stale
        };
        bucket.0 += 1;
        bucket.1 += entry.size;
    }
    (fresh.0, fresh.1, stale.0, stale.1)
}

// ---------------------------------------------------------------------------
// Source discovery
// ---------------------------------------------------------------------------

/// One file the graph would be built from.
struct SourceFile {
    path: PathBuf,
    modified: SystemTime,
}

/// Directory names never worth walking. Mirrors `graphify_detect`'s rule so
/// diagnostics count the same corpus a build would.
fn is_noise_dir(name: &str) -> bool {
    graphify_detect::constants::SKIP_DIRS.contains(&name)
        || name.ends_with("_venv")
        || name.ends_with("_env")
        || name.ends_with(".egg-info")
}

/// Walk `root` and collect the files graphify would extract from, skipping
/// `exclude` (normally the output directory).
///
/// This reimplements `graphify_detect`'s filtering but stops at `stat`:
/// `detect()` reads every file to count words, which is far more work than a
/// health check justifies when all we need is a modification time.
fn collect_sources(root: &Path, exclude: Option<&Path>) -> Vec<SourceFile> {
    let patterns = graphify_detect::load_graphifyignore(root);
    let ignore = IgnoreSet::new(&patterns);
    let exclude = exclude.and_then(|p| p.canonicalize().ok());

    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if ignore.is_ignored(&path, root) {
                continue;
            }
            // `file_type()` does not follow symlinks, matching the build walk.
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            if is_dir {
                let skip = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_none_or(|name| is_noise_dir(name) || name.starts_with('.'));
                if skip {
                    continue;
                }
                if let Some(excluded) = &exclude
                    && path.canonicalize().is_ok_and(|p| &p == excluded)
                {
                    continue;
                }
                stack.push(path);
            } else {
                if graphify_detect::classify_file(&path).is_none()
                    || graphify_detect::is_sensitive(&path)
                {
                    continue;
                }
                let modified = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                found.push(SourceFile { path, modified });
            }
        }
    }
    found
}

// ---------------------------------------------------------------------------
// diagnose
// ---------------------------------------------------------------------------

/// Report environment and graph health.
///
/// Exits non-zero only when something is genuinely broken (an unreadable
/// graph, a non-writable output directory, a provider name that no build can
/// resolve). Everything else — a missing graph, a stale graph, absent hooks —
/// is a warning with the command that resolves it.
pub fn cmd_diagnose(graph_path: &str) -> Result<()> {
    let graph_file = Path::new(graph_path);
    let output_dir = match graph_file.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    println!(
        "\n{} {}",
        "graphify-rs".cyan().bold(),
        format!("v{VERSION} diagnostics").dimmed()
    );

    let mut report = Report::default();
    diagnose_environment(&mut report, &root);

    // Scanned once and shared: both the freshness check and the corpus line
    // need it, and walking a large repo twice is the slow part of `diagnose`.
    let sources = collect_sources(&root, Some(&output_dir));
    diagnose_graph(&mut report, graph_file, &sources, &root);
    diagnose_output(&mut report, &output_dir);

    println!();
    match (report.failures, report.warnings) {
        (0, 0) => println!("  {}", "All checks passed.".green().bold()),
        (0, w) => println!(
            "  {}",
            format!("{w} warning{} — nothing broken.", plural(w))
                .yellow()
                .bold()
        ),
        (f, w) => println!(
            "  {}",
            format!("{f} problem{}, {w} warning{}.", plural(f), plural(w))
                .red()
                .bold()
        ),
    }
    println!();

    if report.failures > 0 {
        bail!(
            "diagnose found {} problem{} — see the report above",
            report.failures,
            plural(report.failures)
        );
    }
    Ok(())
}

fn diagnose_environment(report: &mut Report, root: &Path) {
    report.section("Environment");
    report.line(
        Status::Ok,
        "graphify-rs",
        format!(
            "v{VERSION} ({} {})",
            std::env::consts::ARCH,
            std::env::consts::OS
        ),
    );
    report.line(Status::Ok, "project root", root.display().to_string());

    match find_git_root(root) {
        Some(git_root) => {
            let where_ = match current_branch(&git_root) {
                Some(branch) => format!("{} (on {branch})", git_root.display()),
                None => git_root.display().to_string(),
            };
            report.line(Status::Ok, "git repository", where_);
            match graphify_hooks::hook_status(&git_root) {
                Ok(status) if status.starts_with("All hooks") => {
                    report.line(Status::Ok, "git hooks", status);
                }
                Ok(status) => {
                    report.line(Status::Warn, "git hooks", status);
                    report
                        .hint("run `graphify-rs hook install` to keep the graph current on commit");
                }
                Err(err) => report.line(Status::Warn, "git hooks", format!("unreadable: {err}")),
            }
        }
        None => {
            report.line(Status::Warn, "git repository", "none found");
            report.hint("`diff`, `affected`, and the commit hooks all need a git repository");
        }
    }

    let agents = detect_agent_integrations(root);
    if agents.is_empty() {
        report.line(Status::Warn, "agent hooks", "no coding-agent integration");
        report.hint("run `graphify-rs claude install` (or codex/opencode/…) to wire one up");
    } else {
        report.line(Status::Ok, "agent hooks", agents.join(", "));
    }

    let config_path = root.join("graphify-rs.toml");
    if config_path.exists() {
        report.line(Status::Ok, "config", config_path.display().to_string());
    } else {
        report.line(
            Status::Warn,
            "config",
            "no graphify-rs.toml — using defaults",
        );
        report.hint("run `graphify-rs init` to write one");
    }
    diagnose_llm(report, crate::config::load_config(root).llm.as_ref());
}

/// Report the configured LLM provider and whether a credential is present.
///
/// Prints only *whether* a key exists and where it came from. The value is
/// never read into the output — diagnose output routinely ends up pasted into
/// bug reports.
fn diagnose_llm(report: &mut Report, llm: Option<&crate::config::LLMConfig>) {
    let Some(llm) = llm else {
        report.line(Status::Warn, "LLM provider", "not configured");
        report.hint("AST-only builds still work; semantic extraction will be skipped");
        return;
    };

    let provider = llm.provider.as_deref().unwrap_or_default();
    let model = llm.model.as_deref().unwrap_or("(no model set)");

    // `None` means "this provider needs a key and none was found".
    let credential: Option<&str> = match provider {
        "anthropic" => credential_source(llm.anthropic_api_key.as_deref(), "ANTHROPIC_API_KEY"),
        "openai" => credential_source(llm.openai_api_key.as_deref(), "OPENAI_API_KEY"),
        "openai_compatible" => credential_source(llm.openai_compatible_api_key.as_deref(), ""),
        "ollama" => Some("not required (local)"),
        "" => {
            report.line(
                Status::Warn,
                "LLM provider",
                "[llm] section has no provider",
            );
            report.hint(
                "set `provider = \"anthropic\" | \"openai\" | \"ollama\" | \"openai_compatible\"`",
            );
            return;
        }
        other => {
            report.line(
                Status::Fail,
                "LLM provider",
                format!("unknown provider '{other}' — every semantic build will fail"),
            );
            report.hint("supported: anthropic, openai, ollama, openai_compatible");
            return;
        }
    };

    match credential {
        Some(source) => report.line(
            Status::Ok,
            "LLM provider",
            format!("{provider} · {model} · key {source}"),
        ),
        None => {
            report.line(
                Status::Warn,
                "LLM provider",
                format!("{provider} · {model} · no API key found"),
            );
            let hint = match provider {
                "anthropic" => {
                    "set ANTHROPIC_API_KEY, add [llm].anthropic_api_key, or sign in to Claude Code"
                }
                "openai" => "set OPENAI_API_KEY or add [llm].openai_api_key",
                _ => "add the provider's api_key to the [llm] section",
            };
            report.hint(hint);
        }
    }
}

/// Where a credential came from, without revealing any of it.
fn credential_source(from_config: Option<&str>, env_var: &str) -> Option<&'static str> {
    if from_config.is_some_and(|k| !k.is_empty()) {
        return Some("from graphify-rs.toml");
    }
    if !env_var.is_empty() && std::env::var_os(env_var).is_some_and(|v| !v.is_empty()) {
        return Some("from environment");
    }
    None
}

fn diagnose_graph(report: &mut Report, graph_file: &Path, sources: &[SourceFile], root: &Path) {
    report.section("Graph");

    let Ok(meta) = fs::metadata(graph_file) else {
        report.line(
            Status::Warn,
            "graph.json",
            format!("not found at {}", graph_file.display()),
        );
        report.hint("run `graphify-rs build` to create it");
        report.line(
            Status::Ok,
            "corpus",
            format!(
                "{} extractable file{} under {}",
                thousands(sources.len()),
                plural(sources.len()),
                root.display()
            ),
        );
        return;
    };

    let built_at = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    report.line(
        Status::Ok,
        "graph.json",
        format!(
            "{} · {} · built {}",
            graph_file.display(),
            human_size(meta.len()),
            human_age(age_of(built_at))
        ),
    );

    match graphify_serve::load_graph(graph_file) {
        Ok(graph) => {
            let labeled = graph
                .communities
                .iter()
                .filter(|c| c.label.is_some())
                .count();
            report.line(
                Status::Ok,
                "contents",
                format!(
                    "{} nodes · {} edges · {} communities ({labeled} labeled)",
                    thousands(graph.node_count()),
                    thousands(graph.edge_count()),
                    graph.communities.len()
                ),
            );
            if graph.node_count() == 0 {
                report.line(Status::Warn, "coverage", "the graph is empty");
                report.hint("check .graphifyignore — every file may be filtered out");
            } else {
                let isolated = graph
                    .nodes()
                    .into_iter()
                    .filter(|n| graph.degree(&n.id) == 0)
                    .count();
                if isolated > 0 {
                    let pct = isolated * 100 / graph.node_count();
                    report.line(
                        Status::Warn,
                        "coverage",
                        format!(
                            "{} node{} have no edges ({pct}%)",
                            thousands(isolated),
                            plural(isolated)
                        ),
                    );
                    report.hint(
                        "usually means extraction ran without a resolver for those languages",
                    );
                }
            }
            if graph.communities.is_empty() && graph.node_count() > 0 {
                report.line(Status::Warn, "communities", "none — clustering has not run");
                report.hint(
                    "re-run `graphify-rs build` (clustering is what GRAPH_REPORT.md is built from)",
                );
            }
        }
        Err(err) => {
            report.line(Status::Fail, "contents", format!("cannot parse: {err}"));
            report.hint("the file is corrupt or truncated — re-run `graphify-rs build`");
        }
    }

    let (newer, newest) = newer_than(built_at, sources);
    if newer == 0 {
        report.line(
            Status::Ok,
            "freshness",
            format!(
                "current — none of {} source file{} changed since the build",
                thousands(sources.len()),
                plural(sources.len())
            ),
        );
    } else {
        let newest = newest
            .and_then(|p| p.strip_prefix(root).ok().or(Some(p)))
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        report.line(
            Status::Warn,
            "freshness",
            format!(
                "stale — {} source file{} changed since the build (newest: {newest})",
                thousands(newer),
                plural(newer)
            ),
        );
        report.hint("run `graphify-rs build --no-llm` for a fast AST-only refresh");
    }
}

fn diagnose_output(report: &mut Report, output_dir: &Path) {
    report.section("Output");

    if !output_dir.exists() {
        report.line(
            Status::Warn,
            "output dir",
            format!("{} does not exist yet", output_dir.display()),
        );
        return;
    }

    if is_writable(output_dir) {
        report.line(Status::Ok, "output dir", output_dir.display().to_string());
    } else {
        report.line(
            Status::Fail,
            "output dir",
            format!(
                "{} is not writable — no build can complete",
                output_dir.display()
            ),
        );
    }

    let artifacts = [
        ("GRAPH_REPORT.md", "GRAPH_REPORT.md"),
        ("graph.html", "graph.html"),
        ("wiki/", "wiki/index.md"),
    ];
    let present: Vec<&str> = artifacts
        .iter()
        .filter(|(_, probe)| output_dir.join(probe).exists())
        .map(|(name, _)| *name)
        .collect();
    if present.is_empty() {
        report.line(
            Status::Warn,
            "artifacts",
            "none — only graph.json was written",
        );
    } else {
        report.line(Status::Ok, "artifacts", present.join(", "));
    }

    let cache_dir = output_dir.join("cache");
    match scan_cache(&cache_dir) {
        Ok(stats) if stats.entries.is_empty() && stats.temp_files == 0 => {
            report.line(Status::Ok, "cache", "empty");
        }
        Ok(stats) => {
            report.line(
                Status::Ok,
                "cache",
                format!(
                    "{} {} · {} on disk",
                    thousands(stats.entries.len()),
                    entries_word(stats.entries.len()),
                    human_size(stats.total_bytes())
                ),
            );
            report.hint("run `graphify-rs cache-check` to see how much of it is stale");
        }
        Err(_) => report.line(Status::Ok, "cache", "not created yet"),
    }
}

/// Nearest ancestor of `start` (inclusive) containing a `.git` entry.
///
/// Accepts a `.git` *file* as well as a directory so worktrees and submodules
/// are recognised.
fn find_git_root(start: &Path) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(dir) = current {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

/// Current branch name, read straight from `.git/HEAD` so `diagnose` never
/// needs the `git` binary on PATH.
///
/// Returns `None` for a detached HEAD (a raw SHA, not a ref) and for worktrees
/// or submodules where `.git` is a pointer file rather than a directory.
fn current_branch(git_root: &Path) -> Option<String> {
    let head = fs::read_to_string(git_root.join(".git/HEAD")).ok()?;
    let reference = head.trim().strip_prefix("ref: ")?;
    // Branch names may contain slashes, so strip the prefix rather than
    // taking the last segment.
    Some(
        reference
            .strip_prefix("refs/heads/")
            .unwrap_or(reference)
            .to_string(),
    )
}

/// Coding-agent integrations installed in this project.
fn detect_agent_integrations(root: &Path) -> Vec<&'static str> {
    // (label, file, marker the installer writes into it)
    let probes: &[(&'static str, &str, &str)] = &[
        ("claude", ".claude/settings.json", "graphify-rs"),
        ("claude", "CLAUDE.md", "## graphify-rs"),
        ("codebuddy", ".codebuddy/settings.json", "graphify-rs"),
        ("codex", ".codex/hooks.json", "graphify-rs"),
        ("opencode", ".opencode/plugin/graphify-rs.js", "graphify-rs"),
        ("agents", "AGENTS.md", "## graphify-rs"),
    ];
    let mut found: Vec<&'static str> = Vec::new();
    for &(label, file, marker) in probes {
        if found.contains(&label) {
            continue;
        }
        if fs::read_to_string(root.join(file)).is_ok_and(|c| c.contains(marker)) {
            found.push(label);
        }
    }
    found
}

/// Probe whether `dir` accepts writes, cleaning up after itself.
///
/// Tested by writing rather than by inspecting permission bits: read-only
/// mounts, SELinux, and Windows ACLs all make the bits lie.
fn is_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".graphify-write-probe-{}", std::process::id()));
    match fs::write(&probe, b"") {
        Ok(()) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// check-update
// ---------------------------------------------------------------------------

/// Check whether a newer graphify-rs release is available.
///
/// Best-effort by design: the network call is bounded by a short timeout and
/// every failure path degrades to an explanatory line and `Ok(())`. Being
/// offline is not an error, and this command must never be the reason a
/// script or a shell prompt blocks.
pub fn cmd_check_update() -> Result<()> {
    println!(
        "\n{} {}",
        "graphify-rs".cyan().bold(),
        format!("v{VERSION}").dimmed()
    );

    if std::env::var_os("GRAPHIFY_OFFLINE").is_some() {
        println!(
            "  {} {}",
            Status::Ok.marker(),
            "GRAPHIFY_OFFLINE is set — skipping the crates.io check".dimmed()
        );
        println!();
        return Ok(());
    }

    let url = format!("https://crates.io/api/v1/crates/{CRATE_NAME}");
    let body = match http_get(&url) {
        Ok(body) => body,
        Err(why) => {
            println!("  {} update check skipped: {why}", Status::Warn.marker());
            println!(
                "    {}",
                format!("compare manually at https://crates.io/crates/{CRATE_NAME}").dimmed()
            );
            println!();
            return Ok(());
        }
    };

    let Some(latest_text) = latest_from_crates_io_json(&body) else {
        println!(
            "  {} crates.io returned no version for {CRATE_NAME}",
            Status::Warn.marker()
        );
        println!();
        return Ok(());
    };

    match (parse_version(VERSION), parse_version(&latest_text)) {
        (Some(current), Some(latest)) if latest > current => {
            println!(
                "  {} {} {}",
                Status::Warn.marker(),
                "update available".bold(),
                format!("{latest_text} (you have {VERSION})").dimmed()
            );
            println!("    {}", "cargo install graphify-rs --force".cyan());
        }
        (Some(current), Some(latest)) if latest < current => {
            println!(
                "  {} running ahead of crates.io {}",
                Status::Ok.marker(),
                format!("(local {VERSION}, published {latest_text})").dimmed()
            );
        }
        (Some(_), Some(_)) => {
            println!(
                "  {} up to date {}",
                Status::Ok.marker(),
                format!("({VERSION} is the latest release)").dimmed()
            );
        }
        _ => {
            // One of the two strings is not a version we can order. Report
            // both rather than guessing which way the comparison would go.
            println!(
                "  {} could not compare versions {}",
                Status::Warn.marker(),
                format!("(local {VERSION}, crates.io {latest_text})").dimmed()
            );
        }
    }
    println!();
    Ok(())
}

/// Best-effort HTTP GET, returning the response body or a human explanation.
///
/// Shells out to `curl` deliberately. The CLI binary does not link an HTTP
/// client of its own, and a once-in-a-while version ping does not justify
/// pulling a TLS stack into it. `--max-time` bounds the child process, so an
/// unreachable or black-holed network cannot wedge the command.
fn http_get(url: &str) -> std::result::Result<String, String> {
    let user_agent = format!("graphify-rs/{VERSION} (+{REPO_URL})");
    let max_time = UPDATE_TIMEOUT_SECS.to_string();
    let output = Command::new("curl")
        .args([
            "-sfL",
            "--connect-timeout",
            "3",
            "--max-time",
            max_time.as_str(),
            "-H",
            "Accept: application/json",
            "-A",
            user_agent.as_str(),
            url,
        ])
        .output()
        .map_err(|err| format!("curl is unavailable ({err})"))?;

    if !output.status.success() {
        return Err(match output.status.code() {
            Some(6) => "could not resolve crates.io (offline?)".to_string(),
            Some(7) => "could not connect to crates.io (offline?)".to_string(),
            Some(22) => "crates.io returned an HTTP error".to_string(),
            Some(28) => format!("timed out after {UPDATE_TIMEOUT_SECS}s"),
            Some(code) => format!("curl exited with status {code}"),
            None => "curl was terminated by a signal".to_string(),
        });
    }

    String::from_utf8(output.stdout).map_err(|_| "response was not valid UTF-8".to_string())
}

// ---------------------------------------------------------------------------
// cache-check
// ---------------------------------------------------------------------------

/// One file in the cache directory.
struct CacheEntry {
    /// The content hash the entry is keyed by (the filename stem).
    key: String,
    size: u64,
}

/// Tally of a cache directory's contents.
#[derive(Default)]
struct CacheStats {
    entries: Vec<CacheEntry>,
    /// `.tmp` files left behind when a write was interrupted. Always garbage.
    temp_files: usize,
    temp_bytes: u64,
    other_files: usize,
    other_bytes: u64,
}

impl CacheStats {
    fn total_bytes(&self) -> u64 {
        self.entries.iter().map(|e| e.size).sum::<u64>() + self.temp_bytes + self.other_bytes
    }
}

/// Read a cache directory into a [`CacheStats`].
///
/// The on-disk layout is flat: `<cache_dir>/<sha256-of-file-content>.json`,
/// written atomically via a sibling `.tmp`. Anything else in there is either
/// a crashed write or not ours.
fn scan_cache(cache_dir: &Path) -> std::io::Result<CacheStats> {
    let mut stats = CacheStats::default();
    for entry in fs::read_dir(cache_dir)? {
        let Ok(entry) = entry else { continue };
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let size = meta.len();
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(key) = name.strip_suffix(".json") {
            stats.entries.push(CacheEntry {
                key: key.to_string(),
                size,
            });
        } else if name.ends_with(".tmp") {
            stats.temp_files += 1;
            stats.temp_bytes += size;
        } else {
            stats.other_files += 1;
            stats.other_bytes += size;
        }
    }
    Ok(stats)
}

/// Report extraction cache size, hit rate, and staleness.
///
/// Cache keys are content hashes, so "stale" means the entry's hash no longer
/// matches any file that exists today — the source was edited or deleted and
/// nothing will ever read that entry again. Those bytes are pure waste, so the
/// headline number is how much removing them would reclaim.
pub fn cmd_cache_check(output_dir: &str) -> Result<()> {
    let output_dir = Path::new(output_dir);
    let cache_dir = output_dir.join("cache");

    println!(
        "\n{} {}",
        "graphify-rs".cyan().bold(),
        "cache-check".dimmed()
    );
    println!("  {} {}", "location".dimmed(), cache_dir.display());

    let stats = match scan_cache(&cache_dir) {
        Ok(stats) => stats,
        Err(_) => {
            println!(
                "\n  {} no cache directory yet — nothing to reclaim",
                Status::Ok.marker()
            );
            println!("    {}", "it is created on the first build".dimmed());
            println!();
            return Ok(());
        }
    };

    if stats.entries.is_empty() && stats.temp_files == 0 && stats.other_files == 0 {
        println!("\n  {} the cache is empty", Status::Ok.marker());
        println!();
        return Ok(());
    }

    println!(
        "\n  {} {} {} · {} in the directory",
        Status::Ok.marker(),
        thousands(stats.entries.len()),
        entries_word(stats.entries.len()),
        human_size(stats.total_bytes())
    );

    // Staleness needs the current content hash of every source file, which is
    // the expensive half of this command — parallelised because it is pure I/O
    // plus SHA256 over independent files.
    let root = output_dir.parent().unwrap_or(Path::new("."));
    let sources = collect_sources(root, Some(output_dir));
    let hashes: Vec<String> = sources
        .par_iter()
        .filter_map(|file| graphify_cache::file_hash(&file.path))
        .collect();

    if sources.is_empty() {
        println!(
            "  {} no source files found under {} — staleness cannot be judged",
            Status::Warn.marker(),
            root.display()
        );
        println!(
            "    {}",
            "run cache-check from the project root, or pass --output".dimmed()
        );
        println!();
        return Ok(());
    }

    let live: HashSet<&str> = hashes.iter().map(String::as_str).collect();
    let (fresh_count, _fresh_bytes, stale_count, stale_bytes) =
        split_by_liveness(&stats.entries, &live);

    let cached_keys: HashSet<&str> = stats.entries.iter().map(|e| e.key.as_str()).collect();
    let hits = hashes
        .iter()
        .filter(|h| cached_keys.contains(h.as_str()))
        .count();
    let hit_rate = hits * 100 / sources.len();

    println!(
        "  {} {} {} still match a current source file",
        Status::Ok.marker(),
        thousands(fresh_count),
        entries_word(fresh_count)
    );
    println!(
        "  {} {} of {} source file{} are cached ({hit_rate}% hit rate)",
        if hit_rate >= 50 {
            Status::Ok.marker()
        } else {
            Status::Warn.marker()
        },
        thousands(hits),
        thousands(sources.len()),
        plural(sources.len())
    );

    let mut reclaimable = stale_bytes;
    if stale_count > 0 {
        println!(
            "  {} {} stale {} ({}) — the source changed or was deleted",
            Status::Warn.marker(),
            thousands(stale_count),
            entries_word(stale_count),
            human_size(stale_bytes)
        );
    }
    if stats.temp_files > 0 {
        reclaimable += stats.temp_bytes;
        println!(
            "  {} {} leftover .tmp file{} ({}) from interrupted writes",
            Status::Warn.marker(),
            thousands(stats.temp_files),
            plural(stats.temp_files),
            human_size(stats.temp_bytes)
        );
    }
    if stats.other_files > 0 {
        println!(
            "  {} {} unrecognised file{} ({}) — not written by graphify",
            Status::Warn.marker(),
            thousands(stats.other_files),
            plural(stats.other_files),
            human_size(stats.other_bytes)
        );
    }

    println!();
    if reclaimable == 0 {
        println!(
            "  {}",
            "Nothing to reclaim — the cache is all live.".green()
        );
    } else {
        println!(
            "  {}",
            format!("Reclaimable: {}", human_size(reclaimable))
                .yellow()
                .bold()
        );
        println!(
            "    {}",
            format!(
                "delete {} to reclaim it; the next build re-caches only what it needs",
                cache_dir.display()
            )
            .dimmed()
        );
    }
    println!();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::TempDir;

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn source(path: &str, secs: u64) -> SourceFile {
        SourceFile {
            path: PathBuf::from(path),
            modified: at(secs),
        }
    }

    // -- human_size ---------------------------------------------------------

    #[test]
    fn human_size_uses_bytes_below_one_kb() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1), "1 B");
        assert_eq!(human_size(1023), "1023 B");
    }

    #[test]
    fn human_size_steps_through_binary_units() {
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(1024 * 1024), "1.0 MB");
        assert_eq!(human_size(3 * 1024 * 1024 + 512 * 1024), "3.5 MB");
        assert_eq!(human_size(1024 * 1024 * 1024), "1.0 GB");
        assert_eq!(human_size(1024u64.pow(4)), "1.0 TB");
    }

    // -- human_age ----------------------------------------------------------

    #[test]
    fn human_age_is_coarse_and_pluralised() {
        assert_eq!(human_age(Duration::from_secs(0)), "just now");
        assert_eq!(human_age(Duration::from_secs(59)), "just now");
        assert_eq!(human_age(Duration::from_secs(60)), "1 minute ago");
        assert_eq!(human_age(Duration::from_secs(3 * 60)), "3 minutes ago");
        assert_eq!(human_age(Duration::from_secs(3600)), "1 hour ago");
        assert_eq!(human_age(Duration::from_secs(5 * 3600)), "5 hours ago");
        assert_eq!(human_age(Duration::from_secs(86_400)), "1 day ago");
        assert_eq!(human_age(Duration::from_secs(10 * 86_400)), "10 days ago");
    }

    #[test]
    fn age_of_future_timestamp_saturates_at_zero() {
        let future = SystemTime::now() + Duration::from_secs(3600);
        assert_eq!(age_of(future), Duration::ZERO);
    }

    // -- thousands ----------------------------------------------------------

    #[test]
    fn plurals_agree_with_their_counts() {
        assert_eq!(plural(0), "s");
        assert_eq!(plural(1), "");
        assert_eq!(plural(2), "s");
        assert_eq!(entries_word(0), "entries");
        assert_eq!(entries_word(1), "entry");
        assert_eq!(entries_word(2), "entries");
    }

    #[test]
    fn thousands_groups_digits() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(12_431), "12,431");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }

    // -- version comparison -------------------------------------------------

    #[test]
    fn parse_version_accepts_common_forms() {
        assert_eq!(parse_version("0.8.2").unwrap().parts, [0, 8, 2]);
        assert_eq!(parse_version("v1.2.3").unwrap().parts, [1, 2, 3]);
        assert_eq!(parse_version(" 1.2.3 ").unwrap().parts, [1, 2, 3]);
        assert_eq!(parse_version("2").unwrap().parts, [2, 0, 0]);
        assert_eq!(parse_version("2.1").unwrap().parts, [2, 1, 0]);
        assert_eq!(parse_version("1.0.0+build.5").unwrap().pre, None);
        assert_eq!(
            parse_version("1.0.0-rc.1").unwrap().pre.as_deref(),
            Some("rc.1")
        );
    }

    #[test]
    fn parse_version_rejects_junk() {
        assert!(parse_version("").is_none());
        assert!(parse_version("not-a-version").is_none());
        assert!(parse_version("1.2.3.4").is_none());
        assert!(parse_version("1.x.3").is_none());
    }

    #[test]
    fn version_ordering_is_numeric_not_lexicographic() {
        let cmp = |a: &str, b: &str| parse_version(a).unwrap().cmp(&parse_version(b).unwrap());
        assert_eq!(cmp("0.8.10", "0.8.9"), Ordering::Greater);
        assert_eq!(cmp("1.0.0", "0.9.9"), Ordering::Greater);
        assert_eq!(cmp("0.8.2", "0.8.2"), Ordering::Equal);
        assert_eq!(cmp("0.8.2", "0.9.0"), Ordering::Less);
    }

    #[test]
    fn prerelease_sorts_before_its_release() {
        let cmp = |a: &str, b: &str| parse_version(a).unwrap().cmp(&parse_version(b).unwrap());
        assert_eq!(cmp("1.0.0-rc.1", "1.0.0"), Ordering::Less);
        assert_eq!(cmp("1.0.0", "1.0.0-rc.1"), Ordering::Greater);
        assert_eq!(cmp("1.0.0-rc.1", "1.0.0-rc.2"), Ordering::Less);
        // A pre-release still beats the previous stable release.
        assert_eq!(cmp("1.0.0-rc.1", "0.9.9"), Ordering::Greater);
    }

    // -- crates.io payload parsing (no network) -----------------------------

    #[test]
    fn latest_prefers_max_stable_version() {
        let body = r#"{"crate":{"id":"graphify-rs","max_version":"0.9.0-rc.1",
            "newest_version":"0.9.0-rc.1","max_stable_version":"0.8.2"}}"#;
        assert_eq!(latest_from_crates_io_json(body).as_deref(), Some("0.8.2"));
    }

    #[test]
    fn latest_falls_back_when_no_stable_release_exists() {
        let body = r#"{"crate":{"max_stable_version":"","max_version":"0.1.0-alpha.1"}}"#;
        assert_eq!(
            latest_from_crates_io_json(body).as_deref(),
            Some("0.1.0-alpha.1")
        );
    }

    #[test]
    fn latest_returns_none_for_errors_and_junk() {
        assert!(latest_from_crates_io_json(r#"{"errors":[{"detail":"Not Found"}]}"#).is_none());
        assert!(latest_from_crates_io_json("not json").is_none());
        assert!(latest_from_crates_io_json(r#"{"crate":{}}"#).is_none());
    }

    // -- staleness ----------------------------------------------------------

    #[test]
    fn nothing_is_newer_than_a_fresh_graph() {
        let sources = vec![source("a.rs", 100), source("b.rs", 200)];
        let (count, newest) = newer_than(at(300), &sources);
        assert_eq!(count, 0);
        assert!(newest.is_none());
    }

    #[test]
    fn files_modified_after_the_build_are_reported() {
        let sources = vec![
            source("old.rs", 100),
            source("edited.rs", 400),
            source("newest.rs", 500),
        ];
        let (count, newest) = newer_than(at(300), &sources);
        assert_eq!(count, 2);
        assert_eq!(newest, Some(Path::new("newest.rs")));
    }

    #[test]
    fn a_file_saved_at_the_build_instant_is_not_stale() {
        // The build reads the file before writing the graph, so equal
        // timestamps mean the content is already represented.
        let sources = vec![source("same.rs", 300)];
        assert_eq!(newer_than(at(300), &sources).0, 0);
    }

    #[test]
    fn empty_corpus_is_never_stale() {
        assert_eq!(newer_than(at(1), &[]), (0, None));
    }

    // -- cache liveness -----------------------------------------------------

    fn entry(key: &str, size: u64) -> CacheEntry {
        CacheEntry {
            key: key.to_string(),
            size,
        }
    }

    #[test]
    fn liveness_split_separates_matched_and_orphaned_entries() {
        let entries = vec![entry("aaa", 100), entry("bbb", 250), entry("ccc", 400)];
        let live: HashSet<&str> = ["aaa", "ccc"].into_iter().collect();
        let (fresh_n, fresh_b, stale_n, stale_b) = split_by_liveness(&entries, &live);
        assert_eq!((fresh_n, fresh_b), (2, 500));
        assert_eq!((stale_n, stale_b), (1, 250));
    }

    #[test]
    fn everything_is_stale_when_no_source_survives() {
        let entries = vec![entry("aaa", 10), entry("bbb", 20)];
        let (fresh_n, _, stale_n, stale_b) = split_by_liveness(&entries, &HashSet::new());
        assert_eq!(fresh_n, 0);
        assert_eq!((stale_n, stale_b), (2, 30));
    }

    #[test]
    fn scan_cache_classifies_entries_temp_and_foreign_files() {
        let dir = TempDir::new().unwrap();
        let cache = dir.path().join("cache");
        fs::create_dir_all(&cache).unwrap();
        fs::write(cache.join("deadbeef.json"), "12345").unwrap();
        fs::write(cache.join("cafebabe.json"), "1234567890").unwrap();
        fs::write(cache.join("halfwritten.tmp"), "xx").unwrap();
        fs::write(cache.join("README"), "x").unwrap();

        let stats = scan_cache(&cache).unwrap();
        assert_eq!(stats.entries.len(), 2);
        assert_eq!(stats.temp_files, 1);
        assert_eq!(stats.other_files, 1);
        assert_eq!(stats.total_bytes(), 5 + 10 + 2 + 1);

        let mut keys: Vec<&str> = stats.entries.iter().map(|e| e.key.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["cafebabe", "deadbeef"]);
    }

    #[test]
    fn scan_cache_errors_when_the_directory_is_absent() {
        let dir = TempDir::new().unwrap();
        assert!(scan_cache(&dir.path().join("nope")).is_err());
    }

    // -- source discovery ---------------------------------------------------

    #[test]
    fn collect_sources_skips_noise_dirs_and_the_output_dir() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::write(root.join("main.rs"), "fn main() {}").unwrap();
        fs::write(root.join("notes.md"), "hello").unwrap();
        fs::write(root.join("image.bin"), "x").unwrap(); // unclassifiable

        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join("target/built.rs"), "x").unwrap();
        fs::create_dir_all(root.join(".hidden")).unwrap();
        fs::write(root.join(".hidden/secret.rs"), "x").unwrap();

        let out = root.join("graphify-rs-out");
        fs::create_dir_all(&out).unwrap();
        fs::write(out.join("GRAPH_REPORT.md"), "x").unwrap();

        let found = collect_sources(root, Some(&out));
        let mut names: Vec<String> = found
            .iter()
            .map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["main.rs", "notes.md"]);
    }

    #[test]
    fn is_noise_dir_matches_the_build_walk() {
        assert!(is_noise_dir("target"));
        assert!(is_noise_dir("node_modules"));
        assert!(is_noise_dir("my_venv"));
        assert!(is_noise_dir("pkg.egg-info"));
        assert!(!is_noise_dir("src"));
    }

    // -- credential reporting -----------------------------------------------

    #[test]
    fn credential_source_never_returns_the_secret() {
        assert_eq!(
            credential_source(Some("sk-super-secret"), "NOPE_UNSET_VAR"),
            Some("from graphify-rs.toml")
        );
        assert_eq!(credential_source(Some(""), "NOPE_UNSET_VAR"), None);
        assert_eq!(credential_source(None, "NOPE_UNSET_VAR"), None);
        assert_eq!(credential_source(None, ""), None);
    }

    // -- git discovery ------------------------------------------------------

    #[test]
    fn find_git_root_walks_up_to_the_repository() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".git")).unwrap();
        let nested = root.join("a/b/c");
        fs::create_dir_all(&nested).unwrap();
        assert_eq!(find_git_root(&nested), Some(root.to_path_buf()));
    }

    #[test]
    fn current_branch_reads_head_without_the_git_binary() {
        let dir = TempDir::new().unwrap();
        let git = dir.path().join(".git");
        fs::create_dir_all(&git).unwrap();
        fs::write(git.join("HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        assert_eq!(current_branch(dir.path()).as_deref(), Some("feature/x"));

        // A detached HEAD holds a raw SHA, which is not a branch name.
        fs::write(git.join("HEAD"), "9f1c0de\n").unwrap();
        assert!(current_branch(dir.path()).is_none());
    }
}
