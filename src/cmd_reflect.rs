//! Reflect command: aggregate save-result outcomes into LESSONS.md.
//!
//! `save-result` files one markdown doc per answered question into
//! `<output>/memory/`. On its own that pile is write-only — nobody re-reads
//! thirty Q&A docs at the start of a session. `reflect` folds them into a
//! single orientation artifact: which sources keep paying off, which questions
//! led nowhere, and what was corrected.
//!
//! Two properties are deliberate:
//!
//! * **Deterministic.** No LLM, stable sort orders, byte-identical output for
//!   the same inputs and the same `now`. A lessons file that churns on every
//!   run is noise in `git diff` and can't be committed.
//! * **Corroborated.** A source is only *preferred* once `min_corroboration`
//!   independent results have cited it usefully. One save must not be able to
//!   mint a trusted lesson, or the artifact just amplifies the first guess.
//!
//! Signals are scored rather than counted: each citation contributes a signed,
//! time-decayed value (useful positive, dead end / corrected negative), so a
//! fresh dead end outweighs a months-old success.
//!
//! The output lands in `<output>/reflections/LESSONS.md` rather than in
//! `wiki/`, because `export wiki` deletes every `wiki/*.md` on each run.
//!
//! ## Degradation
//!
//! Outcome signals come from `outcome` / `correction` frontmatter that
//! `save-result` does **not** write yet (see [`Outcome`]). Until it does, every
//! doc parses as *unmarked*: the summary counts still render, but no lesson can
//! be corroborated, and the command says so instead of pretending.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use colored::Colorize;

/// A signal's weight halves every 30 days, so stale verdicts fade rather than
/// competing forever with what was learned last week.
const HALF_LIFE_DAYS: f64 = 30.0;

/// Scores are rounded before comparison: `powf` can differ in the last ULP
/// across platforms, and an unstable sort order would break determinism.
const SCORE_SCALE: f64 = 1e9;

/// Bucket for docs whose cited nodes resolve to no community.
const UNCATEGORIZED: &str = "Uncategorized";

// --- parsed memory docs ---------------------------------------------------

/// How a saved answer turned out.
///
/// Not yet recorded by `save-result`; parsed here so the aggregation is ready
/// the moment the flags exist, and so docs written by the Python
/// implementation (which does record them) aggregate correctly today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Useful,
    DeadEnd,
    Corrected,
}

impl Outcome {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "useful" => Some(Outcome::Useful),
            "dead_end" | "dead-end" => Some(Outcome::DeadEnd),
            "corrected" => Some(Outcome::Corrected),
            _ => None,
        }
    }

    /// `+1` for a signal that endorses its sources, `-1` for one that warns off.
    fn sign(self) -> i32 {
        match self {
            Outcome::Useful => 1,
            Outcome::DeadEnd | Outcome::Corrected => -1,
        }
    }
}

/// One memory doc, reduced to the fields reflect reasons about.
#[derive(Debug, Clone, Default, PartialEq)]
struct MemoryDoc {
    /// File name, used only as a deterministic tiebreak.
    path: String,
    /// ISO-8601 UTC, or empty when the doc carries no usable stamp.
    date: String,
    /// Unix seconds parsed from `date`/`timestamp`; `None` disables decay.
    ts: Option<i64>,
    question: String,
    outcome: Option<Outcome>,
    correction: String,
    source_nodes: Vec<String>,
}

/// Split a doc's YAML-ish frontmatter into raw key/value pairs.
///
/// `save-result` hand-builds a tiny YAML subset rather than depending on a YAML
/// crate, so the same subset is parsed back by hand. A doc without a properly
/// terminated `---` block is not a memory doc and yields `None`, which is what
/// keeps foreign markdown in `memory/` from being half-read.
fn parse_frontmatter(text: &str) -> Option<Vec<(String, String)>> {
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))?;
    let mut fields = Vec::new();
    for line in rest.lines() {
        if line.trim() == "---" {
            return Some(fields);
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        if !key
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        {
            continue;
        }
        fields.push((key.to_string(), value.trim().to_string()));
    }
    None
}

/// Undo the escaping the frontmatter writer applies inside double quotes.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            // Leave anything we don't know about exactly as written.
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Strip the optional surrounding quotes from a scalar value.
///
/// Both forms occur: this crate writes `type: query`, the Python writer emits
/// `type: "query"`.
fn unquote(value: &str) -> String {
    let v = value.trim();
    match v.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        Some(inner) => unescape(inner),
        None => v.to_string(),
    }
}

