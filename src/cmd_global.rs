//! `global` — one knowledge graph spanning every project you have added.
//!
//! A per-project graph can only answer questions about that project. The global
//! graph exists for the questions that cross a repo boundary: which of my
//! services touch this library, where else does this concept appear, what does
//! my whole codebase estate look like. It is an *accumulation*, built by adding
//! finished `graph.json` files one at a time, never regenerated from source.
//!
//! That accumulation is the whole design constraint. Nothing here re-derives
//! anything: if the file is lost, every project has to be re-added by hand. So
//! the writes are atomic, the manifest is never silently discarded, and adding
//! a project twice replaces its contribution instead of doubling it.
//!
//! # Storage
//!
//! `~/.graphify-rs/global-graph.json` and `~/.graphify-rs/global-manifest.json`.
//!
//! Python graphify keeps the same pair under `~/.graphify/`. Sharing that
//! directory would be actively harmful rather than convenient: Python writes
//! nodes with a `file_type` field, this tool requires `node_type`, and
//! [`graphify_serve::load_graph`] rejects a Python-written graph outright.
//! Pointed at one file, whichever tool wrote second would destroy the other's
//! work. Separate directories let both be installed at once.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use colored::Colorize;
use serde::{Deserialize, Serialize};

use graphify_build::merge::{MergeInput, merge_graphs, prune_repo_from_graph};
use graphify_core::graph::KnowledgeGraph;
use graphify_core::model::CommunityInfo;

/// Directory under `$HOME` holding the cross-project graph. See the module docs
/// for why this is not Python graphify's `~/.graphify`.
const GLOBAL_DIR: &str = ".graphify-rs";

const GRAPH_FILE: &str = "global-graph.json";
const MANIFEST_FILE: &str = "global-manifest.json";

/// Schema version stamped into the manifest, so a future format change can be
/// detected instead of guessed at.
const MANIFEST_VERSION: u32 = 1;

/// Reject a graph file larger than this before parsing it. Mirrors the cap in
/// `cmd_merge`: a corrupt or hostile file must not be able to exhaust memory
/// just by being handed to `serde_json`, and real graphs are well under 5 MB.
const MAX_GRAPH_BYTES: u64 = 50 * 1024 * 1024;

/// Namespace separator used by [`MergeInput::tagged`]. A tag containing it would
/// make `repo::local_id` ambiguous, so tags are rejected on the way in.
const TAG_SEPARATOR: &str = "::";

/// Identity of an edge in the global graph: `(source, target, relation)`.
///
/// Directed even though the graph is undirected, matching `graphify_build::merge`
/// — "a calls b" and "b calls a" are different facts.
type EdgeKey = (String, String, String);

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

/// The record of which projects are in the global graph.
///
/// The graph itself could almost supply this (every node carries its `repo`),
/// but not the parts that matter for maintaining it: where each project's
/// `graph.json` lives and what it hashed to last time. Without the hash, every
/// `add` would be a full re-merge; that is what makes re-running cheap.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manifest {
    version: u32,
    /// Keyed by repo tag. A `BTreeMap` so both the file and `global list` come
    /// out in a stable order rather than whatever the hasher felt like.
    #[serde(default)]
    repos: BTreeMap<String, RepoEntry>,
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            version: MANIFEST_VERSION,
            repos: BTreeMap::new(),
        }
    }
}

/// One tracked project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RepoEntry {
    /// RFC 3339 UTC instant of the last successful add.
    added_at: String,
    /// Absolute path to the source `graph.json`, so a later add can tell "same
    /// project again" from "a different project under the same tag".
    source_path: String,
    /// Nodes this project contributed, after external-symbol deduplication.
    node_count: usize,
    /// Edges in the project's namespaced graph.
    edge_count: usize,
    /// First 16 hex chars of the source file's SHA-256. Short enough to read,
    /// long enough that a collision is not a practical concern.
    source_hash: String,
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

/// Where the global graph lives.
///
/// The directory is a field rather than a constant so tests can point at a temp
/// directory. A test that wrote to the developer's real `~/.graphify-rs` would
/// delete projects they had added, which is precisely the failure this whole
/// module is built to prevent.
struct GlobalStore {
    dir: PathBuf,
}

impl GlobalStore {
    fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The real location, `~/.graphify-rs`.
    fn home() -> Result<Self> {
        let home = dirs::home_dir().context(
            "cannot determine the home directory, so there is nowhere to keep the global graph",
        )?;
        Ok(Self::at(home.join(GLOBAL_DIR)))
    }

    fn graph_path(&self) -> PathBuf {
        self.dir.join(GRAPH_FILE)
    }

    fn manifest_path(&self) -> PathBuf {
        self.dir.join(MANIFEST_FILE)
    }

