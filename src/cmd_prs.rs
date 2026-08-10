//! PRs command: graph-aware pull request dashboard.
//!
//! Everything GitHub knows about an open PR — CI, review state, age — answers
//! "is this mergeable?". None of it answers "what does merging it disturb?".
//! That second question is the one the knowledge graph can answer, so this
//! command joins the two: `gh` supplies the queue, `graph.json` supplies the
//! blast radius, and the union tells you which PRs collide with each other.
//!
//! Two costs shape the design. Reading the graph is cheap and local; asking
//! GitHub for a PR's file list is a network round trip per PR, so diffs are
//! fetched only for the modes that actually consume them, and then eight at a
//! time. Every external tool is optional: no `gh`, no repository, no graph —
//! each degrades to the most useful output still available rather than to an
//! error.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use colored::{ColoredString, Colorize};
use graphify_core::graph::KnowledgeGraph;
use serde_json::Value;
use tokio::task::JoinSet;

// ── Tuning ────────────────────────────────────────────────────────────────────

/// Ceiling on any single `gh` invocation. GitHub occasionally stalls; a
/// dashboard that hangs is worse than one that reports a timeout.
const GH_TIMEOUT: Duration = Duration::from_secs(30);

/// Ceiling on local `git` invocations, which either answer immediately or are
/// wedged on something the dashboard cannot fix.
const GIT_TIMEOUT: Duration = Duration::from_secs(10);

/// How often a running child is re-checked while waiting out its deadline.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// How many open PRs to request. Matches the reference implementation; beyond
/// this a dashboard stops being readable anyway.
const PR_LIMIT: u32 = 50;

/// Days without an update before a PR is called stale.
const STALE_DAYS: i64 = 14;

/// Concurrent `gh pr diff` calls. The work is pure network wait, but GitHub
/// rate-limits, so the fan-out is bounded rather than unlimited.
const MAX_CONCURRENT_DIFFS: usize = 8;

/// Changed files listed in the single-PR view before the rest are summarised.
const MAX_DETAIL_FILES: usize = 10;

/// Member labels sampled to describe an unnamed community.
const COMMUNITY_LABEL_SAMPLE: usize = 4;

/// Reply budget for the triage ranking.
const TRIAGE_MAX_TOKENS: u32 = 1024;

/// Model used when only `ANTHROPIC_API_KEY` is present, matching `build`.
const ENV_FALLBACK_MODEL: &str = "claude-sonnet-4.6";

const SECONDS_PER_DAY: i64 = 86_400;

// Dashboard column widths, in visible characters.
const W_NUM: usize = 6;
const W_MARK: usize = 2;
const W_CI: usize = 2;
const W_STATUS: usize = 13;
const W_AGE: usize = 8;
const W_IMPACT: usize = 22;
const MAX_TITLE: usize = 52;

/// Options for the `prs` dashboard, mirroring the CLI flags.
pub struct PrsArgs {
    /// Show detail for one PR instead of the dashboard.
    pub number: Option<u32>,
    pub triage: bool,
    pub worktrees: bool,
    pub conflicts: bool,
    /// Also list PRs opened against the wrong base branch.
    pub wrong_base: bool,
    /// Base branch; auto-detected from the repository when absent.
    pub base: Option<String>,
    /// `owner/repo`; defaults to the repository in the working directory.
    pub repo: Option<String>,
    pub graph_path: String,
}

/// Render the pull request dashboard.
pub async fn cmd_prs(args: PrsArgs, llm: Option<crate::config::LLMConfig>) -> Result<()> {
    let now = now_unix();
    let repo = args.repo.as_deref();
    let base = match &args.base {
        Some(b) => b.clone(),
        None => detect_default_branch(repo),
    };

    let mut prs = match fetch_prs(repo, &base, now) {
        Ok(prs) => prs,
        // Being outside a GitHub checkout is a situation, not a fault: say so
        // and name the flag that fixes it.
        Err(FetchFailure::NoRepoContext) => {
            println!(
                "\n  {} not inside a GitHub repository — pass {} to target one\n",
                "·".dimmed(),
                "--repo owner/repo".cyan()
            );
            return Ok(());
        }
        Err(failure) => bail!(failure.advice()),
    };

    let worktrees = fetch_worktrees();
    for pr in &mut prs {
        pr.worktree_path = worktrees.get(&pr.branch).cloned();
    }

    // A diff costs a round trip per PR, so it is fetched only where the answer
    // is actually shown: the single-PR view, conflict detection, and triage.
    let index = load_impact_index(&args.graph_path);
    if let Some(index) = &index
        && (args.number.is_some() || args.triage || args.conflicts)
    {
        attach_graph_impact(&mut prs, index, repo, now).await;
    }

    if let Some(number) = args.number {
        let Some(pr) = prs.iter().find(|p| p.number == number) else {
            bail!(
                "PR #{number} is not open in this repository{}",
                open_numbers_hint(&prs)
            );
        };
        render_pr_detail(pr, now);
        return Ok(());
    }

    if args.worktrees {
        render_worktrees(&prs, &worktrees, now);
        return Ok(());
    }

    render_dashboard(&prs, &base, args.wrong_base, now);

    if args.conflicts {
        render_conflicts(&prs, &base, index.as_ref(), now);
    }
    if args.triage {
        run_triage(&prs, &base, llm.as_ref(), now).await;
    }
    Ok(())
}

/// " (open: #3, #7)" — or nothing when the queue is empty.
fn open_numbers_hint(prs: &[PrInfo]) -> String {
    if prs.is_empty() {
        return String::new();
    }
    let numbers: Vec<String> = prs.iter().map(|p| format!("#{}", p.number)).collect();
    format!(" (open: {})", numbers.join(", "))
}

// ── Text helpers ──────────────────────────────────────────────────────────────

/// Printable width of a string, ignoring ANSI escape sequences.
///
/// `colored` embeds escapes in the string it returns, so `{:width$}` pads
/// against the byte count and silently ruins every column. Counting only the
/// characters a terminal draws keeps the table aligned whether or not colour
/// is enabled.
fn visible_len(s: &str) -> usize {
    let mut len = 0;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // Consume up to and including the sequence terminator.
            for esc in chars.by_ref() {
                if esc.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            len += 1;
        }
    }
    len
}

/// Left-align `s` in a `width`-wide cell.
fn pad(s: &str, width: usize) -> String {
    format!("{s}{}", " ".repeat(width.saturating_sub(visible_len(s))))
}

/// Right-align `s` in a `width`-wide cell.
fn pad_left(s: &str, width: usize) -> String {
    format!("{}{s}", " ".repeat(width.saturating_sub(visible_len(s))))
}

/// Shorten `s` to `max` characters, marking the cut with an ellipsis.
///
/// Counts characters rather than bytes: PR titles routinely carry emoji and
/// non-ASCII text, and slicing those by byte offset panics.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

// ── Time ──────────────────────────────────────────────────────────────────────

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Days elapsed between two Unix timestamps, never negative.
///
/// A clock skewed behind GitHub's would otherwise report a PR as updated in
/// the future and sort it ahead of everything real.
fn days_between(from: i64, to: i64) -> i64 {
    (to - from).max(0) / SECONDS_PER_DAY
}

/// Parse an RFC 3339 timestamp (`2026-08-09T12:34:56Z`) into Unix seconds.
///
/// Returns `None` instead of guessing when the shape is unfamiliar: a wrong
/// age silently flips a PR into STALE, which is worse than showing no age.
fn parse_timestamp(s: &str) -> Option<i64> {
    let (date, rest) = s.split_once('T')?;
    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: i64 = date_parts.next()?.parse().ok()?;
    let day: i64 = date_parts.next()?.parse().ok()?;
    if date_parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    // Split the clock time from its zone designator. The offset sign doubles
    // as the delimiter, so the scan stops at the first of Z, + or -.
    let zone_at = rest.find(['Z', 'z', '+', '-']).unwrap_or(rest.len());
    let (clock, zone) = rest.split_at(zone_at);

    let mut clock_parts = clock.split(':');
    let hour: i64 = clock_parts.next()?.parse().ok()?;
    let minute: i64 = clock_parts.next()?.parse().ok()?;
    // Fractional seconds carry no information at day resolution.
    let second_text = clock_parts.next().unwrap_or("0");
    let second: i64 = second_text
        .split_once('.')
        .map_or(second_text, |(whole, _)| whole)
        .parse()
        .ok()?;
    if clock_parts.next().is_some() || hour > 23 || minute > 59 || second > 60 {
        return None;
    }

    let offset = parse_utc_offset(zone)?;
    let days = days_from_civil(year, month, day);
    Some(days * SECONDS_PER_DAY + hour * 3600 + minute * 60 + second - offset)
}