/// Parse a flow list, in either the quoted (`["a", "b"]`) or bare (`[a, b]`)
/// form. Bare items cannot contain commas — see the note in the module docs of
/// `graphify-ingest::save_query_result`.
fn parse_list(value: &str) -> Vec<String> {
    let v = value.trim();
    let inner = v
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(v);
    if inner.contains('"') {
        return quoted_items(inner);
    }
    inner
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Every double-quoted item in a flow list, escapes resolved.
fn quoted_items(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_item = false;
    let mut escaped = false;
    for c in s.chars() {
        if !in_item {
            if c == '"' {
                in_item = true;
                cur.clear();
            }
            continue;
        }
        if escaped {
            cur.push('\\');
            cur.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '"' => {
                out.push(unescape(&cur));
                in_item = false;
            }
            _ => cur.push(c),
        }
    }
    out
}

/// Recover the question from the doc body.
///
/// `save-result` writes it under a `## Question` heading instead of into the
/// frontmatter, so without this every dead end would render as `""`.
fn question_from_body(text: &str) -> String {
    let heading = "\n## Question";
    let Some(start) = text.find(heading) else {
        return String::new();
    };
    let after = &text[start + heading.len()..];
    let body = match after.find("\n## ") {
        Some(i) => &after[..i],
        None => after,
    };
    body.trim().to_string()
}

/// Parse one memory doc, or `None` when it isn't one.
fn parse_memory_doc(text: &str, name: &str) -> Option<MemoryDoc> {
    let fields = parse_frontmatter(text)?;
    let mut doc = MemoryDoc {
        path: name.to_string(),
        ..Default::default()
    };
    let mut stamp: Option<i64> = None;
    for (key, value) in fields {
        match key.as_str() {
            "date" => doc.date = unquote(&value),
            "timestamp" => stamp = unquote(&value).parse().ok(),
            "question" => doc.question = unquote(&value),
            "correction" => doc.correction = unquote(&value),
            "outcome" => doc.outcome = Outcome::parse(&unquote(&value)),
            // `source_nodes` is the Python spelling, `nodes` this crate's.
            "source_nodes" | "nodes" => doc.source_nodes = parse_list(&value),
            _ => {}
        }
    }
    // An explicit ISO `date` wins; otherwise render the epoch stamp so docs from
    // both writers sort against each other lexicographically.
    if doc.date.is_empty()
        && let Some(ts) = stamp
    {
        doc.date = unix_to_iso(ts);
    }
    doc.ts = iso_to_unix(&doc.date).or(stamp);
    if doc.question.is_empty() {
        doc.question = question_from_body(text);
    }
    Some(doc)
}

/// Parse every memory doc under `memory_dir`, oldest first.
///
/// Unreadable files are skipped rather than fatal: one bad doc must not cost
/// the user their whole lessons file.
fn load_memory_docs(memory_dir: &Path) -> Vec<MemoryDoc> {
    let Ok(entries) = std::fs::read_dir(memory_dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("md")))
        .collect();
    paths.sort();

    let mut docs = Vec::new();
    for path in paths {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        if let Some(doc) = parse_memory_doc(&text, &name) {
            docs.push(doc);
        }
    }
    // (date, filename) ordering makes the aggregate order-independent of the
    // filesystem, which is what makes the rendered output reproducible.
    docs.sort_by(|a, b| a.date.cmp(&b.date).then_with(|| a.path.cmp(&b.path)));
    docs
}

// --- calendar helpers -----------------------------------------------------
//
// Howard Hinnant's civil-calendar algorithms, transcribed. Two conversions do
// not justify a `chrono` dependency in the CLI crate.

/// Days since the Unix epoch for a proleptic-Gregorian date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400; // [0, 399]
    let mp = (m + 9) % 12; // March = 0 … February = 11
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The `(year, month, day)` for a count of days since the Unix epoch.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Render Unix seconds as an ISO-8601 UTC instant.
fn unix_to_iso(ts: i64) -> String {
    let (y, m, d) = civil_from_days(ts.div_euclid(86_400));
    let rem = ts.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}+00:00",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Parse an ISO-8601 date or datetime to Unix seconds.
///
/// Accepts `YYYY-MM-DD`, an optional `T`/space-separated time, optional
/// fractional seconds, and an optional `Z` or `±HH[:]MM` offset. A naive
/// timestamp is read as UTC, matching the Python implementation.
fn iso_to_unix(s: &str) -> Option<i64> {
    let s = s.trim();
    // Byte slicing below is only sound for ASCII, and no valid stamp is not.
    if !s.is_ascii() || s.len() < 10 {
        return None;
    }
    let b = s.as_bytes();
    if b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let year: i64 = s[0..4].parse().ok()?;
    let month: i64 = s[5..7].parse().ok()?;
    let day: i64 = s[8..10].parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut secs = days_from_civil(year, month, day) * 86_400;

    let rest = &s[10..];
    if rest.is_empty() {
        return Some(secs);
    }
    let rest = rest
        .strip_prefix('T')
        .or_else(|| rest.strip_prefix('t'))
        .or_else(|| rest.strip_prefix(' '))?;
    if rest.len() < 5 || rest.as_bytes()[2] != b':' {
        return None;
    }
    secs += rest[0..2].parse::<i64>().ok()? * 3600 + rest[3..5].parse::<i64>().ok()? * 60;

    let mut tail = &rest[5..];
    if let Some(t) = tail.strip_prefix(':')
        && t.len() >= 2
    {
        secs += t[0..2].parse::<i64>().ok()?;
        tail = &t[2..];
    }
    if let Some(t) = tail.strip_prefix('.') {
        let digits = t.bytes().take_while(u8::is_ascii_digit).count();
        tail = &t[digits..];
    }

    let tail = tail.trim();
    if tail.is_empty() || tail.eq_ignore_ascii_case("z") {
        return Some(secs);
    }
    let (sign, off) = match tail.as_bytes()[0] {
        b'+' => (1i64, &tail[1..]),
        b'-' => (-1i64, &tail[1..]),
        // An unrecognised suffix is not worth discarding the date over.
        _ => return Some(secs),
    };
    if off.len() < 2 {
        return Some(secs);
    }
    let hours: i64 = off[0..2].parse().ok()?;
    let minutes: i64 = if off.len() >= 5 && off.as_bytes()[2] == b':' {
        off[3..5].parse().ok()?
    } else if off.len() >= 4 {
        off[2..4].parse().ok()?
    } else {
        0
    };
    Some(secs - sign * (hours * 3600 + minutes * 60))
}

// --- graph context --------------------------------------------------------