    /// Read the manifest, backing up anything unreadable rather than losing it.
    ///
    /// Infallible on purpose. A parse error here must not stop `global add` from
    /// working — but it must also never quietly become an empty manifest, since
    /// that would erase the record of every repo the user has added.
    fn load_manifest(&self) -> Manifest {
        let path = self.manifest_path();
        if !path.exists() {
            return Manifest::default();
        }
        let parsed = fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|text| serde_json::from_str::<Manifest>(&text).map_err(|e| e.to_string()));
        match parsed {
            Ok(manifest) => manifest,
            Err(reason) => {
                back_up_corrupt(&path, &reason);
                Manifest::default()
            }
        }
    }

    fn save_manifest(&self, manifest: &Manifest) -> Result<()> {
        let json =
            serde_json::to_vec_pretty(manifest).context("serializing the global manifest")?;
        write_atomic(&self.manifest_path(), |w| {
            w.write_all(&json).context("writing the global manifest")
        })
    }

    /// Load the global graph, treating "not created yet" as an empty graph.
    ///
    /// Unlike the manifest, an unparseable graph is fatal. The manifest can be
    /// rebuilt by re-adding projects; silently starting from an empty graph
    /// would report success while throwing away everything accumulated so far.
    fn load_graph(&self) -> Result<KnowledgeGraph> {
        let path = self.graph_path();
        if !path.exists() {
            return Ok(KnowledgeGraph::new());
        }
        check_size_cap(&path)?;
        graphify_serve::load_graph(&path).with_context(|| {
            format!(
                "failed to load the global graph from {}; move it aside to start over \
                 (every project then needs re-adding)",
                path.display()
            )
        })
    }

    fn save_graph(&self, graph: &KnowledgeGraph) -> Result<()> {
        write_atomic(&self.graph_path(), |w| {
            graph
                .write_node_link_json(w)
                .context("serializing the global graph")
        })
    }
}

/// Move an unreadable manifest aside so a parse error never costs the user the
/// record of every repo they have added.
///
/// Best effort by design: if even the rename fails, say so and carry on. Refusing
/// to run because a *bookkeeping* file is broken would be a worse outcome than
/// starting a new one.
fn back_up_corrupt(path: &Path, reason: &str) {
    let stamp = unix_now();
    let mut backup = with_extra_suffix(path, &format!("corrupt.{stamp}"));
    // Two failures in the same second must not have the second overwrite the
    // first backup — that is the same data loss, one step removed.
    let mut attempt = 1;
    while backup.exists() && attempt < 100 {
        backup = with_extra_suffix(path, &format!("corrupt.{stamp}-{attempt}"));
        attempt += 1;
    }

    let warn = "warning:".yellow().bold();
    match fs::rename(path, &backup) {
        Ok(()) => eprintln!(
            "{warn} the global manifest at {} failed to parse ({reason}); moved it to {} \
             and started fresh. Restore from that backup if this was unexpected.",
            path.display(),
            backup.display()
        ),
        Err(e) => eprintln!(
            "{warn} the global manifest at {} failed to parse ({reason}) and could not be \
             backed up ({e}). Starting fresh — the tracked repo list will be rebuilt as \
             projects are re-added.",
            path.display()
        ),
    }
}

/// `foo/bar.json` + `corrupt.1` -> `foo/bar.json.corrupt.1`.
///
/// Appends rather than replacing the extension so the original name stays
/// legible in a directory listing.
fn with_extra_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{suffix}"));
    path.with_file_name(name)
}

/// Write `path` by filling a sibling temp file and renaming it over the target.
///
/// The global graph is the accumulation of every project the user has added and
/// there is no second copy anywhere. A crash, a full disk, or a `^C` partway
/// through a write must leave the previous version intact rather than a
/// truncated file that fails to parse on the next run. `rename` within a single
/// directory is atomic, so a concurrent reader sees either the old file or the
/// new one and never a half-written one.
fn write_atomic<F>(path: &Path, fill: F) -> Result<()>
where
    F: FnOnce(&mut BufWriter<File>) -> Result<()>,
{
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;

    // The pid keeps two concurrent graphify processes off each other's temp file;
    // the temp file must be a sibling so the rename stays within one filesystem.
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = dir.join(format!(".{name}.tmp.{}", std::process::id()));

    let file = File::create(&tmp).with_context(|| format!("cannot create {}", tmp.display()))?;
    let mut writer = BufWriter::new(file);
    let flushed = fill(&mut writer).and_then(|()| {
        let file = writer
            .into_inner()
            .map_err(std::io::IntoInnerError::into_error)
            .with_context(|| format!("cannot flush {}", tmp.display()))?;
        // fsync before the rename: without it a crash can leave the renamed
        // file present but empty, which is exactly the outcome being avoided.
        file.sync_all()
            .with_context(|| format!("cannot flush {} to disk", tmp.display()))
    });
    if let Err(e) = flushed {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }

    fs::rename(&tmp, path)
        .with_context(|| {
            format!(
                "cannot move {} into place at {}",
                tmp.display(),
                path.display()
            )
        })
        .inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })
}