/// Seconds to subtract to reach UTC. `""`/`"Z"` mean the time is already UTC.
fn parse_utc_offset(zone: &str) -> Option<i64> {
    if zone.is_empty() || zone.eq_ignore_ascii_case("Z") {
        return Some(0);
    }
    let sign = match zone.as_bytes()[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let (hours, minutes) = zone[1..].split_once(':')?;
    let hours: i64 = hours.parse().ok()?;
    let minutes: i64 = minutes.parse().ok()?;
    Some(sign * (hours * 3600 + minutes * 60))
}

/// Days from 1970-01-01 to a proleptic-Gregorian date (Howard Hinnant's
/// `days_from_civil`). Avoids taking on a date-time dependency for the one
/// arithmetic this command needs.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let year_of_era = y - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

// ── Data model ────────────────────────────────────────────────────────────────

/// Aggregate verdict of a PR's checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CiStatus {
    Success,
    Failure,
    Pending,
    None,
}

impl CiStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Success => "SUCCESS",
            Self::Failure => "FAILURE",
            Self::Pending => "PENDING",
            Self::None => "NONE",
        }
    }

    fn icon(self) -> ColoredString {
        match self {
            Self::Success => "✓".green(),
            Self::Failure => "✗".red(),
            Self::Pending => "…".yellow(),
            Self::None => "–".dimmed(),
        }
    }
}

/// Triage bucket for one PR.
///
/// Declaration order is *both* the classification precedence and the dashboard
/// sort order, so the derived `Ord` keeps the two from drifting apart: adding a
/// bucket in the right place is enough to place it correctly in both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum PrStatus {
    WrongBase,
    CiFail,
    ChangesReq,
    Draft,
    Stale,
    Approved,
    Pending,
    Ready,
}

impl PrStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::WrongBase => "WRONG-BASE",
            Self::CiFail => "CI-FAIL",
            Self::ChangesReq => "CHANGES-REQ",
            Self::Draft => "DRAFT",
            Self::Stale => "STALE",
            Self::Approved => "APPROVED",
            Self::Pending => "PENDING",
            Self::Ready => "READY",
        }
    }

    fn colored(self) -> ColoredString {
        let text = self.as_str();
        match self {
            Self::Ready => text.green(),
            Self::Approved => text.green().bold(),
            Self::CiFail | Self::ChangesReq => text.red(),
            Self::WrongBase | Self::Stale => text.dimmed(),
            Self::Draft | Self::Pending => text.yellow(),
        }
    }
}

/// One open pull request, plus whatever the graph knows about it.
#[derive(Debug, Clone)]
struct PrInfo {
    number: u32,
    title: String,
    branch: String,
    base_branch: String,
    author: String,
    is_draft: bool,
    /// `APPROVED`, `CHANGES_REQUESTED`, or empty when nobody has reviewed.
    review_decision: String,
    ci_status: CiStatus,
    /// `updatedAt` as Unix seconds.
    updated_at: i64,
    /// Base branch this PR *should* target.
    expected_base: String,
    worktree_path: Option<String>,
    communities_touched: Vec<usize>,
    nodes_affected: usize,
    files_changed: Vec<String>,
}

impl PrInfo {
    fn days_old(&self, now: i64) -> i64 {
        days_between(self.updated_at, now)
    }

    fn status(&self, now: i64) -> PrStatus {
        classify(self, now)
    }

    /// "12 nodes / 3 communities", or `None` when the graph knows nothing.
    fn blast_radius(&self) -> Option<String> {
        if self.nodes_affected == 0 {
            return None;
        }
        let nodes = self.nodes_affected;
        let communities = self.communities_touched.len();
        Some(format!(
            "{nodes} node{} / {communities} communit{}",
            if nodes == 1 { "" } else { "s" },
            if communities == 1 { "y" } else { "ies" }
        ))
    }

    /// One line describing the PR to an LLM or a plain-text consumer.
    fn summary(&self, now: i64) -> String {
        let review = if self.review_decision.is_empty() {
            "none"
        } else {
            &self.review_decision
        };
        let impact = self
            .blast_radius()
            .map(|b| format!(", blast_radius={b}"))
            .unwrap_or_default();
        format!(
            "PR #{} [{}] CI={} review={review} age={}d author={}{impact}\n  title: {}",
            self.number,
            self.status(now).as_str(),
            self.ci_status.as_str(),
            self.days_old(now),
            self.author,
            self.title
        )
    }
}

/// Bucket a PR, first match wins.
///
/// The order encodes what a maintainer should look at first: a PR aimed at the
/// wrong branch cannot be judged on its CI, and a red build makes its review
/// state irrelevant.
fn classify(pr: &PrInfo, now: i64) -> PrStatus {
    if pr.base_branch != pr.expected_base {
        return PrStatus::WrongBase;
    }
    if pr.ci_status == CiStatus::Failure {
        return PrStatus::CiFail;
    }
    if pr.review_decision == "CHANGES_REQUESTED" {
        return PrStatus::ChangesReq;
    }
    if pr.is_draft {
        return PrStatus::Draft;
    }
    if pr.days_old(now) >= STALE_DAYS {
        return PrStatus::Stale;
    }
    if pr.review_decision == "APPROVED" {
        return PrStatus::Approved;
    }
    if pr.ci_status == CiStatus::Pending {
        return PrStatus::Pending;
    }
    PrStatus::Ready
}

/// PRs aimed at `base`, most urgent first, newest first within a bucket.
fn sort_prs<'p>(prs: &'p [PrInfo], base: &str, now: i64) -> Vec<&'p PrInfo> {
    let mut actionable: Vec<&PrInfo> = prs.iter().filter(|p| p.base_branch == base).collect();
    actionable.sort_by_key(|p| (p.status(now), p.days_old(now), p.number));
    actionable
}

/// How many PRs sit in each bucket.
fn status_counts(prs: &[&PrInfo], now: i64) -> BTreeMap<PrStatus, usize> {
    let mut counts = BTreeMap::new();
    for pr in prs {
        *counts.entry(pr.status(now)).or_insert(0) += 1;
    }
    counts
}

// ── External commands ─────────────────────────────────────────────────────────

/// Why an external command produced no usable output.
#[derive(Debug, PartialEq, Eq)]
enum CmdError {
    /// The binary is not on `PATH`.
    Missing,
    /// It ran and exited non-zero; carries stderr so the caller can explain why.
    Failed(String),
    /// It outlived its deadline and was killed.
    TimedOut,
}

/// Run a command with a hard deadline and return its stdout.
///
/// `Command::output` has no timeout and `tokio::process` is not compiled into
/// this binary, so the child is polled to a deadline here. Both pipes are
/// drained on their own threads for a non-obvious reason: a child that fills a
/// pipe buffer blocks until someone reads it, which would outlast any deadline
/// we set on the parent side.
fn run(program: &str, args: &[&str], timeout: Duration) -> Result<String, CmdError> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => CmdError::Missing,
            _ => CmdError::Failed(e.to_string()),
        })?;

    let stdout = child.stdout.take().map(drain);
    let stderr = child.stderr.take().map(drain);

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(CmdError::TimedOut);
            }
            Ok(None) => std::thread::sleep(POLL_INTERVAL),
            Err(e) => return Err(CmdError::Failed(e.to_string())),
        }
    };

    let collect = |h: Option<JoinHandle<String>>| h.and_then(|h| h.join().ok()).unwrap_or_default();
    if status.success() {
        Ok(collect(stdout))
    } else {
        Err(CmdError::Failed(collect(stderr)))
    }
}

fn drain<R: Read + Send + 'static>(mut reader: R) -> JoinHandle<String> {
    std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = reader.read_to_string(&mut buf);
        buf
    })
}

/// Run `gh` and parse its `--json` reply.
fn gh_json(args: &[&str]) -> Result<Value, CmdError> {
    let out = run("gh", args, GH_TIMEOUT)?;
    serde_json::from_str(&out).map_err(|e| CmdError::Failed(e.to_string()))
}

/// Append `--repo owner/repo` when the user targeted another repository.
fn with_repo<'a>(args: &mut Vec<&'a str>, repo: Option<&'a str>) {
    if let Some(repo) = repo {
        args.push("--repo");
        args.push(repo);
    }
}

// ── Fetching from GitHub ──────────────────────────────────────────────────────

/// What `gh`'s stderr is really complaining about.
///
/// `gh` reports both "you are logged out" and "this is not a GitHub checkout"
/// as a non-zero exit with prose on stderr. Echoing that prose leaves the user
/// to work out which one happened, so it is classified into the two cases that
/// have different fixes.
#[derive(Debug, PartialEq, Eq)]
enum GhRejection {
    NeedsAuth,
    NoRepoContext,
    Other,
}

fn classify_gh_stderr(stderr: &str) -> GhRejection {
    let lower = stderr.to_lowercase();
    if lower.contains("gh auth login")
        || lower.contains("authentication token")
        || lower.contains("not logged in")
        || lower.contains("requires authentication")
        || lower.contains("http 401")
    {
        return GhRejection::NeedsAuth;
    }
    if lower.contains("not a git repository")
        || lower.contains("no git remotes")
        || lower.contains("git remotes configured")
        || lower.contains("could not determine base repository")
    {
        return GhRejection::NoRepoContext;
    }
    GhRejection::Other
}