/// What the current graph knows, used to age lessons out and group them.
///
/// Optional: reflect runs fine before a build, it just produces one flat
/// section and keeps every citation.
struct GraphLens {
    /// Node ids *and* labels. `save-result` cites nodes by the label an agent
    /// saw (`build_from_json()`), while the graph is keyed by id
    /// (`module_build_from_json`) — indexing only ids would drop every
    /// label-form citation, which is the common case.
    known: HashSet<String>,
    /// Node id or label -> community label.
    community: HashMap<String, String>,
}

impl GraphLens {
    /// Load `graph.json`, or `None` when it is missing or unreadable.
    fn load(graph_path: &Path) -> Option<Self> {
        if !graph_path.exists() {
            return None;
        }
        let graph = graphify_serve::load_graph(graph_path).ok()?;

        let mut known = HashSet::new();
        for node in graph.nodes() {
            known.insert(node.id.clone());
            known.insert(node.label.clone());
        }
        if known.is_empty() {
            return None;
        }

        // Ascending community id + first-write-wins makes a label shared by two
        // communities resolve the same way on every run.
        let mut communities: Vec<_> = graph.communities.iter().collect();
        communities.sort_by_key(|c| c.id);
        let mut community: HashMap<String, String> = HashMap::new();
        for info in communities {
            let label = info
                .label
                .clone()
                .unwrap_or_else(|| format!("Community {}", info.id));
            for id in &info.nodes {
                community.entry(id.clone()).or_insert_with(|| label.clone());
                if let Some(node) = graph.get_node(id) {
                    community
                        .entry(node.label.clone())
                        .or_insert_with(|| label.clone());
                }
            }
        }
        Some(Self { known, community })
    }
}

/// The community a doc belongs to: the plurality community of its sources.
///
/// Ties break to the lexicographically smallest label so the grouping does not
/// depend on the order nodes were cited in.
fn doc_community(nodes: &[String], lens: Option<&GraphLens>) -> String {
    let Some(lens) = lens else {
        return UNCATEGORIZED.to_string();
    };
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for node in nodes {
        if let Some(label) = lens.community.get(node) {
            *counts.entry(label.as_str()).or_default() += 1;
        }
    }
    let mut best: Option<(&str, usize)> = None;
    for (label, count) in counts {
        if best.is_none_or(|(_, seen)| count > seen) {
            best = Some((label, count));
        }
    }
    best.map_or_else(|| UNCATEGORIZED.to_string(), |(label, _)| label.to_string())
}

// --- aggregation ----------------------------------------------------------

/// Outcome tallies over a set of docs.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Counts {
    useful: usize,
    dead_end: usize,
    corrected: usize,
    unmarked: usize,
}

impl Counts {
    /// Docs carrying any outcome signal at all.
    fn marked(self) -> usize {
        self.useful + self.dead_end + self.corrected
    }
}

/// Running per-node tallies while a bucket is being filled.
#[derive(Debug, Default, Clone)]
struct NodeStat {
    score: f64,
    pos: usize,
    neg: usize,
    /// Most recent event date, for the contested verdict line.
    last: String,
}

/// Accumulator for one scope (the whole corpus, or one community).
#[derive(Debug, Default)]
struct Bucket {
    counts: Counts,
    nodes: BTreeMap<String, NodeStat>,
    dead_ends: Vec<Finding>,
    corrections: Vec<Finding>,
}

/// A question-scoped lesson: a dead end, or a correction.
#[derive(Debug, Clone, Default, PartialEq)]
struct Finding {
    question: String,
    date: String,
    /// Sources cited by a dead end; empty for corrections.
    nodes: Vec<String>,
    /// What the right answer was; empty for dead ends.
    correction: String,
}

/// A source with only positive signals.
#[derive(Debug, Clone, PartialEq)]
struct Scored {
    node: String,
    /// Distinct results that found it useful.
    n: usize,
    score: f64,
}

/// A source with signals pointing both ways.
#[derive(Debug, Clone, PartialEq)]
struct Contested {
    node: String,
    pos: usize,
    neg: usize,
    score: f64,
    verdict: Verdict,
    last: String,
}

/// Which way the time-decayed score tips a contested source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Useful,
    DeadEnd,
    Even,
}

/// The five lesson lists rendered for one scope.
#[derive(Debug, Default, PartialEq)]
struct Section {
    counts: Counts,
    preferred: Vec<Scored>,
    tentative: Vec<Scored>,
    contested: Vec<Contested>,
    dead_ends: Vec<Finding>,
    corrections: Vec<Finding>,
}

impl Section {
    fn is_empty(&self) -> bool {
        self.preferred.is_empty()
            && self.tentative.is_empty()
            && self.contested.is_empty()
            && self.dead_ends.is_empty()
            && self.corrections.is_empty()
    }
}

/// Everything the lessons doc is rendered from.
#[derive(Debug)]
struct Aggregate {
    total: usize,
    min_corroboration: usize,
    overall: Section,
    /// Empty unless a graph supplied community labels.
    by_community: Vec<(String, Section)>,
}

/// Time-decay weight in `(0, 1]`, halving every `half_life_days`.
///
/// Undated signals keep full weight; future-dated ones are clamped to age zero
/// rather than being allowed to out-weigh the present.
fn decay(ts: Option<i64>, now: i64, half_life_days: f64) -> f64 {
    let Some(ts) = ts else {
        return 1.0;
    };
    if half_life_days <= 0.0 {
        return 1.0;
    }
    let age_days = (now - ts).max(0) as f64 / 86_400.0;
    0.5f64.powf(age_days / half_life_days)
}