/// Fail before parsing if a graph file is implausibly large.
fn check_size_cap(path: &Path) -> Result<()> {
    let meta =
        fs::metadata(path).with_context(|| format!("cannot stat graph file {}", path.display()))?;
    if meta.len() > MAX_GRAPH_BYTES {
        bail!(
            "{} is {} bytes, exceeding the {MAX_GRAPH_BYTES}-byte cap",
            path.display(),
            meta.len()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

/// What one `add` did, so the caller can report it and tests can assert on it.
#[derive(Debug, PartialEq, Eq)]
struct AddOutcome {
    tag: String,
    nodes_added: usize,
    nodes_pruned: usize,
    /// The source hashed identically to last time, so nothing was touched.
    skipped: bool,
}

/// Add or update one project's graph in the global graph.
fn add(store: &GlobalStore, source: &Path, tag: Option<&str>) -> Result<AddOutcome> {
    // Canonicalizing first does double duty: it is the existence check, and the
    // manifest needs an absolute path anyway so a tag can be recognized again
    // from a different working directory.
    let resolved = source
        .canonicalize()
        .with_context(|| format!("graph not found: {}", source.display()))?;
    check_size_cap(&resolved)?;

    let tag = match tag {
        Some(t) => validate_tag(t)?.to_string(),
        None => default_tag(&resolved)?,
    };

    let mut manifest = store.load_manifest();
    let bytes =
        fs::read(&resolved).with_context(|| format!("cannot read {}", resolved.display()))?;
    let source_hash = short_hash(&bytes);

    if let Some(existing) = manifest.repos.get(&tag) {
        if let Some(warning) = collision_warning(existing, &tag, &resolved) {
            eprintln!("{warning}");
        }
        if existing.source_hash == source_hash {
            return Ok(AddOutcome {
                tag,
                nodes_added: 0,
                nodes_pruned: 0,
                skipped: true,
            });
        }
    }

    // Parsed from the bytes that were hashed, not re-read from disk: a file
    // rewritten between the two reads would otherwise be stored under the old
    // hash and never picked up by a later add.
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("{} is not valid JSON", resolved.display()))?;
    let source_graph = KnowledgeGraph::from_node_link_json(&value)
        .with_context(|| format!("failed to load graph from {}", resolved.display()))?;

    // Namespacing is delegated rather than reimplemented: `merge_graphs` already
    // rewrites ids to `tag::id` and records `repo`/`local_id` on every node, and
    // `prune_repo_from_graph` reads that same `repo` field back out.
    let (incoming, _) = merge_graphs(&[MergeInput::tagged(tag.clone(), &source_graph)])
        .with_context(|| format!("namespacing {} under '{tag}'", resolved.display()))?;

    // Prune before merging: an update has to replace this project's previous
    // contribution, not stack a second copy of it on top.
    let (mut global, nodes_pruned) = prune_repo_from_graph(&store.load_graph()?, &tag);
    let nodes_added = graft(&mut global, &incoming);

    // Graph first, manifest second. If the manifest write fails after the graph
    // one, the nodes are in the graph but untracked — and the next add prunes by
    // the `repo` field on the nodes themselves, so it still replaces them
    // cleanly. The reverse order would leave the manifest promising nodes that
    // are not there.
    store.save_graph(&global)?;

    manifest.version = MANIFEST_VERSION;
    manifest.repos.insert(
        tag.clone(),
        RepoEntry {
            added_at: rfc3339_utc(unix_now()),
            source_path: resolved.display().to_string(),
            node_count: nodes_added,
            edge_count: incoming.edge_count(),
            source_hash,
        },
    );
    store.save_manifest(&manifest)?;

    Ok(AddOutcome {
        tag,
        nodes_added,
        nodes_pruned,
        skipped: false,
    })
}

/// Fold `incoming` into `global`, merging third-party symbols. Returns the
/// number of nodes actually inserted.
///
/// A node with no `source_file` is not part of any repo: it is `String`,
/// `HashMap`, a stdlib call — a symbol referenced from outside the codebase.
/// Those are the *same* entity whichever project mentions them, and keeping one
/// copy per project would leave the global graph unable to answer the question
/// it exists for ("which of my repos use this?"). So they collapse onto whichever
/// node landed first, and edges that pointed at the duplicate are **rewired** to
/// the survivor. Dropping those edges instead would delete precisely the
/// cross-project links the merge is for.
fn graft(global: &mut KnowledgeGraph, incoming: &KnowledgeGraph) -> usize {
    let external: HashMap<String, String> = global
        .nodes()
        .into_iter()
        .filter(|n| n.source_file.is_empty() && !n.label.is_empty())
        .map(|n| (n.label.clone(), n.id.clone()))
        .collect();

    let mut remap: HashMap<String, String> = HashMap::new();
    for node in incoming.nodes() {
        if node.source_file.is_empty()
            && let Some(survivor) = external.get(&node.label)
        {
            remap.insert(node.id.clone(), survivor.clone());
        }
    }

    // Community ids are opaque per-run integers with no meaning outside the graph
    // that produced them, and every project's numbering restarts at 0. Shifting
    // the incoming ids past everything already here keeps each project's
    // partition internally valid; without it, community 0 from every project
    // would read as one cross-repo cluster nobody ever computed.
    let offset = global
        .nodes()
        .iter()
        .filter_map(|n| n.community)
        .chain(global.communities.iter().map(|c| c.id))
        .max()
        .map_or(0, |highest| highest + 1);

    let mut added = 0usize;
    for node in incoming.nodes() {
        if remap.contains_key(&node.id) {
            continue;
        }
        let mut node = node.clone();
        node.community = node.community.map(|c| c + offset);
        if global.add_node(node).is_ok() {
            added += 1;
        }
    }

    // Edge identity is `(source, target, relation)`, the same key `merge_graphs`
    // uses. Collapsing onto it is not just tidiness: an edge whose *both*
    // endpoints deduplicate onto shared external nodes ends up owned by no repo,
    // so pruning the project that contributed it never removes it — and without
    // this check every re-add would append another copy, growing the file
    // without bound. Deduplicating makes a repeated `global add` idempotent.
    let mut seen: HashSet<EdgeKey> = global
        .edges()
        .into_iter()
        .map(|e| (e.source.clone(), e.target.clone(), e.relation.clone()))
        .collect();

    for edge in incoming.edges() {
        let mut edge = edge.clone();
        if let Some(survivor) = remap.get(&edge.source) {
            edge.source = survivor.clone();
        }
        if let Some(survivor) = remap.get(&edge.target) {
            edge.target = survivor.clone();
        }
        // Two endpoints deduplicating onto one node is not a relationship.
        if edge.source == edge.target {
            continue;
        }
        if !seen.insert((
            edge.source.clone(),
            edge.target.clone(),
            edge.relation.clone(),
        )) {
            continue;
        }
        // Dangling only if a node above failed to insert; dropping is the same
        // rule the merge path already applies.
        let _ = global.add_edge(edge);
    }

    for info in &incoming.communities {
        let nodes: Vec<String> = info
            .nodes
            .iter()
            .filter(|id| !remap.contains_key(*id))
            .cloned()
            .collect();
        if nodes.is_empty() {
            continue;
        }
        global.communities.push(CommunityInfo {
            id: info.id + offset,
            nodes,
            cohesion: info.cohesion,
            label: info.label.clone(),
        });
    }

    added
}

/// Drop a project from the global graph. Returns the node count removed.
fn remove(store: &GlobalStore, tag: &str) -> Result<usize> {
    let mut manifest = store.load_manifest();
    if !manifest.repos.contains_key(tag) {
        let known: Vec<&str> = manifest.repos.keys().map(String::as_str).collect();
        if known.is_empty() {
            bail!("'{tag}' is not in the global graph (no projects are tracked yet)");
        }
        bail!(
            "'{tag}' is not in the global graph (tracked: {})",
            known.join(", ")
        );
    }

    let (pruned, removed) = prune_repo_from_graph(&store.load_graph()?, tag);
    store.save_graph(&pruned)?;
    manifest.repos.remove(tag);
    store.save_manifest(&manifest)?;
    Ok(removed)
}

/// Render the tracked-project listing.
fn render_list(store: &GlobalStore) -> String {
    let manifest = store.load_manifest();
    if manifest.repos.is_empty() {
        return format!(
            "The global graph is empty. Add a project with:\n  {}\n",
            "graphify-rs global add <path/to/graph.json>".cyan()
        );
    }

    let mut out = format!(
        "{} {}\n",
        "Global graph:".bold(),
        store.graph_path().display()
    );
    // Pad before coloring: ANSI escapes count toward a format width and would
    // push every line's columns out of alignment by the length of the escape.
    let width = manifest.repos.keys().map(String::len).max().unwrap_or(0);
    for (tag, info) in &manifest.repos {
        let padded = format!("{tag:width$}");
        out.push_str(&format!(
            "  {}  {} nodes, added {}\n",
            padded.cyan(),
            info.node_count,
            date_of(&info.added_at)
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Tags, hashing and time
// ---------------------------------------------------------------------------

/// Default namespace for a graph at `<project>/graphify-rs-out/graph.json`: the
/// project directory, two levels up.
///
/// Derived from the canonicalized path so that `global add graphify-rs-out/graph.json`
/// from inside a project gets the project's name rather than an empty string.
fn default_tag(resolved: &Path) -> Result<String> {
    resolved
        .parent()
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty() && !n.contains(TAG_SEPARATOR))
        .with_context(|| {
            format!(
                "cannot derive a repo tag from {}; pass --as <tag>",
                resolved.display()
            )
        })
}

/// Reject a tag that would break the namespacing scheme.
fn validate_tag(tag: &str) -> Result<&str> {
    if tag.trim().is_empty() {
        bail!("repo tag cannot be empty");
    }
    if tag.contains(TAG_SEPARATOR) {
        bail!(
            "repo tag '{tag}' cannot contain '{TAG_SEPARATOR}' — it is the separator between \
             the tag and the project's own node ids"
        );
    }
    Ok(tag)
}

/// Warn when a tag is being pointed at a different project than last time.
///
/// Not an error: re-adding the same project from a moved or renamed checkout is
/// perfectly normal. But two genuinely different projects sharing one tag
/// overwrite each other on every add, and the user almost certainly wanted
/// `--as`.
fn collision_warning(existing: &RepoEntry, tag: &str, resolved: &Path) -> Option<String> {
    let incoming = resolved.display().to_string();
    if existing.source_path.is_empty() || existing.source_path == incoming {
        return None;
    }
    // Deliberately does not promise what happens next: an unchanged hash still
    // short-circuits the add, so "proceeding" would be a lie half the time.
    Some(format!(
        "{} repo tag '{tag}' already tracks {}, but you passed {incoming}. \
         Use --as <tag> to keep the two projects separate.",
        "warning:".yellow().bold(),
        existing.source_path
    ))
}

/// First 16 hex chars of the content's SHA-256 — enough to detect a changed
/// graph, short enough to eyeball in the manifest.
fn short_hash(bytes: &[u8]) -> String {
    let full = graphify_cache::content_hash(bytes);
    full.get(..16).unwrap_or(&full).to_string()
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// The `(year, month, day)` for a count of days since the Unix epoch.
///
/// Howard Hinnant's civil-calendar algorithm, as already used by `cmd_reflect`:
/// one timestamp format does not justify pulling `chrono` into the CLI crate.
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

/// Render Unix seconds as an RFC 3339 UTC instant.
fn rfc3339_utc(ts: i64) -> String {
    let (y, m, d) = civil_from_days(ts.div_euclid(86_400));
    let rem = ts.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}+00:00",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// The date part of an RFC 3339 instant, or the whole string if it is not one.
fn date_of(added_at: &str) -> &str {
    added_at.get(..10).unwrap_or(added_at)
}

// ---------------------------------------------------------------------------
// CLI entry points
// ---------------------------------------------------------------------------

/// Add or update a project's graph in the global graph.
pub fn cmd_global_add(source: &str, tag: Option<&str>) -> Result<()> {
    let store = GlobalStore::home()?;
    let outcome = add(&store, Path::new(source), tag)?;

    if outcome.skipped {
        println!(
            "'{}' is unchanged since the last add — the global graph was not modified.",
            outcome.tag.cyan()
        );
        return Ok(());
    }

    let added = format!("+{} nodes", outcome.nodes_added).green();
    let pruned = format!("-{} pruned", outcome.nodes_pruned).yellow();
    println!(
        "Added '{}' to the global graph: {added}, {pruned}",
        outcome.tag.cyan()
    );
    println!("Global: {}", store.graph_path().display());
    Ok(())
}

/// Remove every node contributed by `tag`.
pub fn cmd_global_remove(tag: &str) -> Result<()> {
    let store = GlobalStore::home()?;
    let removed = remove(&store, tag)?;
    println!(
        "Removed '{}' from the global graph ({removed} nodes pruned).",
        tag.cyan()
    );
    Ok(())
}

/// List the projects tracked in the global graph.
pub fn cmd_global_list() -> Result<()> {
    print!("{}", render_list(&GlobalStore::home()?));
    Ok(())
}

/// Print the path to the global graph file.
///
/// Nothing else goes to stdout: this exists to be shell-substituted, as in
/// `jq . "$(graphify-rs global path)"`.
pub fn cmd_global_path() -> Result<()> {
    println!("{}", GlobalStore::home()?.graph_path().display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(id, label, source_file)` — an empty `source_file` marks an external
    /// symbol, which is what the dedup path keys on.
    type NodeSpec<'a> = (&'a str, &'a str, &'a str);

    fn graph_json(nodes: &[NodeSpec<'_>], edges: &[(&str, &str)]) -> String {
        let nodes: Vec<String> = nodes
            .iter()
            .map(|(id, label, file)| {
                format!(
                    r#"{{"id":"{id}","label":"{label}","source_file":"{file}","node_type":"class"}}"#
                )
            })
            .collect();
        let links: Vec<String> = edges
            .iter()
            .map(|(s, t)| {
                format!(
                    r#"{{"source":"{s}","target":"{t}","relation":"calls",
                        "confidence":"EXTRACTED","source_file":"t.rs"}}"#
                )
            })
            .collect();
        format!(
            r#"{{"directed":false,"multigraph":false,"graph":{{}},
                "nodes":[{}],"links":[{}]}}"#,
            nodes.join(","),
            links.join(",")
        )
    }

    /// Write a project graph at `<root>/<project>/graphify-rs-out/graph.json`,
    /// the layout the default tag is derived from.
    fn write_project(root: &Path, project: &str, body: &str) -> PathBuf {
        let dir = root.join(project).join("graphify-rs-out");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("graph.json");
        fs::write(&path, body).unwrap();
        path
    }

    fn store_in(root: &Path) -> GlobalStore {
        GlobalStore::at(root.join("home").join(GLOBAL_DIR))
    }

    fn global_ids(store: &GlobalStore) -> Vec<String> {
        let mut ids = store.load_graph().unwrap().node_ids();
        ids.sort();
        ids
    }

    #[test]
    fn adding_a_project_namespaces_it_and_records_the_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        let src = write_project(
            tmp.path(),
            "api",
            &graph_json(
                &[("main", "main", "m.rs"), ("db", "db", "d.rs")],
                &[("main", "db")],
            ),
        );

        let outcome = add(&store, &src, None).unwrap();

        assert_eq!(outcome.tag, "api");
        assert_eq!(outcome.nodes_added, 2);
        assert_eq!(outcome.nodes_pruned, 0);
        assert!(!outcome.skipped);
        assert_eq!(global_ids(&store), vec!["api::db", "api::main"]);

        let entry = store.load_manifest().repos.remove("api").unwrap();
        assert_eq!(entry.node_count, 2);
        assert_eq!(entry.edge_count, 1);
        assert_eq!(entry.source_hash.len(), 16);
        assert_eq!(
            entry.source_path,
            src.canonicalize().unwrap().display().to_string()
        );
        assert!(entry.added_at.starts_with("20"), "got {}", entry.added_at);
    }

    #[test]
    fn an_explicit_tag_overrides_the_directory_name() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        let src = write_project(
            tmp.path(),
            "api",
            &graph_json(&[("main", "main", "m.rs")], &[]),
        );

        let outcome = add(&store, &src, Some("backend")).unwrap();

        assert_eq!(outcome.tag, "backend");
        assert_eq!(global_ids(&store), vec!["backend::main"]);
    }

    #[test]
    fn re_adding_an_unchanged_graph_is_skipped_on_the_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        let src = write_project(
            tmp.path(),
            "api",
            &graph_json(&[("main", "main", "m.rs")], &[]),
        );

        add(&store, &src, None).unwrap();
        let before = fs::read_to_string(store.graph_path()).unwrap();

        let second = add(&store, &src, None).unwrap();

        assert!(second.skipped);
        assert_eq!(second.nodes_added, 0);
        assert_eq!(second.nodes_pruned, 0);
        // "Do nothing" has to mean the file is untouched, not rewritten identically.
        assert_eq!(fs::read_to_string(store.graph_path()).unwrap(), before);
    }

    #[test]
    fn re_adding_a_changed_graph_replaces_the_previous_nodes() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        let src = write_project(
            tmp.path(),
            "api",
            &graph_json(
                &[("main", "main", "m.rs"), ("old", "old", "o.rs")],
                &[("main", "old")],
            ),
        );
        add(&store, &src, None).unwrap();

        fs::write(
            &src,
            graph_json(
                &[("main", "main", "m.rs"), ("new", "new", "n.rs")],
                &[("main", "new")],
            ),
        )
        .unwrap();
        let outcome = add(&store, &src, None).unwrap();

        assert!(!outcome.skipped);
        assert_eq!(outcome.nodes_pruned, 2);
        assert_eq!(outcome.nodes_added, 2);
        // The deleted symbol is gone rather than accumulated.
        assert_eq!(global_ids(&store), vec!["api::main", "api::new"]);
        assert_eq!(store.load_graph().unwrap().edge_count(), 1);
    }

    #[test]
    fn two_projects_coexist_without_colliding() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        let api = write_project(
            tmp.path(),
            "api",
            &graph_json(&[("main", "main", "m.rs")], &[]),
        );
        let web = write_project(
            tmp.path(),
            "web",
            &graph_json(&[("main", "main", "m.rs")], &[]),
        );

        add(&store, &api, None).unwrap();
        add(&store, &web, None).unwrap();

        // Same local id in both repos stays two distinct nodes.
        assert_eq!(global_ids(&store), vec!["api::main", "web::main"]);
        assert_eq!(store.load_manifest().repos.len(), 2);
    }

    #[test]
    fn external_symbols_dedupe_across_projects_and_keep_their_edges() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        // `String` has no source_file, so it is a third-party symbol.
        let api = write_project(
            tmp.path(),
            "api",
            &graph_json(
                &[("main", "main", "m.rs"), ("String", "String", "")],
                &[("main", "String")],
            ),
        );
        // `web` references the same symbol under a different local id, plus a
        // second alias of it so the remap can produce a would-be self-loop.
        let web = write_project(
            tmp.path(),
            "web",
            &graph_json(
                &[
                    ("app", "app", "a.rs"),
                    ("str", "String", ""),
                    ("str2", "String", ""),
                ],
                &[("app", "str"), ("str", "str2")],
            ),
        );

        add(&store, &api, None).unwrap();
        let outcome = add(&store, &web, None).unwrap();

        // Only `app` is new: both `String` aliases folded onto the existing node.
        assert_eq!(outcome.nodes_added, 1);
        assert_eq!(
            global_ids(&store),
            vec!["api::String", "api::main", "web::app"]
        );

        let graph = store.load_graph().unwrap();
        // The cross-project edge was rewired, not dropped.
        let mut neighbors = graph.neighbor_ids("api::String");
        neighbors.sort();
        assert_eq!(neighbors, vec!["api::main", "web::app"]);
        // `str -> str2` collapsed to a self-loop and was skipped instead.
        assert_eq!(graph.edge_count(), 2);
    }

    #[test]
    fn re_adding_never_grows_the_edge_list() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        let api = write_project(
            tmp.path(),
            "api",
            &graph_json(&[("main", "main", "m.rs"), ("Vec", "Vec", "")], &[]),
        );
        add(&store, &api, None).unwrap();

        // An edge between two *external* nodes ends up owned by no repo once both
        // endpoints deduplicate, so pruning `web` will never reclaim it. Without
        // edge deduplication every re-add would append another copy for ever.
        let web_body = |seq: u32| {
            graph_json(
                &[
                    ("app", "app", "a.rs"),
                    ("v", "Vec", ""),
                    ("s", "String", ""),
                    (&format!("churn{seq}"), "churn", "c.rs"),
                ],
                &[("app", "v"), ("v", "s")],
            )
        };
        let web = write_project(tmp.path(), "web", &web_body(0));

        add(&store, &web, None).unwrap();
        let after_first = store.load_graph().unwrap().edge_count();

        for seq in 1..4 {
            fs::write(&web, web_body(seq)).unwrap();
            add(&store, &web, None).unwrap();
        }

        assert_eq!(
            store.load_graph().unwrap().edge_count(),
            after_first,
            "re-adding a project must be idempotent on edges"
        );
    }

    #[test]
    fn removing_a_project_prunes_its_nodes_and_forgets_it() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        let api = write_project(
            tmp.path(),
            "api",
            &graph_json(
                &[("main", "main", "m.rs"), ("db", "db", "d.rs")],
                &[("main", "db")],
            ),
        );
        let web = write_project(
            tmp.path(),
            "web",
            &graph_json(&[("app", "app", "a.rs")], &[]),
        );
        add(&store, &api, None).unwrap();
        add(&store, &web, None).unwrap();

        let removed = remove(&store, "api").unwrap();

        assert_eq!(removed, 2);
        assert_eq!(global_ids(&store), vec!["web::app"]);
        assert_eq!(store.load_graph().unwrap().edge_count(), 0);
        let repos = store.load_manifest().repos;
        assert!(!repos.contains_key("api"));
        assert!(repos.contains_key("web"));
    }

    #[test]
    fn removing_an_untracked_tag_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());

        let err = remove(&store, "ghost").unwrap_err().to_string();
        assert!(err.contains("no projects are tracked yet"), "got {err}");

        let api = write_project(
            tmp.path(),
            "api",
            &graph_json(&[("main", "main", "m.rs")], &[]),
        );
        add(&store, &api, None).unwrap();

        let err = remove(&store, "ghost").unwrap_err().to_string();
        assert!(err.contains("tracked: api"), "got {err}");
        // A failed remove leaves the graph alone.
        assert_eq!(global_ids(&store), vec!["api::main"]);
    }

    #[test]
    fn a_corrupt_manifest_is_backed_up_and_a_fresh_one_starts() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        let api = write_project(
            tmp.path(),
            "api",
            &graph_json(&[("main", "main", "m.rs")], &[]),
        );
        add(&store, &api, None).unwrap();

        let manifest_path = store.manifest_path();
        let original = fs::read_to_string(&manifest_path).unwrap();
        fs::write(&manifest_path, "{ this is not json").unwrap();

        // Reading recovers rather than failing...
        assert!(store.load_manifest().repos.is_empty());

        // ...and the unparseable bytes are preserved beside it.
        let backups: Vec<PathBuf> = fs::read_dir(&store.dir)
            .unwrap()
            .filter_map(|e| {
                let path = e.ok()?.path();
                path.file_name()?
                    .to_string_lossy()
                    .contains(".corrupt.")
                    .then_some(path)
            })
            .collect();
        assert_eq!(backups.len(), 1, "expected one backup, got {backups:?}");
        assert_eq!(
            fs::read_to_string(&backups[0]).unwrap(),
            "{ this is not json"
        );
        assert!(
            !manifest_path.exists(),
            "the corrupt file was moved, not copied"
        );
        assert_ne!(original, "");

        // The tool keeps working: adding again writes a valid manifest.
        let web = write_project(
            tmp.path(),
            "web",
            &graph_json(&[("app", "app", "a.rs")], &[]),
        );
        add(&store, &web, None).unwrap();
        assert_eq!(
            store.load_manifest().repos.keys().collect::<Vec<_>>(),
            vec!["web"]
        );
    }

    #[test]
    fn a_second_corruption_does_not_overwrite_the_first_backup() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        fs::create_dir_all(&store.dir).unwrap();

        for body in ["{ bad one", "{ bad two"] {
            fs::write(store.manifest_path(), body).unwrap();
            let _ = store.load_manifest();
        }

        let backups = fs::read_dir(&store.dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .is_ok_and(|e| e.file_name().to_string_lossy().contains(".corrupt."))
            })
            .count();
        assert_eq!(backups, 2);
    }

    #[test]
    fn pointing_a_tag_at_a_different_project_warns_but_proceeds() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        let first = write_project(tmp.path(), "one", &graph_json(&[("a", "a", "a.rs")], &[]));
        let second = write_project(tmp.path(), "two", &graph_json(&[("b", "b", "b.rs")], &[]));
        add(&store, &first, Some("shared")).unwrap();

        let entry = store.load_manifest().repos.remove("shared").unwrap();
        let resolved = second.canonicalize().unwrap();

        // The warning names both paths and points at the fix.
        let warning = collision_warning(&entry, "shared", &resolved).expect("expected a warning");
        assert!(warning.contains(&entry.source_path), "got {warning}");
        assert!(
            warning.contains(&resolved.display().to_string()),
            "got {warning}"
        );
        assert!(warning.contains("--as"), "got {warning}");

        // Same project again is not a collision.
        let same = first.canonicalize().unwrap();
        assert!(collision_warning(&entry, "shared", &same).is_none());

        // And the add itself goes through, replacing the old contribution.
        let outcome = add(&store, &second, Some("shared")).unwrap();
        assert!(!outcome.skipped);
        assert_eq!(global_ids(&store), vec!["shared::b"]);
        assert_eq!(
            store.load_manifest().repos["shared"].source_path,
            resolved.display().to_string()
        );
    }

    #[test]
    fn listing_shows_the_empty_state_then_the_tracked_projects() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());

        let empty = render_list(&store);
        assert!(empty.contains("global graph is empty"), "got {empty}");
        assert!(empty.contains("global add"), "got {empty}");

        let api = write_project(
            tmp.path(),
            "api",
            &graph_json(&[("main", "main", "m.rs"), ("db", "db", "d.rs")], &[]),
        );
        let web = write_project(
            tmp.path(),
            "web",
            &graph_json(&[("app", "app", "a.rs")], &[]),
        );
        add(&store, &api, None).unwrap();
        add(&store, &web, None).unwrap();

        let listed = render_list(&store);
        assert!(listed.contains(&store.graph_path().display().to_string()));
        assert!(listed.contains("api"), "got {listed}");
        assert!(listed.contains("2 nodes"), "got {listed}");
        assert!(listed.contains("web"), "got {listed}");
        assert!(listed.contains("1 nodes"), "got {listed}");
        // The date column is the date, not the whole timestamp.
        assert!(!listed.contains("+00:00"), "got {listed}");
    }

    #[test]
    fn adding_a_missing_graph_reports_the_path() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        let err = add(&store, &tmp.path().join("nope.json"), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("graph not found"), "got {err}");
    }

    #[test]
    fn a_tag_containing_the_separator_is_rejected() {
        assert!(validate_tag("api").is_ok());
        assert!(validate_tag("").is_err());
        assert!(validate_tag("  ").is_err());
        let err = validate_tag("a::b").unwrap_err().to_string();
        assert!(err.contains("separator"), "got {err}");
    }

    #[test]
    fn writes_are_atomic_and_leave_no_temp_files() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        let api = write_project(
            tmp.path(),
            "api",
            &graph_json(&[("main", "main", "m.rs")], &[]),
        );
        add(&store, &api, None).unwrap();

        let leftovers: Vec<String> = fs::read_dir(&store.dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp."))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    #[test]
    fn a_failed_write_leaves_the_previous_file_intact() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("graph.json");
        fs::write(&target, "original").unwrap();

        let err = write_atomic(&target, |_| bail!("disk on fire"));

        assert!(err.is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "original");
        let leftovers = fs::read_dir(tmp.path())
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .is_ok_and(|e| e.file_name().to_string_lossy().contains(".tmp."))
            })
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn communities_from_different_projects_do_not_conflate() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        let clustered = |id: &str| {
            format!(
                r#"{{"directed":false,"multigraph":false,"graph":{{}},"nodes":[
                    {{"id":"{id}","label":"{id}","source_file":"x.rs",
                      "node_type":"class","community":0}}],"links":[]}}"#
            )
        };
        let api = write_project(tmp.path(), "api", &clustered("a"));
        let web = write_project(tmp.path(), "web", &clustered("b"));

        add(&store, &api, None).unwrap();
        add(&store, &web, None).unwrap();

        let graph = store.load_graph().unwrap();
        let api_community = graph.get_node("api::a").unwrap().community;
        let web_community = graph.get_node("web::b").unwrap().community;
        assert!(api_community.is_some());
        assert_ne!(
            api_community, web_community,
            "two projects' community 0 must not read as one cluster"
        );
    }

    #[test]
    fn timestamps_render_as_rfc3339_utc() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00+00:00");
        assert_eq!(rfc3339_utc(1_770_000_000), "2026-02-02T02:40:00+00:00");
        assert_eq!(date_of("2026-02-02T02:40:00+00:00"), "2026-02-02");
        assert_eq!(date_of("nonsense"), "nonsense");
    }

    #[test]
    fn the_default_tag_is_the_project_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let src = write_project(tmp.path(), "my-project", &graph_json(&[], &[]));
        let resolved = src.canonicalize().unwrap();
        assert_eq!(default_tag(&resolved).unwrap(), "my-project");
    }

    #[test]
    fn an_oversized_graph_is_rejected_before_parsing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("huge.json");
        let file = File::create(&path).unwrap();
        file.set_len(MAX_GRAPH_BYTES + 1).unwrap();
        drop(file);

        let err = check_size_cap(&path).unwrap_err().to_string();
        assert!(err.contains("exceeding"), "got {err}");
    }
}