/// Why the PR list could not be read.
#[derive(Debug, PartialEq, Eq)]
enum FetchFailure {
    MissingCli,
    NeedsAuth,
    NoRepoContext,
    Timeout,
    Other(String),
}

impl FetchFailure {
    /// A single line naming the next command to run. Deliberately not an error
    /// chain: "caused by: exit status 1" tells the user nothing they can act on.
    fn advice(&self) -> String {
        match self {
            Self::MissingCli => "GitHub CLI (`gh`) not found on PATH. \
                 Install it from https://cli.github.com, then run: gh auth login"
                .to_string(),
            Self::NeedsAuth => "GitHub CLI is not authenticated. Run: gh auth login".to_string(),
            Self::NoRepoContext => {
                "not inside a GitHub repository — pass --repo owner/repo".to_string()
            }
            Self::Timeout => format!(
                "`gh pr list` timed out after {}s. Check your network, then retry",
                GH_TIMEOUT.as_secs()
            ),
            Self::Other(detail) => {
                let first = detail.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
                format!("`gh pr list` failed: {}", first.trim())
            }
        }
    }
}

impl From<CmdError> for FetchFailure {
    fn from(e: CmdError) -> Self {
        match e {
            CmdError::Missing => Self::MissingCli,
            CmdError::TimedOut => Self::Timeout,
            CmdError::Failed(stderr) => match classify_gh_stderr(&stderr) {
                GhRejection::NeedsAuth => Self::NeedsAuth,
                GhRejection::NoRepoContext => Self::NoRepoContext,
                GhRejection::Other => Self::Other(stderr),
            },
        }
    }
}

/// The branch PRs are expected to target: ask `gh`, then `git`, then assume
/// `main`. `gh` goes first because it also answers for `--repo`, which the
/// local checkout knows nothing about.
fn detect_default_branch(repo: Option<&str>) -> String {
    let mut args = vec!["repo", "view", "--json", "defaultBranchRef"];
    with_repo(&mut args, repo);
    if let Ok(value) = gh_json(&args)
        && let Some(name) = value
            .get("defaultBranchRef")
            .and_then(|r| r.get("name"))
            .and_then(Value::as_str)
        && !name.is_empty()
    {
        return name.to_string();
    }

    if let Ok(out) = run(
        "git",
        &["symbolic-ref", "refs/remotes/origin/HEAD"],
        GIT_TIMEOUT,
    ) && let Some(name) = out.trim().rsplit('/').next()
        && !name.is_empty()
    {
        return name.to_string();
    }

    "main".to_string()
}

/// The open PRs, annotated with the base they are expected to target.
fn fetch_prs(repo: Option<&str>, base: &str, now: i64) -> Result<Vec<PrInfo>, FetchFailure> {
    let limit = PR_LIMIT.to_string();
    let mut args = vec![
        "pr",
        "list",
        "--state",
        "open",
        "--limit",
        &limit,
        "--json",
        "number,title,headRefName,baseRefName,author,isDraft,\
         reviewDecision,statusCheckRollup,updatedAt",
    ];
    with_repo(&mut args, repo);

    let raw = run("gh", &args, GH_TIMEOUT)?;
    parse_pr_list(&raw, base, now).map_err(|e| FetchFailure::Other(e.to_string()))
}

/// Turn `gh pr list --json …` output into [`PrInfo`]s.
fn parse_pr_list(raw: &str, expected_base: &str, now: i64) -> Result<Vec<PrInfo>> {
    let value: Value = serde_json::from_str(raw)?;
    let Some(items) = value.as_array() else {
        bail!("expected a JSON array of pull requests");
    };

    let text = |item: &Value, key: &str| {
        item.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };

    Ok(items
        .iter()
        .filter_map(|item| {
            let number = item.get("number").and_then(Value::as_u64)? as u32;
            let rollup = item
                .get("statusCheckRollup")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default();
            Some(PrInfo {
                number,
                title: text(item, "title"),
                branch: text(item, "headRefName"),
                base_branch: text(item, "baseRefName"),
                author: item
                    .get("author")
                    .and_then(|a| a.get("login"))
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_string(),
                is_draft: item
                    .get("isDraft")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                review_decision: text(item, "reviewDecision"),
                ci_status: parse_ci(rollup),
                // An unreadable timestamp is treated as "just now" so the PR
                // is never wrongly demoted to STALE on a formatting quirk.
                updated_at: parse_timestamp(&text(item, "updatedAt")).unwrap_or(now),
                expected_base: expected_base.to_string(),
                worktree_path: None,
                communities_touched: Vec::new(),
                nodes_affected: 0,
                files_changed: Vec::new(),
            })
        })
        .collect())
}

/// Conclusions GitHub reports for a check that did not pass.
const CI_FAILURE_CONCLUSIONS: [&str; 5] = [
    "FAILURE",
    "CANCELLED",
    "TIMED_OUT",
    "ACTION_REQUIRED",
    "STARTUP_FAILURE",
];

/// Collapse a `statusCheckRollup` array into one verdict.
///
/// Failure outranks everything — one red check is enough to stop a merge — and
/// still-running checks outrank green ones, because a partially green rollup
/// has not finished having an opinion yet.
fn parse_ci(rollup: &[Value]) -> CiStatus {
    let (mut failed, mut running, mut passed) = (false, false, false);
    for check in rollup {
        if let Some(conclusion) = check.get("conclusion").and_then(Value::as_str) {
            failed |= CI_FAILURE_CONCLUSIONS.contains(&conclusion);
            passed |= conclusion == "SUCCESS";
        }
        if let Some(status) = check.get("status").and_then(Value::as_str) {
            running |= matches!(status, "IN_PROGRESS" | "QUEUED");
        }
    }
    match (failed, running, passed) {
        (true, _, _) => CiStatus::Failure,
        (_, true, _) => CiStatus::Pending,
        (_, _, true) => CiStatus::Success,
        _ => CiStatus::None,
    }
}

/// Files a PR touches. A failure here is not fatal — the PR simply shows no
/// impact — so the error is swallowed rather than aborting the dashboard.
fn fetch_pr_files(number: u32, repo: Option<&str>) -> Vec<String> {
    let number = number.to_string();
    let mut args = vec!["pr", "diff", &number, "--name-only"];
    with_repo(&mut args, repo);
    let Ok(out) = run("gh", &args, GH_TIMEOUT) else {
        return Vec::new();
    };
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Local worktrees as branch → path.
fn fetch_worktrees() -> HashMap<String, String> {
    run("git", &["worktree", "list", "--porcelain"], GIT_TIMEOUT)
        .map(|out| parse_worktrees(&out))
        .unwrap_or_default()
}

/// Parse `git worktree list --porcelain` into branch → path.
///
/// A blank line separates records. Resetting on it matters: a detached-HEAD
/// worktree emits no `branch` line, and without the reset its successor's
/// branch would be attributed to the wrong path.
fn parse_worktrees(porcelain: &str) -> HashMap<String, String> {
    let mut mapping = HashMap::new();
    let mut current: Option<&str> = None;
    for line in porcelain.lines() {
        if line.trim().is_empty() {
            current = None;
        } else if let Some(path) = line.strip_prefix("worktree ") {
            current = Some(path);
        } else if let Some(branch) = line.strip_prefix("branch refs/heads/")
            && let Some(path) = current
        {
            mapping.insert(branch.to_string(), path.to_string());
        }
    }
    mapping
}

// ── Graph impact ──────────────────────────────────────────────────────────────

/// Last path segment, which is all `path_match` can ever key on.
fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// True when `long` ends with `short` at a path boundary.
fn ends_with_segment(long: &str, short: &str) -> bool {
    long.len() > short.len()
        && long.ends_with(short)
        && long.as_bytes()[long.len() - short.len() - 1] == b'/'
}

/// True when a graph source path and a PR file path name the same file.
///
/// The graph stores whatever path the extractor walked and `gh` reports paths
/// relative to the repository root, so one is often a suffix of the other. The
/// boundary check is the whole point: a bare `ends_with` would decide that
/// `crates/ab.rs` and `b.rs` are the same file.
fn path_match(graph_src: &str, pr_file: &str) -> bool {
    graph_src == pr_file
        || ends_with_segment(graph_src, pr_file)
        || ends_with_segment(pr_file, graph_src)
}

/// One source file's footprint in the graph.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct FileFootprint {
    communities: BTreeSet<usize>,
    nodes: usize,
}