/// Round to [`SCORE_SCALE`] so sort order is identical across platforms.
fn round_score(score: f64) -> f64 {
    (score * SCORE_SCALE).round() / SCORE_SCALE
}

fn record(bucket: &mut Bucket, node: &str, sign: i32, weight: f64, date: &str) {
    let stat = bucket.nodes.entry(node.to_string()).or_default();
    stat.score += f64::from(sign) * weight;
    if sign > 0 {
        stat.pos += 1;
    } else if sign < 0 {
        stat.neg += 1;
    }
    if date > stat.last.as_str() {
        stat.last = date.to_string();
    }
}

/// Collapse repeated questions to their most recent entry.
///
/// Docs arrive oldest-first, so the last write wins — saving the same Q&A twice
/// must not duplicate a line, and a re-correction should replace the old text.
fn dedupe_by_question(items: Vec<Finding>) -> Vec<Finding> {
    let mut latest: BTreeMap<String, Finding> = BTreeMap::new();
    for item in items {
        latest.insert(item.question.clone(), item);
    }
    let mut out: Vec<Finding> = latest.into_values().collect();
    out.sort_by(|a, b| {
        a.date
            .cmp(&b.date)
            .then_with(|| a.question.cmp(&b.question))
    });
    out
}

/// Split a filled bucket into the rendered lists.
///
/// Sources with only negative signals are intentionally dropped here: they are
/// already named by the dead-end questions that produced them, and listing them
/// twice reads as two separate warnings.
fn finalize(bucket: Bucket, min_corroboration: usize) -> Section {
    let mut preferred = Vec::new();
    let mut tentative = Vec::new();
    let mut contested = Vec::new();

    for (node, stat) in bucket.nodes {
        let score = round_score(stat.score);
        if stat.pos > 0 && stat.neg > 0 {
            let verdict = if score > 0.0 {
                Verdict::Useful
            } else if score < 0.0 {
                Verdict::DeadEnd
            } else {
                Verdict::Even
            };
            contested.push(Contested {
                node,
                pos: stat.pos,
                neg: stat.neg,
                score,
                verdict,
                last: stat.last,
            });
        } else if stat.pos > 0 {
            let entry = Scored {
                node,
                n: stat.pos,
                score,
            };
            if stat.pos >= min_corroboration {
                preferred.push(entry);
            } else {
                tentative.push(entry);
            }
        }
    }

    let by_score = |a: &Scored, b: &Scored| b.score.total_cmp(&a.score).then(a.node.cmp(&b.node));
    preferred.sort_by(by_score);
    tentative.sort_by(by_score);
    contested.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.node.cmp(&b.node)));

    Section {
        counts: bucket.counts,
        preferred,
        tentative,
        contested,
        dead_ends: dedupe_by_question(bucket.dead_ends),
        corrections: dedupe_by_question(bucket.corrections),
    }
}

/// Fold parsed docs into the deterministic lessons structure.
///
/// `now` is a parameter so callers (and tests) can anchor the decay and get
/// byte-stable output.
fn aggregate(
    docs: &[MemoryDoc],
    lens: Option<&GraphLens>,
    now: i64,
    min_corroboration: usize,
) -> Aggregate {
    let mut overall = Bucket::default();
    let mut by_community: BTreeMap<String, Bucket> = BTreeMap::new();

    for doc in docs {
        // One event per node per doc, and drop citations the graph has lost:
        // a lesson pointing at deleted code is worse than no lesson.
        let mut nodes: Vec<String> = Vec::new();
        for node in &doc.source_nodes {
            if lens.is_some_and(|l| !l.known.contains(node)) {
                continue;
            }
            if !nodes.iter().any(|seen| seen == node) {
                nodes.push(node.clone());
            }
        }

        let community = doc_community(&nodes, lens);
        let bucket = by_community.entry(community).or_default();
        let sign = doc.outcome.map_or(0, Outcome::sign);
        let weight = if sign == 0 {
            0.0
        } else {
            decay(doc.ts, now, HALF_LIFE_DAYS)
        };

        for target in [&mut overall, bucket] {
            match doc.outcome {
                Some(Outcome::Useful) => target.counts.useful += 1,
                Some(Outcome::DeadEnd) => target.counts.dead_end += 1,
                Some(Outcome::Corrected) => target.counts.corrected += 1,
                None => target.counts.unmarked += 1,
            }
            if sign != 0 {
                for node in &nodes {
                    record(target, node, sign, weight, &doc.date);
                }
            }
            match doc.outcome {
                Some(Outcome::DeadEnd) => target.dead_ends.push(Finding {
                    question: doc.question.clone(),
                    date: doc.date.clone(),
                    nodes: nodes.clone(),
                    correction: String::new(),
                }),
                Some(Outcome::Corrected) => target.corrections.push(Finding {
                    question: doc.question.clone(),
                    date: doc.date.clone(),
                    nodes: Vec::new(),
                    correction: doc.correction.clone(),
                }),
                _ => {}
            }
        }
    }

    // Without a graph every doc lands in Uncategorized, and the "By topic"
    // section would just repeat the flat one.
    let grouped = if lens.is_some_and(|l| !l.community.is_empty()) {
        by_community
            .into_iter()
            .map(|(label, bucket)| (label, finalize(bucket, min_corroboration)))
            .collect()
    } else {
        Vec::new()
    };

    Aggregate {
        total: docs.len(),
        min_corroboration,
        overall: finalize(overall, min_corroboration),
        by_community: grouped,
    }
}

// --- rendering ------------------------------------------------------------

fn render_section(out: &mut Vec<String>, section: &Section, k: usize) {
    if section.is_empty() {
        out.push("_No marked outcomes yet._".into());
        out.push(String::new());
        return;
    }
    if !section.preferred.is_empty() {
        out.push(format!(
            "**Preferred sources** — corroborated by ≥{k} useful results; start here."
        ));
        out.push(String::new());
        for e in &section.preferred {
            out.push(format!("- `{}` ({}× useful)", e.node, e.n));
        }
        out.push(String::new());
    }
    if !section.tentative.is_empty() {
        out.push(format!(
            "**Tentative** — useful in fewer than {k} results; verify before relying."
        ));
        out.push(String::new());
        for e in &section.tentative {
            out.push(format!("- `{}` ({}× useful)", e.node, e.n));
        }
        out.push(String::new());
    }
    if !section.contested.is_empty() {
        out.push("**Contested** — mixed signals; recency decides.".into());
        out.push(String::new());
        for e in &section.contested {
            let verdict = match e.verdict {
                Verdict::Even => "evenly split".to_string(),
                Verdict::Useful => "recency leans **useful**".to_string(),
                Verdict::DeadEnd => "recency leans **dead end**".to_string(),
            };
            let day = e.last.get(..10).unwrap_or_default();
            let latest = if day.is_empty() {
                String::new()
            } else {
                format!(" (latest {day})")
            };
            out.push(format!(
                "- `{}` — {}× useful, {}× dead end/corrected → {verdict}{latest}",
                e.node, e.pos, e.neg
            ));
        }
        out.push(String::new());
    }
    if !section.dead_ends.is_empty() {
        out.push("**Known dead ends** — led nowhere; don't re-derive.".into());
        out.push(String::new());
        for d in &section.dead_ends {
            let nodes = d
                .nodes
                .iter()
                .map(|n| format!("`{n}`"))
                .collect::<Vec<_>>()
                .join(", ");
            let tail = if nodes.is_empty() {
                String::new()
            } else {
                format!(" — {nodes}")
            };
            out.push(format!("- \"{}\"{tail}", d.question));
        }
        out.push(String::new());
    }
    if !section.corrections.is_empty() {
        out.push("**Corrections** — do these differently.".into());
        out.push(String::new());
        for c in &section.corrections {
            out.push(format!("- \"{}\" → {}", c.question, c.correction));
        }
        out.push(String::new());
    }
}

/// Uncategorized sorts last; everything else alphabetically.
fn topic_key(label: &str) -> (u8, &str) {
    (u8::from(label == UNCATEGORIZED), label)
}

fn render_lessons(agg: &Aggregate, memory_label: &str) -> String {
    let c = agg.overall.counts;
    let k = agg.min_corroboration;
    let plural = if agg.total == 1 { "memory" } else { "memories" };
    let mut out: Vec<String> = vec![
        "# Lessons".into(),
        String::new(),
        format!(
            "_Auto-generated by `graphify-rs reflect` from {} session {plural} in {memory_label}. \
             Deterministic; no LLM. Use for orientation — verify before relying, and revisit dead \
             ends if the code has changed since._",
            agg.total
        ),
        String::new(),
        "## Summary".into(),
        String::new(),
        format!(
            "- {} useful · {} dead ends · {} corrected · {} unmarked",
            c.useful, c.dead_end, c.corrected, c.unmarked
        ),
        String::new(),
        "## Lessons".into(),
        String::new(),
    ];
    render_section(&mut out, &agg.overall, k);

    if !agg.by_community.is_empty() {
        out.push("## By topic".into());
        out.push(String::new());
        let mut topics: Vec<&(String, Section)> = agg.by_community.iter().collect();
        topics.sort_by(|a, b| topic_key(&a.0).cmp(&topic_key(&b.0)));
        for (label, section) in topics {
            out.push(format!("### {label}"));
            out.push(String::new());
            render_section(&mut out, section, k);
        }
    }

    let mut body = out.join("\n");
    while body.ends_with('\n') {
        body.pop();
    }
    body.push('\n');
    body
}