/// File → graph footprint, bucketed by path basename.
///
/// Built once per run and then queried per PR. The bucketing is what keeps the
/// query cheap: `path_match` can only succeed when both paths end in the same
/// segment, so a changed file need only be compared against the handful of
/// graph files sharing its name instead of against every file in the graph.
struct ImpactIndex {
    by_basename: HashMap<String, Vec<(String, FileFootprint)>>,
    /// Community id → a few words describing it.
    community_labels: BTreeMap<usize, Vec<String>>,
}

impl ImpactIndex {
    /// Build from `(source_file, community, node_label)` triples.
    fn from_nodes<'a>(nodes: impl IntoIterator<Item = (&'a str, Option<usize>, &'a str)>) -> Self {
        let mut footprints: HashMap<&str, FileFootprint> = HashMap::new();
        let mut community_labels: BTreeMap<usize, Vec<String>> = BTreeMap::new();

        for (source_file, community, label) in nodes {
            if let Some(id) = community {
                let sample = community_labels.entry(id).or_default();
                if sample.len() < COMMUNITY_LABEL_SAMPLE && !label.is_empty() {
                    sample.push(label.to_string());
                }
            }
            if source_file.is_empty() {
                continue;
            }
            let entry = footprints.entry(source_file).or_default();
            entry.nodes += 1;
            if let Some(id) = community {
                entry.communities.insert(id);
            }
        }

        let mut by_basename: HashMap<String, Vec<(String, FileFootprint)>> = HashMap::new();
        for (source_file, footprint) in footprints {
            by_basename
                .entry(basename(source_file).to_string())
                .or_default()
                .push((source_file.to_string(), footprint));
        }
        Self {
            by_basename,
            community_labels,
        }
    }

    fn from_graph(graph: &KnowledgeGraph) -> Self {
        let mut index = Self::from_nodes(
            graph
                .nodes()
                .into_iter()
                .map(|n| (n.source_file.as_str(), n.community, n.label.as_str())),
        );
        // A community that `graphify-rs label` has already named describes
        // itself better than any sample of its members.
        for community in &graph.communities {
            if let Some(name) = &community.label
                && !name.is_empty()
            {
                index
                    .community_labels
                    .insert(community.id, vec![name.clone()]);
            }
        }
        index
    }

    /// Communities and node count reached by a set of changed files.
    ///
    /// A graph file is counted once per call even if two changed paths both
    /// resolve to it, which they can when the diff lists a path in more than
    /// one form.
    fn impact(&self, files: &[String]) -> (Vec<usize>, usize) {
        let mut communities = BTreeSet::new();
        let mut nodes = 0;
        let mut matched: HashSet<&str> = HashSet::new();

        for file in files {
            let Some(bucket) = self.by_basename.get(basename(file)) else {
                continue;
            };
            for (source_file, footprint) in bucket {
                if path_match(source_file, file) && matched.insert(source_file.as_str()) {
                    communities.extend(footprint.communities.iter().copied());
                    nodes += footprint.nodes;
                }
            }
        }
        (communities.into_iter().collect(), nodes)
    }

    fn describe_community(&self, id: usize) -> Option<String> {
        let labels = self.community_labels.get(&id)?;
        (!labels.is_empty()).then(|| labels.join(", "))
    }
}

/// Load and index the graph, or `None` when there is no usable one.
///
/// A missing graph is not an error: every mode still works without it, minus
/// the IMPACT column. Only a graph that exists but cannot be read is worth a
/// warning, because that one is a surprise.
fn load_impact_index(graph_path: &str) -> Option<ImpactIndex> {
    let path = Path::new(graph_path);
    if !path.exists() {
        return None;
    }
    match graphify_serve::load_graph(path) {
        Ok(graph) => Some(ImpactIndex::from_graph(&graph)),
        Err(e) => {
            eprintln!(
                "  {} could not read {}: {e}",
                "⚠".yellow(),
                path.display().to_string().dimmed()
            );
            None
        }
    }
}

/// Fetch each PR's diff and record what it disturbs in the graph.
///
/// The diffs are network-bound and independent, so they run on the blocking
/// pool with a bounded fan-out; `gh` is a process, not a future, and blocking
/// the async runtime on fifty of them serially is the slow path this avoids.
/// WRONG-BASE PRs are skipped — their diff is against the wrong branch, so any
/// impact computed from it would be fiction.
async fn attach_graph_impact(
    prs: &mut [PrInfo],
    index: &ImpactIndex,
    repo: Option<&str>,
    now: i64,
) {
    let mut queue = prs
        .iter()
        .filter(|p| p.status(now) != PrStatus::WrongBase)
        .map(|p| p.number)
        .collect::<Vec<_>>()
        .into_iter();

    let mut tasks: JoinSet<(u32, Vec<String>)> = JoinSet::new();
    let mut diffs: HashMap<u32, Vec<String>> = HashMap::new();
    loop {
        while tasks.len() < MAX_CONCURRENT_DIFFS {
            let Some(number) = queue.next() else { break };
            let repo = repo.map(str::to_string);
            tasks.spawn_blocking(move || (number, fetch_pr_files(number, repo.as_deref())));
        }
        match tasks.join_next().await {
            // A task that panicked simply contributes no impact for its PR.
            Some(Ok((number, files))) => {
                diffs.insert(number, files);
            }
            Some(Err(_)) => {}
            None => break,
        }
    }

    for pr in prs {
        if let Some(files) = diffs.remove(&pr.number) {
            let (communities, nodes) = index.impact(&files);
            pr.communities_touched = communities;
            pr.nodes_affected = nodes;
            pr.files_changed = files;
        }
    }
}

/// Communities touched by more than one PR, most contended first.
///
/// This is the merge-order warning: two PRs in the same community are editing
/// the same cluster of the codebase, whether or not they touch the same file.
fn find_conflicts<'a>(prs: impl IntoIterator<Item = (u32, &'a [usize])>) -> Vec<(usize, Vec<u32>)> {
    let mut by_community: BTreeMap<usize, Vec<u32>> = BTreeMap::new();
    for (number, communities) in prs {
        for &id in communities {
            by_community.entry(id).or_default().push(number);
        }
    }
    let mut shared: Vec<(usize, Vec<u32>)> = by_community
        .into_iter()
        .filter(|(_, numbers)| numbers.len() > 1)
        .collect();
    // Most-contended first; community id breaks ties so output is stable.
    shared.sort_by_key(|(id, numbers)| (std::cmp::Reverse(numbers.len()), *id));
    shared
}

// ── Rendering ─────────────────────────────────────────────────────────────────

fn render_dashboard(prs: &[PrInfo], base: &str, show_wrong_base: bool, now: i64) {
    let actionable = sort_prs(prs, base, now);
    let wrong_base: Vec<&PrInfo> = prs.iter().filter(|p| p.base_branch != base).collect();

    println!();
    println!(
        "  {}",
        format!("graphify prs  ·  base: {base}  ·  {} PRs", actionable.len()).bold()
    );
    println!();

    if actionable.is_empty() {
        println!("  {}", "No open PRs targeting this base branch.".dimmed());
    } else {
        print_table_header();
        for pr in &actionable {
            println!("{}", dashboard_row(pr, now));
        }
    }

    let counts = status_counts(&actionable, now);
    println!();
    println!("  {}", summary_line(&counts, wrong_base.len()));
    println!();

    if show_wrong_base && !wrong_base.is_empty() {
        let mut wrong_base = wrong_base;
        wrong_base.sort_by_key(|p| std::cmp::Reverse(p.number));
        println!(
            "  {}",
            format!("── {} PRs targeting wrong base ──", wrong_base.len()).dimmed()
        );
        for pr in wrong_base {
            println!(
                "  {}",
                format!(
                    "#{:<5} base={:<14} {}",
                    pr.number,
                    pr.base_branch,
                    truncate(&pr.title, 60)
                )
                .dimmed()
            );
        }
        println!();
    }
}

fn print_table_header() {
    println!(
        "  {}{}  {}  {}  {}  {}  TITLE",
        pad("#", W_NUM),
        pad("", W_MARK),
        pad("CI", W_CI),
        pad("STATUS", W_STATUS),
        pad_left("UPDATED", W_AGE),
        pad("IMPACT", W_IMPACT),
    );
    let rule = |n: usize| "─".repeat(n);
    println!(
        "  {}{}  {}  {}  {}  {}  {}",
        rule(W_NUM),
        rule(W_MARK),
        rule(W_CI),
        rule(W_STATUS),
        rule(W_AGE),
        rule(W_IMPACT),
        rule(MAX_TITLE),
    );
}