// --- command --------------------------------------------------------------

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Summarize recorded query outcomes into a lessons file under `output_dir`.
pub fn cmd_reflect(output_dir: &str, min_corroboration: usize) -> Result<()> {
    let root = Path::new(output_dir);
    let memory_dir = root.join("memory");
    let docs = load_memory_docs(&memory_dir);

    if docs.is_empty() {
        println!(
            "\n  {} no session memories found in {}",
            "ℹ".blue(),
            memory_dir.display()
        );
        println!(
            "    Record one with: {}\n",
            "graphify-rs save-result --question \"…\" --answer \"…\" --nodes A --nodes B".dimmed()
        );
        return Ok(());
    }

    let lens = GraphLens::load(&root.join("graph.json"));
    let agg = aggregate(&docs, lens.as_ref(), now_unix(), min_corroboration);

    let out_path = root.join("reflections").join("LESSONS.md");
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let memory_label = memory_dir.display().to_string();
    std::fs::write(&out_path, render_lessons(&agg, &memory_label))
        .with_context(|| format!("failed to write {}", out_path.display()))?;

    let c = agg.overall.counts;
    println!("\n  {} {}", "Lessons:".dimmed(), out_path.display());
    println!(
        "    {} {} · {} useful · {} dead ends · {} corrected · {} unmarked",
        agg.total.to_string().bold(),
        if agg.total == 1 { "memory" } else { "memories" },
        c.useful,
        c.dead_end,
        c.corrected,
        c.unmarked
    );
    println!(
        "    {} preferred · {} tentative · {} contested (corroboration ≥{})",
        agg.overall.preferred.len(),
        agg.overall.tentative.len(),
        agg.overall.contested.len(),
        agg.min_corroboration
    );
    if lens.is_none() {
        println!(
            "    {} no graph.json — lessons are one flat section and stale sources aren't pruned",
            "ℹ".blue()
        );
    }
    if c.marked() == 0 {
        // The honest failure mode: everything parsed, nothing could be learned.
        println!(
            "\n  {} every memory is unmarked, so no lesson could be corroborated.",
            "⚠".yellow()
        );
        println!(
            "    {}",
            "`save-result` does not record outcomes yet; LESSONS.md carries the summary only."
                .dimmed()
        );
    }
    println!();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2024-01-01T00:00:00Z — the anchor every dated fixture is relative to.
    const T0: i64 = 1_704_067_200;
    const DAY: i64 = 86_400;

    fn doc(name: &str, outcome: Option<Outcome>, ts: i64, nodes: &[&str]) -> MemoryDoc {
        MemoryDoc {
            path: name.to_string(),
            date: unix_to_iso(ts),
            ts: Some(ts),
            question: format!("q for {name}"),
            outcome,
            correction: String::new(),
            source_nodes: nodes.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    fn names(entries: &[Scored]) -> Vec<&str> {
        entries.iter().map(|e| e.node.as_str()).collect()
    }

    // --- frontmatter parsing ---

    #[test]
    fn parses_the_doc_save_result_actually_writes() {
        // Byte-for-byte the shape of graphify_ingest::save_query_result.
        let text = "---\ntype: query\ntimestamp: 1704067200\nnodes: [alpha, beta]\n---\n\n\
                    ## Question\n\nHow does clustering work?\n\n## Answer\n\nLouvain.\n";
        let d = parse_memory_doc(text, "query_1704067200.md").unwrap();
        assert_eq!(d.source_nodes, vec!["alpha", "beta"]);
        assert_eq!(d.question, "How does clustering work?");
        assert_eq!(d.ts, Some(T0));
        assert_eq!(d.date, "2024-01-01T00:00:00+00:00");
        // Nothing records outcomes yet, so it must parse as unmarked.
        assert_eq!(d.outcome, None);
    }

    #[test]
    fn parses_the_python_writers_quoted_form() {
        let text = "---\ntype: \"query\"\ndate: \"2024-01-02T03:04:05+00:00\"\n\
                    question: \"Where is the \\\"root\\\" resolved?\"\noutcome: \"corrected\"\n\
                    correction: \"it is paths.rs\"\nsource_nodes: [\"a b\", \"c,d\"]\n---\n\nbody\n";
        let d = parse_memory_doc(text, "x.md").unwrap();
        assert_eq!(d.question, "Where is the \"root\" resolved?");
        assert_eq!(d.outcome, Some(Outcome::Corrected));
        assert_eq!(d.correction, "it is paths.rs");
        // The quoted form survives a comma inside an item; the bare form cannot.
        assert_eq!(d.source_nodes, vec!["a b", "c,d"]);
        assert_eq!(d.ts, Some(T0 + DAY + 3 * 3600 + 4 * 60 + 5));
    }

    #[test]
    fn rejects_docs_without_frontmatter() {
        assert!(parse_memory_doc("# Lessons\n\nno frontmatter\n", "LESSONS.md").is_none());
        // Unterminated frontmatter is not a memory doc either.
        assert!(parse_memory_doc("---\ntype: query\n\nbody\n", "half.md").is_none());
    }

    #[test]
    fn empty_node_list_parses_to_nothing() {
        let text = "---\ntype: query\ntimestamp: 1\nnodes: []\n---\n\n## Question\n\nq\n";
        assert!(
            parse_memory_doc(text, "a.md")
                .unwrap()
                .source_nodes
                .is_empty()
        );
    }

    // --- calendar ---

    #[test]
    fn iso_round_trips() {
        for ts in [0, 1, T0, T0 + 12_345_678, 2_000_000_000] {
            assert_eq!(iso_to_unix(&unix_to_iso(ts)), Some(ts), "ts={ts}");
        }
    }

    #[test]
    fn iso_accepts_the_forms_that_show_up_in_frontmatter() {
        assert_eq!(iso_to_unix("2024-01-01"), Some(T0));
        assert_eq!(iso_to_unix("2024-01-01T00:00:00Z"), Some(T0));
        assert_eq!(iso_to_unix("2024-01-01 00:00"), Some(T0));
        assert_eq!(iso_to_unix("2024-01-01T00:00:00.123456+00:00"), Some(T0));
        // A +01:00 wall clock is one hour *earlier* in UTC.
        assert_eq!(iso_to_unix("2024-01-01T01:00:00+01:00"), Some(T0));
        assert_eq!(iso_to_unix("2024-01-01T01:00:00+0100"), Some(T0));
    }

    #[test]
    fn iso_rejects_junk() {
        assert_eq!(iso_to_unix(""), None);
        assert_eq!(iso_to_unix("not-a-date"), None);
        assert_eq!(iso_to_unix("2024-13-01"), None);
        assert_eq!(iso_to_unix("2024/01/01"), None);
    }

    // --- corroboration threshold ---

    #[test]
    fn a_single_useful_result_is_tentative_not_preferred() {
        let docs = vec![doc("a.md", Some(Outcome::Useful), T0, &["graph.rs"])];
        let agg = aggregate(&docs, None, T0, 2);
        assert!(agg.overall.preferred.is_empty());
        assert_eq!(names(&agg.overall.tentative), vec!["graph.rs"]);
    }

    #[test]
    fn independent_results_promote_a_source() {
        let docs = vec![
            doc("a.md", Some(Outcome::Useful), T0, &["graph.rs"]),
            doc("b.md", Some(Outcome::Useful), T0 + DAY, &["graph.rs"]),
        ];
        let agg = aggregate(&docs, None, T0 + DAY, 2);
        assert_eq!(names(&agg.overall.preferred), vec!["graph.rs"]);
        assert_eq!(agg.overall.preferred[0].n, 2);
        assert!(agg.overall.tentative.is_empty());
    }

    #[test]
    fn one_doc_citing_a_node_twice_is_still_one_result() {
        // Otherwise a single save could corroborate itself into "preferred".
        let docs = vec![doc(
            "a.md",
            Some(Outcome::Useful),
            T0,
            &["graph.rs", "graph.rs"],
        )];
        let agg = aggregate(&docs, None, T0, 2);
        assert!(agg.overall.preferred.is_empty());
        assert_eq!(agg.overall.tentative.len(), 1);
        assert_eq!(agg.overall.tentative[0].n, 1);
    }

    #[test]
    fn raising_the_threshold_demotes_sources() {
        let docs = vec![
            doc("a.md", Some(Outcome::Useful), T0, &["graph.rs"]),
            doc("b.md", Some(Outcome::Useful), T0, &["graph.rs"]),
        ];
        assert_eq!(aggregate(&docs, None, T0, 2).overall.preferred.len(), 1);
        assert_eq!(aggregate(&docs, None, T0, 3).overall.preferred.len(), 0);
        assert_eq!(aggregate(&docs, None, T0, 3).overall.tentative.len(), 1);
    }

    // --- mixed signals ---

    #[test]
    fn mixed_signals_are_contested_and_recency_decides() {
        let docs = vec![
            doc("old.md", Some(Outcome::Useful), T0, &["parser.rs"]),
            // 120 days later: four half-lives, so the dead end dominates.
            doc(
                "new.md",
                Some(Outcome::DeadEnd),
                T0 + 120 * DAY,
                &["parser.rs"],
            ),
        ];
        let agg = aggregate(&docs, None, T0 + 120 * DAY, 2);
        assert!(agg.overall.preferred.is_empty());
        assert_eq!(agg.overall.contested.len(), 1);
        let c = &agg.overall.contested[0];
        assert_eq!((c.pos, c.neg), (1, 1));
        assert_eq!(c.verdict, Verdict::DeadEnd);
    }

    #[test]
    fn a_stale_dead_end_loses_to_a_fresh_success() {
        let docs = vec![
            doc("old.md", Some(Outcome::DeadEnd), T0, &["parser.rs"]),
            doc(
                "new.md",
                Some(Outcome::Useful),
                T0 + 120 * DAY,
                &["parser.rs"],
            ),
        ];
        let agg = aggregate(&docs, None, T0 + 120 * DAY, 2);
        assert_eq!(agg.overall.contested[0].verdict, Verdict::Useful);
    }

    #[test]
    fn negative_only_sources_are_left_to_the_dead_end_list() {
        let docs = vec![doc("a.md", Some(Outcome::DeadEnd), T0, &["ghost.rs"])];
        let agg = aggregate(&docs, None, T0, 2);
        assert!(agg.overall.preferred.is_empty());
        assert!(agg.overall.tentative.is_empty());
        assert!(agg.overall.contested.is_empty());
        assert_eq!(agg.overall.dead_ends.len(), 1);
        assert_eq!(agg.overall.dead_ends[0].nodes, vec!["ghost.rs"]);
    }

    #[test]
    fn repeated_questions_collapse_to_the_latest() {
        let mut first = doc("a.md", Some(Outcome::Corrected), T0, &[]);
        first.question = "same question".into();
        first.correction = "old answer".into();
        let mut second = doc("b.md", Some(Outcome::Corrected), T0 + DAY, &[]);
        second.question = "same question".into();
        second.correction = "new answer".into();

        let agg = aggregate(&[first, second], None, T0 + DAY, 2);
        assert_eq!(agg.overall.corrections.len(), 1);
        assert_eq!(agg.overall.corrections[0].correction, "new answer");
        assert_eq!(agg.overall.counts.corrected, 2);
    }

    // --- counts and degradation ---

    #[test]
    fn unmarked_docs_count_but_teach_nothing() {
        let docs = vec![
            doc("a.md", None, T0, &["graph.rs"]),
            doc("b.md", None, T0, &["graph.rs"]),
            doc("c.md", None, T0, &["graph.rs"]),
        ];
        let agg = aggregate(&docs, None, T0, 2);
        assert_eq!(agg.total, 3);
        assert_eq!(agg.overall.counts.unmarked, 3);
        assert_eq!(agg.overall.counts.marked(), 0);
        assert!(agg.overall.is_empty());
        let md = render_lessons(&agg, "out/memory");
        assert!(md.contains("- 0 useful · 0 dead ends · 0 corrected · 3 unmarked"));
        assert!(md.contains("_No marked outcomes yet._"));
    }

    #[test]
    fn counts_cover_every_outcome() {
        let docs = vec![
            doc("a.md", Some(Outcome::Useful), T0, &["x"]),
            doc("b.md", Some(Outcome::DeadEnd), T0, &["y"]),
            doc("c.md", Some(Outcome::Corrected), T0, &["z"]),
            doc("d.md", None, T0, &[]),
        ];
        let agg = aggregate(&docs, None, T0, 2);
        assert_eq!(
            agg.overall.counts,
            Counts {
                useful: 1,
                dead_end: 1,
                corrected: 1,
                unmarked: 1
            }
        );
    }

    // --- graph awareness ---

    #[test]
    fn citations_the_graph_no_longer_knows_are_dropped() {
        let lens = GraphLens {
            known: ["kept.rs".to_string()].into_iter().collect(),
            community: HashMap::new(),
        };
        let docs = vec![
            doc(
                "a.md",
                Some(Outcome::Useful),
                T0,
                &["kept.rs", "deleted.rs"],
            ),
            doc(
                "b.md",
                Some(Outcome::Useful),
                T0,
                &["kept.rs", "deleted.rs"],
            ),
        ];
        let agg = aggregate(&docs, Some(&lens), T0, 2);
        assert_eq!(names(&agg.overall.preferred), vec!["kept.rs"]);
        assert!(agg.overall.tentative.is_empty());
    }

    #[test]
    fn lessons_group_by_the_plurality_community() {
        let lens = GraphLens {
            known: ["a", "b", "c"].iter().map(|s| (*s).to_string()).collect(),
            community: [("a", "Export"), ("b", "Export"), ("c", "Cluster")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        };
        let docs = vec![
            doc("1.md", Some(Outcome::Useful), T0, &["a", "b", "c"]),
            doc("2.md", Some(Outcome::Useful), T0 + DAY, &["a", "b", "c"]),
        ];
        let agg = aggregate(&docs, Some(&lens), T0 + DAY, 2);
        assert_eq!(agg.by_community.len(), 1);
        assert_eq!(agg.by_community[0].0, "Export");
        assert!(render_lessons(&agg, "m").contains("### Export"));
    }

    #[test]
    fn grouping_is_skipped_without_a_graph() {
        let docs = vec![doc("a.md", Some(Outcome::Useful), T0, &["x"])];
        let agg = aggregate(&docs, None, T0, 2);
        assert!(agg.by_community.is_empty());
        assert!(!render_lessons(&agg, "m").contains("## By topic"));
    }

    #[test]
    fn uncategorized_sorts_after_named_topics() {
        assert!(topic_key("Zebra") < topic_key(UNCATEGORIZED));
        assert!(topic_key("Alpha") < topic_key("Beta"));
    }

    // --- rendering ---

    #[test]
    fn rendering_is_byte_stable_for_a_fixed_now() {
        let docs = vec![
            doc("a.md", Some(Outcome::Useful), T0, &["graph.rs"]),
            doc("b.md", Some(Outcome::Useful), T0 + DAY, &["graph.rs"]),
            doc("c.md", Some(Outcome::DeadEnd), T0 + 2 * DAY, &["ghost.rs"]),
        ];
        let one = render_lessons(&aggregate(&docs, None, T0 + 3 * DAY, 2), "m");
        let two = render_lessons(&aggregate(&docs, None, T0 + 3 * DAY, 2), "m");
        assert_eq!(one, two);
        assert!(one.ends_with('\n') && !one.ends_with("\n\n"));
        assert!(one.contains("**Preferred sources** — corroborated by ≥2 useful results"));
        assert!(one.contains("- `graph.rs` (2× useful)"));
        assert!(one.contains("**Known dead ends**"));
    }

    #[test]
    fn preferred_sources_are_ordered_by_score() {
        let docs = vec![
            doc("a.md", Some(Outcome::Useful), T0, &["low", "high"]),
            doc("b.md", Some(Outcome::Useful), T0, &["low", "high"]),
            doc("c.md", Some(Outcome::Useful), T0, &["high"]),
        ];
        let agg = aggregate(&docs, None, T0, 2);
        assert_eq!(names(&agg.overall.preferred), vec!["high", "low"]);
    }

    #[test]
    fn singular_wording_for_one_memory() {
        let agg = aggregate(&[doc("a.md", None, T0, &[])], None, T0, 2);
        assert!(render_lessons(&agg, "m").contains("from 1 session memory in m"));
    }

    // --- end to end over a real directory ---

    #[test]
    fn reads_a_memory_directory_written_by_save_result() {
        let dir = tempfile::tempdir().unwrap();
        let memory = dir.path().join("memory");
        std::fs::create_dir_all(&memory).unwrap();
        for (i, nodes) in ["alpha, beta", "alpha"].iter().enumerate() {
            std::fs::write(
                memory.join(format!("query_{i}.md")),
                format!(
                    "---\ntype: query\ntimestamp: {}\nnodes: [{nodes}]\n---\n\n\
                     ## Question\n\nq{i}?\n\n## Answer\n\na{i}\n",
                    T0 + i as i64
                ),
            )
            .unwrap();
        }
        // A foreign file must be ignored rather than half-parsed.
        std::fs::write(memory.join("NOTES.md"), "# just notes\n").unwrap();

        let docs = load_memory_docs(&memory);
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0].question, "q0?");
        assert_eq!(docs[0].source_nodes, vec!["alpha", "beta"]);
        assert!(docs[0].date < docs[1].date);

        cmd_reflect(&dir.path().to_string_lossy(), 2).unwrap();
        let lessons = std::fs::read_to_string(dir.path().join("reflections/LESSONS.md")).unwrap();
        assert!(lessons.contains("- 0 useful · 0 dead ends · 0 corrected · 2 unmarked"));
        assert!(lessons.contains("_No marked outcomes yet._"));
    }

    #[test]
    fn an_empty_memory_directory_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        cmd_reflect(&dir.path().to_string_lossy(), 2).unwrap();
        // Nothing to summarize means nothing is written.
        assert!(!dir.path().join("reflections/LESSONS.md").exists());
    }
}