fn dashboard_row(pr: &PrInfo, now: i64) -> String {
    let days = pr.days_old(now);
    let age = if days > 0 {
        format!("{days}d")
    } else {
        "today".to_string()
    };
    let impact = match pr.blast_radius() {
        Some(text) => truncate(&text, W_IMPACT).dimmed(),
        None => "–".dimmed(),
    };
    // The marker earns its column: a worktree means the branch is already
    // checked out somewhere, so reviewing it costs nothing.
    let mark = if pr.worktree_path.is_some() {
        format!(" {}", "⬡".cyan())
    } else {
        String::new()
    };
    let draft = if pr.is_draft {
        " [draft]".dimmed().to_string()
    } else {
        String::new()
    };
    format!(
        "  {}{}  {}  {}  {}  {}  {}{}",
        pad(&format!("#{}", pr.number).bold().to_string(), W_NUM),
        pad(&mark, W_MARK),
        pad(&pr.ci_status.icon().to_string(), W_CI),
        pad(&pr.status(now).colored().to_string(), W_STATUS),
        pad_left(&age, W_AGE),
        pad(&impact.to_string(), W_IMPACT),
        truncate(&pr.title, MAX_TITLE),
        draft,
    )
}

/// "3 ready · 1 CI failing · 2 stale".
fn summary_line(counts: &BTreeMap<PrStatus, usize>, wrong_base: usize) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut push = |status: PrStatus, render: &dyn Fn(String) -> ColoredString, noun: &str| {
        if let Some(&n) = counts.get(&status) {
            parts.push(render(format!("{n} {noun}")).to_string());
        }
    };
    push(PrStatus::Ready, &|s| s.green(), "ready");
    push(PrStatus::Approved, &|s| s.green().bold(), "approved");
    push(PrStatus::Pending, &|s| s.yellow(), "pending CI");
    push(PrStatus::CiFail, &|s| s.red(), "CI failing");
    push(PrStatus::ChangesReq, &|s| s.red(), "changes requested");
    push(PrStatus::Draft, &|s| s.yellow(), "draft");
    push(PrStatus::Stale, &|s| s.dimmed(), "stale");
    if wrong_base > 0 {
        parts.push(format!("{wrong_base} wrong base").dimmed().to_string());
    }
    if parts.is_empty() {
        return "nothing to review".dimmed().to_string();
    }
    parts.join(" · ")
}

fn render_pr_detail(pr: &PrInfo, now: i64) {
    println!();
    println!(
        "  {}  ·  {}",
        format!("PR #{}", pr.number).bold(),
        pr.status(now).colored()
    );
    println!("  {}", pr.title);
    println!();
    println!(
        "  {}  {}  →  {}",
        "branch:".dimmed(),
        pr.branch,
        pr.base_branch
    );
    println!("  {}  {}", "author:".dimmed(), pr.author);
    println!("  {} {}d ago", "updated:".dimmed(), pr.days_old(now));
    println!(
        "  {}      {} {}",
        "CI:".dimmed(),
        pr.ci_status.icon(),
        pr.ci_status.as_str()
    );
    if !pr.review_decision.is_empty() {
        println!("  {}  {}", "review:".dimmed(), pr.review_decision);
    }
    if let Some(path) = &pr.worktree_path {
        println!("  {} {}", "worktree:".dimmed(), path.cyan());
    }

    match pr.blast_radius() {
        Some(radius) => {
            println!();
            println!("  {}  {radius}", "Graph impact:".bold());
            println!("  {} {:?}", "communities:".dimmed(), pr.communities_touched);
        }
        None => {
            println!();
            println!(
                "  {}",
                "No graph impact — build a graph with `graphify-rs build` to see it.".dimmed()
            );
        }
    }
    if !pr.files_changed.is_empty() {
        println!("  {} {}", "files changed:".dimmed(), pr.files_changed.len());
        for file in pr.files_changed.iter().take(MAX_DETAIL_FILES) {
            println!("    {}", file.dimmed());
        }
        if pr.files_changed.len() > MAX_DETAIL_FILES {
            println!(
                "    {}",
                format!("… and {} more", pr.files_changed.len() - MAX_DETAIL_FILES).dimmed()
            );
        }
    }
    println!();
}

fn render_worktrees(prs: &[PrInfo], worktrees: &HashMap<String, String>, now: i64) {
    println!();
    println!("  {}", "Worktrees".bold());
    println!();
    if worktrees.is_empty() {
        println!("  {}", "No active worktrees found.".dimmed());
        println!();
        return;
    }

    let by_branch: HashMap<&str, &PrInfo> = prs.iter().map(|p| (p.branch.as_str(), p)).collect();
    let mut branches: Vec<(&String, &String)> = worktrees.iter().collect();
    branches.sort();

    for (branch, path) in branches {
        println!("  {}", path.cyan());
        match by_branch.get(branch.as_str()) {
            Some(pr) => println!(
                "    {} {branch}  →  PR {}  [{}]  {}",
                "branch:".dimmed(),
                format!("#{}", pr.number).bold(),
                pr.status(now).colored(),
                truncate(&pr.title, 50)
            ),
            None => println!(
                "    {} {branch}  {}",
                "branch:".dimmed(),
                "(no open PR)".dimmed()
            ),
        }
        println!();
    }
}

fn render_conflicts(prs: &[PrInfo], base: &str, index: Option<&ImpactIndex>, now: i64) {
    let actionable: Vec<&PrInfo> = prs
        .iter()
        .filter(|p| p.base_branch == base && !p.communities_touched.is_empty())
        .collect();

    if actionable.is_empty() {
        let reason = if index.is_some() {
            "No graph impact for these PRs — nothing to compare."
        } else {
            "No graph found — run `graphify-rs build` first to detect conflicts."
        };
        println!("  {}\n", reason.dimmed());
        return;
    }

    let conflicts = find_conflicts(
        actionable
            .iter()
            .map(|p| (p.number, p.communities_touched.as_slice())),
    );
    if conflicts.is_empty() {
        println!(
            "  {}\n",
            "No community overlap between open PRs — safe to merge in any order.".green()
        );
        return;
    }

    println!(
        "  {}",
        "Community conflicts (PRs sharing the same graph community)".bold()
    );
    println!();
    let by_number: HashMap<u32, &PrInfo> = actionable.iter().map(|p| (p.number, *p)).collect();
    for (community, numbers) in conflicts {
        let description = index
            .and_then(|i| i.describe_community(community))
            .map(|d| format!("  — {d}").dimmed().to_string())
            .unwrap_or_default();
        println!(
            "  {}{description}  ({} PRs overlap)",
            format!("Community {community}").yellow(),
            numbers.len()
        );
        for number in numbers {
            if let Some(pr) = by_number.get(&number) {
                println!(
                    "    {}  {}  {}",
                    pad_left(&format!("#{number}"), 5),
                    pad(&pr.status(now).colored().to_string(), W_STATUS),
                    truncate(&pr.title, 55)
                );
            }
        }
        println!();
    }
}

// ── Triage ────────────────────────────────────────────────────────────────────

/// Ask the configured model which PR to merge first.
///
/// Never fails the command: triage is advice layered on top of a dashboard the
/// user has already been shown, so an absent or unreachable model costs them
/// the ranking, not the output.
async fn run_triage(prs: &[PrInfo], base: &str, llm: Option<&crate::config::LLMConfig>, now: i64) {
    let candidates: Vec<&PrInfo> = prs
        .iter()
        .filter(|p| {
            p.base_branch == base && !matches!(p.status(now), PrStatus::WrongBase | PrStatus::Stale)
        })
        .collect();
    if candidates.is_empty() {
        println!("  {}\n", "No actionable PRs to triage.".dimmed());
        return;
    }

    let Some(config) = resolve_llm_config(llm, std::env::var("ANTHROPIC_API_KEY").ok()) else {
        println!(
            "  {} no LLM configured — add an {} section to graphify-rs.toml or set {} to rank this queue.\n",
            "·".dimmed(),
            "[llm]".cyan(),
            "ANTHROPIC_API_KEY".cyan()
        );
        return;
    };

    println!(
        "  {}{}",
        "Triage".bold(),
        format!(" ({})", config.model).dimmed()
    );
    println!();

    let prompt = build_triage_prompt(&candidates, now);
    match graphify_extract::semantic::complete_text(&prompt, TRIAGE_MAX_TOKENS, &config).await {
        Ok(reply) => {
            for line in reply.trim_end().lines() {
                println!("  {line}");
            }
            println!();
        }
        Err(e) => {
            eprintln!("  {} triage failed: {e}", "⚠".yellow());
        }
    }
}

fn build_triage_prompt(candidates: &[&PrInfo], now: i64) -> String {
    let body: Vec<String> = candidates.iter().map(|pr| pr.summary(now)).collect();
    format!(
        "You are a senior engineer helping triage a PR review queue. \
         Given these open PRs, rank them by review priority for the repo maintainer. \
         For each PR give: priority number, one sentence on what action to take and why. \
         Be direct and specific. Format each as: #<number> — <action>.\n\n{}",
        body.join("\n\n")
    )
}

/// Resolve the `[llm]` block the same way `build` and `label` do.
///
/// The environment read is a parameter rather than an inline `std::env::var`
/// so precedence can be unit-tested without mutating process-global state.
/// An invalid config degrades to `None`: triage is optional, and a typo in a
/// base URL should cost the ranking, not the dashboard.
fn resolve_llm_config(
    llm: Option<&crate::config::LLMConfig>,
    anthropic_env_key: Option<String>,
) -> Option<graphify_extract::semantic::LLMProviderConfig> {
    let raw = match llm {
        Some(cfg) => graphify_extract::semantic::LLMConfigRaw {
            provider: cfg.provider.clone().unwrap_or_default(),
            model: cfg.model.clone().unwrap_or_default(),
            anthropic_api_key: cfg.anthropic_api_key.clone(),
            anthropic_base_url: cfg.anthropic_base_url.clone(),
            openai_api_key: cfg.openai_api_key.clone(),
            openai_base_url: cfg.openai_base_url.clone(),
            ollama_base_url: cfg.ollama_base_url.clone(),
            openai_compatible_api_key: cfg.openai_compatible_api_key.clone(),
            openai_compatible_base_url: cfg.openai_compatible_base_url.clone(),
        },
        None => graphify_extract::semantic::LLMConfigRaw {
            provider: "anthropic".to_string(),
            model: ENV_FALLBACK_MODEL.to_string(),
            anthropic_api_key: Some(anthropic_env_key?),
            ..Default::default()
        },
    };
    graphify_extract::semantic::LLMProviderConfig::resolve(&raw).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-08-09T00:00:00Z — the clock every test measures ages against.
    const NOW: i64 = 1_786_233_600;

    fn pr(number: u32, base: &str) -> PrInfo {
        PrInfo {
            number,
            title: format!("PR {number}"),
            branch: format!("feat/{number}"),
            base_branch: base.to_string(),
            author: "octocat".to_string(),
            is_draft: false,
            review_decision: String::new(),
            ci_status: CiStatus::Success,
            updated_at: NOW,
            expected_base: "main".to_string(),
            worktree_path: None,
            communities_touched: Vec::new(),
            nodes_affected: 0,
            files_changed: Vec::new(),
        }
    }

    // ── Classification ───────────────────────────────────────────────────────

    #[test]
    fn classifies_wrong_base() {
        let mut p = pr(1, "develop");
        // Wrong base wins even when everything else is also broken.
        p.ci_status = CiStatus::Failure;
        p.is_draft = true;
        assert_eq!(p.status(NOW), PrStatus::WrongBase);
    }

    #[test]
    fn classifies_ci_fail_over_review_state() {
        let mut p = pr(2, "main");
        p.ci_status = CiStatus::Failure;
        p.review_decision = "APPROVED".to_string();
        assert_eq!(p.status(NOW), PrStatus::CiFail);
    }

    #[test]
    fn classifies_changes_requested_over_draft() {
        let mut p = pr(3, "main");
        p.review_decision = "CHANGES_REQUESTED".to_string();
        p.is_draft = true;
        assert_eq!(p.status(NOW), PrStatus::ChangesReq);
    }

    #[test]
    fn classifies_draft_over_stale() {
        let mut p = pr(4, "main");
        p.is_draft = true;
        p.updated_at = NOW - 60 * SECONDS_PER_DAY;
        assert_eq!(p.status(NOW), PrStatus::Draft);
    }

    #[test]
    fn classifies_stale_over_approved() {
        let mut p = pr(5, "main");
        p.review_decision = "APPROVED".to_string();
        p.updated_at = NOW - STALE_DAYS * SECONDS_PER_DAY;
        assert_eq!(p.status(NOW), PrStatus::Stale);
    }

    #[test]
    fn stale_boundary_is_inclusive() {
        let mut p = pr(6, "main");
        p.updated_at = NOW - (STALE_DAYS * SECONDS_PER_DAY - 1);
        assert_eq!(p.status(NOW), PrStatus::Ready, "one second short of stale");
        p.updated_at = NOW - STALE_DAYS * SECONDS_PER_DAY;
        assert_eq!(p.status(NOW), PrStatus::Stale);
    }

    #[test]
    fn classifies_approved_over_pending() {
        let mut p = pr(7, "main");
        p.review_decision = "APPROVED".to_string();
        p.ci_status = CiStatus::Pending;
        assert_eq!(p.status(NOW), PrStatus::Approved);
    }

    #[test]
    fn classifies_pending() {
        let mut p = pr(8, "main");
        p.ci_status = CiStatus::Pending;
        assert_eq!(p.status(NOW), PrStatus::Pending);
    }

    #[test]
    fn classifies_ready() {
        assert_eq!(pr(9, "main").status(NOW), PrStatus::Ready);
        // No checks at all is still reviewable.
        let mut p = pr(10, "main");
        p.ci_status = CiStatus::None;
        assert_eq!(p.status(NOW), PrStatus::Ready);
    }

    // ── Ordering ─────────────────────────────────────────────────────────────

    #[test]
    fn status_order_is_classification_order() {
        let mut order = [
            PrStatus::Ready,
            PrStatus::Pending,
            PrStatus::Approved,
            PrStatus::Stale,
            PrStatus::Draft,
            PrStatus::ChangesReq,
            PrStatus::CiFail,
            PrStatus::WrongBase,
        ];
        order.sort();
        assert_eq!(
            order.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            [
                "WRONG-BASE",
                "CI-FAIL",
                "CHANGES-REQ",
                "DRAFT",
                "STALE",
                "APPROVED",
                "PENDING",
                "READY",
            ]
        );
    }

    #[test]
    fn dashboard_sorts_by_status_then_age() {
        let mut ready_old = pr(1, "main");
        ready_old.updated_at = NOW - 3 * SECONDS_PER_DAY;
        let ready_new = pr(2, "main");
        let mut failing = pr(3, "main");
        failing.ci_status = CiStatus::Failure;
        let other_base = pr(4, "develop");

        let prs = vec![ready_old, ready_new, failing, other_base];
        let sorted = sort_prs(&prs, "main", NOW);

        // CI-FAIL first, then READY newest-first; the wrong-base PR is excluded.
        assert_eq!(
            sorted.iter().map(|p| p.number).collect::<Vec<_>>(),
            [3, 2, 1]
        );
    }

    #[test]
    fn summary_counts_every_bucket() {
        let mut failing = pr(1, "main");
        failing.ci_status = CiStatus::Failure;
        let prs = vec![failing, pr(2, "main"), pr(3, "main")];
        let sorted = sort_prs(&prs, "main", NOW);
        let counts = status_counts(&sorted, NOW);
        assert_eq!(counts.get(&PrStatus::Ready), Some(&2));
        assert_eq!(counts.get(&PrStatus::CiFail), Some(&1));
        assert!(summary_line(&counts, 1).contains("2 ready"));
        assert!(summary_line(&counts, 1).contains("1 wrong base"));
    }

    // ── CI rollup ────────────────────────────────────────────────────────────

    fn rollup(json: &str) -> Vec<Value> {
        serde_json::from_str(json).expect("fixture is valid JSON")
    }

    #[test]
    fn ci_none_when_no_checks() {
        assert_eq!(parse_ci(&[]), CiStatus::None);
    }

    #[test]
    fn ci_success_when_all_green() {
        let checks = rollup(
            r#"[
              {"__typename":"CheckRun","name":"test","status":"COMPLETED","conclusion":"SUCCESS"},
              {"__typename":"CheckRun","name":"clippy","status":"COMPLETED","conclusion":"SUCCESS"}
            ]"#,
        );
        assert_eq!(parse_ci(&checks), CiStatus::Success);
    }

    #[test]
    fn ci_failure_outranks_success_and_pending() {
        let checks = rollup(
            r#"[
              {"__typename":"CheckRun","name":"test","status":"COMPLETED","conclusion":"SUCCESS"},
              {"__typename":"CheckRun","name":"build","status":"IN_PROGRESS","conclusion":null},
              {"__typename":"CheckRun","name":"clippy","status":"COMPLETED","conclusion":"FAILURE"}
            ]"#,
        );
        assert_eq!(parse_ci(&checks), CiStatus::Failure);
    }

    #[test]
    fn ci_treats_every_bad_conclusion_as_failure() {
        for conclusion in CI_FAILURE_CONCLUSIONS {
            let checks = rollup(&format!(
                r#"[{{"name":"x","status":"COMPLETED","conclusion":"{conclusion}"}}]"#
            ));
            assert_eq!(parse_ci(&checks), CiStatus::Failure, "{conclusion}");
        }
    }

    #[test]
    fn ci_pending_outranks_success() {
        let checks = rollup(
            r#"[
              {"name":"test","status":"COMPLETED","conclusion":"SUCCESS"},
              {"name":"deploy","status":"QUEUED","conclusion":null}
            ]"#,
        );
        assert_eq!(parse_ci(&checks), CiStatus::Pending);
    }

    #[test]
    fn ci_none_when_checks_only_skipped() {
        let checks = rollup(r#"[{"name":"opt","status":"COMPLETED","conclusion":"SKIPPED"}]"#);
        assert_eq!(parse_ci(&checks), CiStatus::None);
    }

    // ── gh JSON parsing ──────────────────────────────────────────────────────

    /// Shape of `gh pr list --state open --limit 50 --json
    /// number,title,headRefName,baseRefName,author,isDraft,reviewDecision,
    /// statusCheckRollup,updatedAt`.
    const PR_LIST_FIXTURE: &str = r#"[
      {
        "number": 42,
        "title": "feat(export): live Neo4j push via --neo4j-push",
        "headRefName": "feat/neo4j-push",
        "baseRefName": "main",
        "author": {"id": "MDQ6VXNlcjE=", "is_bot": false, "login": "dqube", "name": "D Qube"},
        "isDraft": false,
        "reviewDecision": "APPROVED",
        "statusCheckRollup": [
          {"__typename":"CheckRun","name":"test","status":"COMPLETED","conclusion":"SUCCESS",
           "completedAt":"2026-08-08T10:02:11Z","startedAt":"2026-08-08T09:58:00Z",
           "detailsUrl":"https://github.com/o/r/actions/runs/1","workflowName":"CI"}
        ],
        "updatedAt": "2026-08-08T10:05:00Z"
      },
      {
        "number": 41,
        "title": "chore(deps): bump tree-sitter",
        "headRefName": "chore/bump-ts",
        "baseRefName": "develop",
        "author": {"id": "MDQ6VXNlcjI=", "is_bot": true, "login": "dependabot", "name": null},
        "isDraft": true,
        "reviewDecision": "",
        "statusCheckRollup": [
          {"__typename":"CheckRun","name":"test","status":"IN_PROGRESS","conclusion":null}
        ],
        "updatedAt": "2026-07-01T08:00:00Z"
      },
      {
        "number": 40,
        "title": "fix(extract): guard against empty files",
        "headRefName": "fix/empty-files",
        "baseRefName": "main",
        "author": null,
        "isDraft": false,
        "reviewDecision": "CHANGES_REQUESTED",
        "statusCheckRollup": [],
        "updatedAt": "2026-08-09T00:00:00Z"
      }
    ]"#;

    #[test]
    fn parses_gh_pr_list_fixture() {
        let prs = parse_pr_list(PR_LIST_FIXTURE, "main", NOW).unwrap();
        assert_eq!(prs.len(), 3);

        let approved = &prs[0];
        assert_eq!(approved.number, 42);
        assert_eq!(approved.branch, "feat/neo4j-push");
        assert_eq!(approved.author, "dqube");
        assert_eq!(approved.ci_status, CiStatus::Success);
        assert_eq!(approved.status(NOW), PrStatus::Approved);

        let dependabot = &prs[1];
        assert!(dependabot.is_draft);
        // Wrong base outranks both draft and stale.
        assert_eq!(dependabot.status(NOW), PrStatus::WrongBase);

        let anonymous = &prs[2];
        assert_eq!(anonymous.author, "?", "a null author falls back");
        assert_eq!(anonymous.ci_status, CiStatus::None);
        assert_eq!(anonymous.status(NOW), PrStatus::ChangesReq);
        assert_eq!(anonymous.days_old(NOW), 0);
    }

    #[test]
    fn parses_empty_pr_list() {
        assert!(parse_pr_list("[]", "main", NOW).unwrap().is_empty());
    }

    #[test]
    fn rejects_non_array_pr_list() {
        assert!(parse_pr_list(r#"{"message":"Not Found"}"#, "main", NOW).is_err());
    }

    // ── gh failure classification ────────────────────────────────────────────

    #[test]
    fn detects_missing_authentication() {
        let stderr = "To get started with GitHub CLI, please run:  gh auth login\n\
                      Alternatively, populate the GH_TOKEN environment variable.";
        assert_eq!(classify_gh_stderr(stderr), GhRejection::NeedsAuth);
        assert!(
            FetchFailure::from(CmdError::Failed(stderr.to_string()))
                .advice()
                .contains("gh auth login")
        );
    }

    #[test]
    fn detects_missing_repository_context() {
        let stderr = "failed to run git: fatal: not a git repository \
                      (or any of the parent directories): .git";
        assert_eq!(classify_gh_stderr(stderr), GhRejection::NoRepoContext);
        assert_eq!(
            FetchFailure::from(CmdError::Failed(stderr.to_string())),
            FetchFailure::NoRepoContext
        );
    }

    #[test]
    fn missing_cli_advice_names_the_installer() {
        let advice = FetchFailure::from(CmdError::Missing).advice();
        assert!(advice.contains("cli.github.com"));
        assert!(advice.contains("gh auth login"));
    }

    #[test]
    fn unknown_failure_reports_first_stderr_line() {
        let failure = FetchFailure::from(CmdError::Failed(
            "\nGraphQL: Resource not accessible\nsecond line".to_string(),
        ));
        let advice = failure.advice();
        assert!(advice.contains("Resource not accessible"));
        assert!(!advice.contains("second line"));
    }

    #[test]
    fn timeout_advice_names_the_limit() {
        assert!(
            FetchFailure::from(CmdError::TimedOut)
                .advice()
                .contains("30s")
        );
    }

    // ── Worktrees ────────────────────────────────────────────────────────────

    #[test]
    fn parses_worktree_porcelain() {
        let porcelain = "worktree /Users/x/repo\nHEAD abc123\nbranch refs/heads/main\n\n\
                         worktree /Users/x/repo-pr7\nHEAD def456\nbranch refs/heads/feat/seven\n\n";
        let map = parse_worktrees(porcelain);
        assert_eq!(map.get("main").map(String::as_str), Some("/Users/x/repo"));
        assert_eq!(
            map.get("feat/seven").map(String::as_str),
            Some("/Users/x/repo-pr7")
        );
    }

    #[test]
    fn detached_head_worktree_does_not_leak_its_successor() {
        // The middle record has no branch line; without the blank-line reset
        // its path would be attributed to the next record's branch.
        let porcelain = "worktree /a\nHEAD aaa\nbranch refs/heads/main\n\n\
                         worktree /detached\nHEAD bbb\ndetached\n\n\
                         worktree /c\nHEAD ccc\nbranch refs/heads/topic\n";
        let map = parse_worktrees(porcelain);
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("topic").map(String::as_str), Some("/c"));
    }

    #[test]
    fn parses_empty_worktree_output() {
        assert!(parse_worktrees("").is_empty());
    }

    // ── Path matching ────────────────────────────────────────────────────────

    #[test]
    fn path_match_is_boundary_safe() {
        assert!(path_match("a/b.rs", "b.rs"), "suffix at a path boundary");
        assert!(path_match("b.rs", "a/b.rs"), "match is symmetric");
        assert!(path_match("src/cmd_prs.rs", "src/cmd_prs.rs"), "identical");
        assert!(
            path_match("crates/graphify-core/src/lib.rs", "src/lib.rs"),
            "multi-segment suffix"
        );

        assert!(!path_match("ab.rs", "b.rs"), "not a path boundary");
        assert!(!path_match("b.rs", "ab.rs"), "and not in reverse either");
        assert!(!path_match("a/bb.rs", "b.rs"));
        assert!(!path_match("src/lib.rs", "src/main.rs"));
        assert!(!path_match("", "b.rs"));
    }

    #[test]
    fn basename_takes_the_last_segment() {
        assert_eq!(basename("a/b/c.rs"), "c.rs");
        assert_eq!(basename("c.rs"), "c.rs");
        assert_eq!(basename(""), "");
    }

    // ── Impact index ─────────────────────────────────────────────────────────

    fn test_index() -> ImpactIndex {
        ImpactIndex::from_nodes([
            ("src/cmd_prs.rs", Some(1), "cmd_prs"),
            ("src/cmd_prs.rs", Some(1), "PrsArgs"),
            ("src/cmd_prs.rs", Some(2), "classify"),
            ("src/main.rs", Some(3), "main"),
            ("crates/graphify-core/src/lib.rs", Some(4), "core"),
            // A node with no source file still names its community.
            ("", Some(4), "orphan concept"),
            // A node with no community still counts toward its file.
            ("src/main.rs", None, "helper"),
        ])
    }

    #[test]
    fn impact_sums_nodes_and_unions_communities() {
        let index = test_index();
        let (communities, nodes) = index.impact(&["src/cmd_prs.rs".to_string()]);
        assert_eq!(communities, vec![1, 2]);
        assert_eq!(nodes, 3);
    }

    #[test]
    fn impact_matches_on_path_suffix() {
        let index = test_index();
        // The diff reports a repo-relative path; the graph stored a longer one.
        let (communities, nodes) = index.impact(&["src/lib.rs".to_string()]);
        assert_eq!(communities, vec![4]);
        assert_eq!(nodes, 1);
    }

    #[test]
    fn impact_counts_each_graph_file_once() {
        let index = test_index();
        // Both spellings resolve to the same graph file.
        let (communities, nodes) =
            index.impact(&["src/cmd_prs.rs".to_string(), "cmd_prs.rs".to_string()]);
        assert_eq!(communities, vec![1, 2]);
        assert_eq!(nodes, 3, "the file is not double-counted");
    }

    #[test]
    fn impact_ignores_files_absent_from_the_graph() {
        let index = test_index();
        let (communities, nodes) = index.impact(&["README.md".to_string()]);
        assert!(communities.is_empty());
        assert_eq!(nodes, 0);
    }

    #[test]
    fn impact_does_not_match_a_partial_filename() {
        let index = ImpactIndex::from_nodes([("crates/ab.rs", Some(1), "ab")]);
        let (communities, nodes) = index.impact(&["b.rs".to_string()]);
        assert!(communities.is_empty(), "ab.rs is not b.rs");
        assert_eq!(nodes, 0);
    }

    #[test]
    fn index_samples_community_labels() {
        let index = test_index();
        assert_eq!(
            index.describe_community(1).as_deref(),
            Some("cmd_prs, PrsArgs")
        );
        assert_eq!(
            index.describe_community(4).as_deref(),
            Some("core, orphan concept")
        );
        assert!(index.describe_community(99).is_none());
    }

    #[test]
    fn index_caps_the_label_sample() {
        let index =
            ImpactIndex::from_nodes(["a", "b", "c", "d", "e", "f"].map(|l| ("f.rs", Some(0), l)));
        let described = index.describe_community(0).unwrap();
        assert_eq!(described.split(", ").count(), COMMUNITY_LABEL_SAMPLE);
    }

    #[test]
    fn blast_radius_pluralises() {
        let mut p = pr(1, "main");
        assert!(p.blast_radius().is_none(), "no impact, no phrase");
        p.nodes_affected = 1;
        p.communities_touched = vec![7];
        assert_eq!(p.blast_radius().as_deref(), Some("1 node / 1 community"));
        p.nodes_affected = 12;
        p.communities_touched = vec![1, 7];
        assert_eq!(
            p.blast_radius().as_deref(),
            Some("12 nodes / 2 communities")
        );
    }

    // ── Conflict grouping ────────────────────────────────────────────────────

    #[test]
    fn conflicts_report_shared_communities_most_contended_first() {
        let a: &[usize] = &[1, 2];
        let b: &[usize] = &[2, 3];
        let c: &[usize] = &[2, 3];
        let conflicts = find_conflicts([(10, a), (11, b), (12, c)]);
        assert_eq!(
            conflicts,
            vec![(2, vec![10, 11, 12]), (3, vec![11, 12])],
            "community 2 has three PRs so it leads; community 1 is unshared"
        );
    }

    #[test]
    fn conflicts_are_empty_when_nothing_overlaps() {
        let a: &[usize] = &[1];
        let b: &[usize] = &[2];
        assert!(find_conflicts([(1, a), (2, b)]).is_empty());
    }

    #[test]
    fn conflicts_are_empty_without_impact_data() {
        let none: &[usize] = &[];
        assert!(find_conflicts([(1, none), (2, none)]).is_empty());
    }

    // ── Timestamps ───────────────────────────────────────────────────────────

    #[test]
    fn parses_github_timestamps() {
        assert_eq!(parse_timestamp("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_timestamp("2026-08-09T00:00:00Z"), Some(NOW));
        // Fractional seconds and lowercase zone designators both appear in
        // GitHub payloads depending on the endpoint.
        assert_eq!(parse_timestamp("2026-08-09T00:00:00.123Z"), Some(NOW));
        assert_eq!(parse_timestamp("2026-08-09T00:00:00z"), Some(NOW));
        // An offset is normalised to UTC.
        assert_eq!(parse_timestamp("2026-08-09T02:00:00+02:00"), Some(NOW));
        assert_eq!(parse_timestamp("2026-08-08T22:00:00-02:00"), Some(NOW));
    }

    #[test]
    fn rejects_unparseable_timestamps() {
        for bad in [
            "",
            "2026-08-09",
            "not a date",
            "2026-13-01T00:00:00Z",
            "2026-08-32T00:00:00Z",
            "2026-08-09T25:00:00Z",
            "2026-08-09T00:61:00Z",
        ] {
            assert!(parse_timestamp(bad).is_none(), "{bad} should not parse");
        }
    }

    #[test]
    fn handles_leap_years() {
        // 2024 is a leap year, so Feb 29 exists and Mar 1 is one day later.
        let feb29 = parse_timestamp("2024-02-29T00:00:00Z").unwrap();
        let mar01 = parse_timestamp("2024-03-01T00:00:00Z").unwrap();
        assert_eq!(mar01 - feb29, SECONDS_PER_DAY);
        // 1900 was not a leap year despite being divisible by 4.
        let y1900 = parse_timestamp("1900-03-01T00:00:00Z").unwrap();
        let y1900_feb28 = parse_timestamp("1900-02-28T00:00:00Z").unwrap();
        assert_eq!(y1900 - y1900_feb28, SECONDS_PER_DAY);
    }

    #[test]
    fn ages_never_go_negative() {
        // A PR updated "in the future" by clock skew reads as today.
        assert_eq!(days_between(NOW + 10_000, NOW), 0);
        assert_eq!(days_between(NOW - 3 * SECONDS_PER_DAY, NOW), 3);
    }

    // ── Table formatting ─────────────────────────────────────────────────────

    #[test]
    fn visible_len_ignores_ansi_escapes() {
        assert_eq!(visible_len("READY"), 5);
        assert_eq!(visible_len("\u{1b}[32mREADY\u{1b}[0m"), 5);
        assert_eq!(visible_len("\u{1b}[1m\u{1b}[32mOK\u{1b}[0m"), 2);
    }

    #[test]
    fn pad_measures_visible_width() {
        assert_eq!(pad("ab", 5), "ab   ");
        assert_eq!(pad_left("ab", 5), "   ab");
        // A coloured cell keeps its escapes but still lands on the same column.
        let colored = "\u{1b}[32mab\u{1b}[0m";
        assert_eq!(visible_len(&pad(colored, 5)), 5);
        // Over-wide content is never truncated by padding.
        assert_eq!(pad("abcdef", 3), "abcdef");
    }

    #[test]
    fn truncate_is_character_safe() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 8), "hello w…");
        // Multi-byte characters must not be sliced mid-codepoint.
        assert_eq!(truncate("日本語のタイトル", 4), "日本語…");
        assert_eq!(truncate("🚀🚀🚀🚀", 2), "🚀…");
    }

    // ── Triage ───────────────────────────────────────────────────────────────

    #[test]
    fn triage_prompt_describes_every_candidate() {
        let mut approved = pr(1, "main");
        approved.review_decision = "APPROVED".to_string();
        approved.nodes_affected = 9;
        approved.communities_touched = vec![2, 5];
        let plain = pr(2, "main");
        let candidates = vec![&approved, &plain];

        let prompt = build_triage_prompt(&candidates, NOW);
        assert!(prompt.contains("rank them by review priority"));
        assert!(prompt.contains("PR #1 [APPROVED]"));
        assert!(prompt.contains("blast_radius=9 nodes / 2 communities"));
        assert!(prompt.contains("PR #2 [READY]"));
        assert!(prompt.contains("review=none"), "unreviewed reads as none");
    }

    #[test]
    fn triage_needs_a_configured_model() {
        assert!(resolve_llm_config(None, None).is_none());
    }

    #[test]
    fn triage_falls_back_to_the_environment_key() {
        let config = resolve_llm_config(None, Some("sk-ant-test".to_string()))
            .expect("an API key alone is enough");
        assert_eq!(config.model, ENV_FALLBACK_MODEL);
    }

    #[test]
    fn triage_prefers_the_config_block() {
        let llm = crate::config::LLMConfig {
            provider: Some("ollama".to_string()),
            model: Some("llama3".to_string()),
            ..Default::default()
        };
        let config = resolve_llm_config(Some(&llm), Some("sk-ant-test".to_string())).unwrap();
        assert_eq!(config.model, "llama3");
        assert!(config.api_key.is_none(), "ollama needs no key");
    }

    #[test]
    fn invalid_config_degrades_instead_of_failing() {
        let llm = crate::config::LLMConfig {
            provider: Some("not-a-provider".to_string()),
            model: Some("x".to_string()),
            ..Default::default()
        };
        assert!(resolve_llm_config(Some(&llm), None).is_none());
    }

    // ── Misc ─────────────────────────────────────────────────────────────────

    #[test]
    fn open_numbers_hint_lists_the_queue() {
        assert_eq!(open_numbers_hint(&[]), "");
        let prs = vec![pr(3, "main"), pr(7, "main")];
        assert_eq!(open_numbers_hint(&prs), " (open: #3, #7)");
    }
}
