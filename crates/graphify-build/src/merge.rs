//! Merging whole [`KnowledgeGraph`]s.
//!
//! [`crate::build`] merges *extraction results*: many small slices of one graph,
//! where a repeated node id is just the same symbol seen twice, so first-write-wins
//! is the whole story. This module merges *finished graphs*, where a repeated id
//! can mean two genuinely different things — the same symbol name in two repos, or
//! two versions of the same symbol on two branches — so the resolution rules have
//! to be spelled out rather than assumed.
//!
//! Two strategies live here:
//!
//! * [`merge_graphs`] — an N-way union used by `graphify-rs merge-graphs`. Inputs
//!   are unrelated graphs (typically one per repo), so ids may be *namespaced*
//!   with a per-input tag to stop unrelated entities from colliding.
//! * [`three_way_merge`] — the git merge driver for `graph.json`. Inputs are three
//!   versions of *one* graph, so ids are directly comparable and `base` tells us
//!   which side changed what.
//!
//! # Communities
//!
//! Community ids are opaque integers produced by a clustering run. They carry no
//! meaning outside the graph they were computed in: community `3` in repo A and
//! community `3` in repo B are unrelated, and even two clustering runs over the
//! same repo renumber freely. Neither strategy therefore ever *matches* nodes on
//! community, and each is explicit about what it emits (see each function's docs).
//! Either way the merged partition is a best-effort carry-over — re-run
//! `graphify-rs cluster` (or `update`) for a partition that spans the merged graph.
//!
//! # Hyperedges
//!
//! Hyperedges are merged and deduplicated by value, but note that the node-link
//! JSON format written by [`KnowledgeGraph::write_node_link_json`] has no field for
//! them, so they survive in memory only and are lost the moment the merged graph is
//! written to disk. They are handled here so in-process callers get the right
//! answer, not because `merge-graphs` can round-trip them.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde_json::Value;
use tracing::debug;

use graphify_core::error::Result;
use graphify_core::graph::KnowledgeGraph;
use graphify_core::model::{CommunityInfo, GraphEdge, GraphNode, Hyperedge};

/// Identity of an edge for deduplication: `(source, target, relation)`.
///
/// Deliberately *directed* even though the backing graph is undirected — "a calls
/// b" and "b calls a" are different facts, and collapsing them would invent an
/// edge nobody extracted.
type EdgeKey = (String, String, String);

/// Identity of a hyperedge: the whole record, since it has no separate id.
type HyperKey = (Vec<String>, String, String);

/// One input to [`merge_graphs`].
pub struct MergeInput<'g> {
    /// Namespace prepended to every node id as `"{tag}::{id}"`.
    ///
    /// `None` merges ids verbatim, which is what you want when the inputs really
    /// are views of the same codebase. `Some(tag)` is what you want across repos:
    /// two projects both have a `src/main` file node, and merging them by id would
    /// silently fuse two unrelated entities and invent edges between them.
    pub tag: Option<String>,
    pub graph: &'g KnowledgeGraph,
}

impl<'g> MergeInput<'g> {
    /// Namespace this graph's ids under `tag`.
    pub fn tagged(tag: impl Into<String>, graph: &'g KnowledgeGraph) -> Self {
        Self {
            tag: Some(tag.into()),
            graph,
        }
    }

    /// Merge this graph's ids verbatim, letting same-id nodes collide and resolve.
    pub fn untagged(graph: &'g KnowledgeGraph) -> Self {
        Self { tag: None, graph }
    }
}

/// What a merge actually did, for reporting and for tests to assert on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergeStats {
    /// Number of graphs merged.
    pub inputs: usize,
    /// Nodes in the merged graph.
    pub nodes: usize,
    /// Edges in the merged graph.
    pub edges: usize,
    /// Hyperedges in the merged graph.
    pub hyperedges: usize,
    /// Node ids that showed up in more than one input (or on both sides of a
    /// three-way merge).
    pub duplicate_nodes: usize,
    /// Of those, how many disagreed in a way no rule could settle and had to fall
    /// back on the ours-wins tiebreak. A non-zero count is the signal that the
    /// inputs describe the same entity differently.
    pub node_conflicts: usize,
    /// Edges collapsed onto an existing `(source, target, relation)` triple.
    pub duplicate_edges: usize,
    /// Edges and hyperedges dropped because an endpoint node did not survive.
    pub dangling_dropped: usize,
    /// Three-way only: nodes/edges/hyperedges dropped because one side deleted
    /// them relative to `base`.
    pub deletions_honored: usize,
}

/// Merge two or more finished graphs into one.
///
/// # Resolution rules
///
/// * **Node ids** — namespaced per input when [`MergeInput::tag`] is set, which
///   makes collisions impossible by construction. Tagged nodes also carry `repo`
///   (the tag) and `local_id` (the pre-namespacing id) so consumers can recover
///   the original identity.
/// * **Duplicate node ids** (only reachable for untagged inputs) — **first input
///   wins**, matching the first-write-wins rule [`crate::build`] already uses, so
///   the two paths cannot disagree. Input order is therefore a precedence order:
///   put the graph you trust most first. Disagreements are counted in
///   [`MergeStats::node_conflicts`] rather than silently blended, because a blended
///   record would claim a `source_location` from one graph and a `label` from
///   another and be true of neither.
/// * **Duplicate edges** — collapsed on `(source, target, relation)`. The record
///   with the higher `confidence_score` survives (ties keep the earlier input), so
///   an AST-extracted edge beats a guessed one regardless of input order.
/// * **Communities** — each input's ids are renumbered into a disjoint range.
///   Every input's partition stays internally valid and no two inputs' partitions
///   are ever conflated. Labels are prefixed with the tag so `"auth"` from two
///   repos stays distinguishable. Cohesion is carried over unchanged; it was
///   computed within one graph and is not recomputed here.
/// * **Hyperedges** — node ids remapped like everything else, deduplicated by
///   value, dropped if any member node is missing.
/// * **Dangling edges** — dropped, as in [`crate::build_from_extraction`].
pub fn merge_graphs(inputs: &[MergeInput<'_>]) -> Result<(KnowledgeGraph, MergeStats)> {
    let mut stats = MergeStats {
        inputs: inputs.len(),
        ..Default::default()
    };

    let mut nodes: Vec<GraphNode> = Vec::new();
    let mut node_index: HashMap<String, usize> = HashMap::new();
    let mut edges: Vec<GraphEdge> = Vec::new();
    let mut edge_index: HashMap<EdgeKey, usize> = HashMap::new();
    let mut hyperedges: Vec<Hyperedge> = Vec::new();
    let mut hyper_seen: HashSet<HyperKey> = HashSet::new();

    // Community renumbering state, shared across inputs so the ranges stay disjoint.
    let mut next_community = 0usize;
    let mut community_meta: HashMap<usize, (f64, Option<String>)> = HashMap::new();

    for input in inputs {
        let tag = input.tag.as_deref();
        let src = input.graph;

        // Source-side community metadata, so cohesion/labels survive renumbering.
        let src_meta: HashMap<usize, &CommunityInfo> =
            src.communities.iter().map(|c| (c.id, c)).collect();
        let mut remap: HashMap<usize, usize> = HashMap::new();

        for node in src.nodes() {
            let mut merged = node.clone();
            merged.id = namespaced(tag, &node.id);
            if let Some(t) = tag {
                merged
                    .extra
                    .insert("repo".into(), Value::String(t.to_string()));
                merged
                    .extra
                    .entry("local_id".into())
                    .or_insert_with(|| Value::String(node.id.clone()));
            }
            if let Some(old) = node.community {
                merged.community = Some(remap_community(
                    old,
                    tag,
                    &src_meta,
                    &mut remap,
                    &mut next_community,
                    &mut community_meta,
                ));
            }

            match node_index.get(&merged.id) {
                Some(&existing) => {
                    stats.duplicate_nodes += 1;
                    if !same_node_attrs(&nodes[existing], &merged) {
                        stats.node_conflicts += 1;
                        debug!(
                            "merge: keeping first record for conflicting node id {}",
                            merged.id
                        );
                    }
                }
                None => {
                    node_index.insert(merged.id.clone(), nodes.len());
                    nodes.push(merged);
                }
            }
        }

        for edge in src.edges() {
            let mut merged = edge.clone();
            merged.source = namespaced(tag, &edge.source);
            merged.target = namespaced(tag, &edge.target);
            let key = edge_key(&merged);
            match edge_index.get(&key) {
                Some(&existing) => {
                    stats.duplicate_edges += 1;
                    if merged.confidence_score > edges[existing].confidence_score {
                        edges[existing] = merged;
                    }
                }
                None => {
                    edge_index.insert(key, edges.len());
                    edges.push(merged);
                }
            }
        }

        for hyper in &src.hyperedges {
            let mut merged = hyper.clone();
            merged.nodes = hyper.nodes.iter().map(|n| namespaced(tag, n)).collect();
            if hyper_seen.insert(hyperedge_key(&merged)) {
                hyperedges.push(merged);
            }
        }
    }

    let graph = assemble(nodes, edges, hyperedges, &community_meta, &mut stats)?;
    Ok((graph, stats))
}

/// Three-way merge of one graph's `base`, `ours` and `theirs` versions.
///
/// This backs the git merge driver. `graph.json` is a *derived* artifact: nobody
/// hand-edits it, both sides regenerated it from their own tree, and a textual
/// conflict is pure noise a human cannot usefully resolve. So this never fails on
/// disagreement — it always produces a graph, biased toward `ours` the way git's
/// own `-X ours` is, and leaves regeneration (`graphify-rs update`) as the way to
/// get a canonical result.
///
/// # Resolution rules
///
/// Nodes, edges (keyed on `(source, target, relation)`) and hyperedges each get
/// the same set-level treatment:
///
/// * **Present on both sides** — if the records match, done. Otherwise `base`
///   breaks the tie: whichever side still equals `base` did not change it, so the
///   *other* side's edit is taken. If both sides changed it differently (or the
///   item is new on both sides), `ours` wins and the case is counted in
///   [`MergeStats::node_conflicts`].
/// * **Only on one side** — kept if `base` also lacks it (that side added it),
///   dropped if `base` has it (the other side deleted it). Union is the default,
///   but a deliberate deletion is not resurrected.
/// * **Modify/delete** — the delete wins. Git would raise a conflict here; for a
///   graph, keeping a node whose symbol one side has demonstrably removed leaves a
///   phantom in every downstream query, and the edit will come back on the next
///   `update` if the symbol really still exists.
/// * **Communities** — both sides re-clustered independently, so their ids are
///   incomparable even though their node ids are not. The merged graph keeps
///   `ours`' numbering: nodes `ours` has keep their `ours` community, and nodes
///   that exist only on `theirs`' side are left **unassigned** rather than stamped
///   with an id that means something different in `ours`' numbering. The one
///   exception is an `ours` that was never clustered at all, where `theirs`'
///   numbering is adopted wholesale because there is nothing to conflict with.
/// * **Dangling edges** — dropped once the surviving node set is known.
///
/// # Git setup
///
/// The driver is not meant to be run by hand. Register it once per machine:
///
/// ```text
/// git config merge.graphify.name "graphify graph.json union merge"
/// git config merge.graphify.driver "graphify-rs merge-driver %O %A %B"
/// ```
///
/// and point `graph.json` at it from `.gitattributes` (committed, so the whole
/// team gets it):
///
/// ```text
/// graphify-out/graph.json merge=graphify
/// ```
///
/// Git substitutes `%O` (common ancestor), `%A` (ours) and `%B` (theirs) with
/// temp files and reads the result back from the `%A` path, which is why the
/// merged graph is written over `ours`. Exit status 0 means merged, non-zero
/// means conflict.
pub fn three_way_merge(
    base: &KnowledgeGraph,
    ours: &KnowledgeGraph,
    theirs: &KnowledgeGraph,
) -> Result<(KnowledgeGraph, MergeStats)> {
    let mut stats = MergeStats {
        inputs: 3,
        ..Default::default()
    };

    let base_nodes = index_nodes(base);
    let ours_nodes = index_nodes(ours);
    let theirs_nodes = index_nodes(theirs);

    // If `ours` was never clustered there is no numbering to protect, so `theirs`'
    // assignments are strictly better than nothing.
    let adopt_theirs_communities = !ours.nodes().iter().any(|n| n.community.is_some());

    let mut nodes: Vec<GraphNode> = Vec::new();
    for node in ours.nodes() {
        let id = node.id.as_str();
        match theirs_nodes.get(id) {
            Some(other) => {
                stats.duplicate_nodes += 1;
                let winner = pick_node(base_nodes.get(id).copied(), node, other, &mut stats);
                let mut merged = winner.clone();
                merged.community = if adopt_theirs_communities {
                    other.community
                } else {
                    node.community
                };
                nodes.push(merged);
            }
            None if base_nodes.contains_key(id) => stats.deletions_honored += 1,
            None => nodes.push(node.clone()),
        }
    }
    for node in theirs.nodes() {
        let id = node.id.as_str();
        if ours_nodes.contains_key(id) {
            continue;
        }
        if base_nodes.contains_key(id) {
            stats.deletions_honored += 1;
            continue;
        }
        let mut merged = node.clone();
        if !adopt_theirs_communities {
            merged.community = None;
        }
        nodes.push(merged);
    }

    let base_edges = index_edges(base);
    let theirs_edges = index_edges(theirs);

    let mut edges: Vec<GraphEdge> = Vec::new();
    // Nothing stops a graph.json from carrying two edges with the same
    // (source, target, relation) triple, so `seen` doubles as the "already
    // decided" marker across both loops and as an intra-side deduplicator —
    // the driver should not keep re-emitting whatever slop it was handed.
    let mut seen: HashSet<EdgeKey> = HashSet::new();
    for edge in ours.edges() {
        let key = edge_key(edge);
        if !seen.insert(key.clone()) {
            stats.duplicate_edges += 1;
            continue;
        }
        match theirs_edges.get(&key) {
            Some(other) => {
                stats.duplicate_edges += 1;
                edges.push(pick_edge(base_edges.get(&key).copied(), edge, other).clone());
            }
            None if base_edges.contains_key(&key) => stats.deletions_honored += 1,
            None => edges.push(edge.clone()),
        }
    }
    for edge in theirs.edges() {
        let key = edge_key(edge);
        // Already resolved against our copy, or a duplicate within `theirs`.
        if !seen.insert(key.clone()) {
            continue;
        }
        if base_edges.contains_key(&key) {
            stats.deletions_honored += 1;
            continue;
        }
        edges.push(edge.clone());
    }

    // Hyperedges have no id apart from their contents, so "changed" is not a
    // distinguishable state — the whole record is the key and set logic suffices.
    let base_hyper: HashSet<HyperKey> = base.hyperedges.iter().map(hyperedge_key).collect();
    let ours_hyper: HashSet<HyperKey> = ours.hyperedges.iter().map(hyperedge_key).collect();
    let theirs_hyper: HashSet<HyperKey> = theirs.hyperedges.iter().map(hyperedge_key).collect();

    let mut hyperedges: Vec<Hyperedge> = Vec::new();
    let mut hyper_seen: HashSet<HyperKey> = HashSet::new();
    for hyper in ours.hyperedges.iter().chain(theirs.hyperedges.iter()) {
        let key = hyperedge_key(hyper);
        let deleted_by_theirs = base_hyper.contains(&key) && !theirs_hyper.contains(&key);
        let deleted_by_ours = base_hyper.contains(&key) && !ours_hyper.contains(&key);
        if deleted_by_ours || deleted_by_theirs {
            if hyper_seen.insert(key) {
                stats.deletions_honored += 1;
            }
            continue;
        }
        if hyper_seen.insert(key) {
            hyperedges.push(hyper.clone());
        }
    }

    // Community metadata is carried by whichever side supplied the numbering.
    let source_of_truth = if adopt_theirs_communities {
        theirs
    } else {
        ours
    };
    let community_meta: HashMap<usize, (f64, Option<String>)> = source_of_truth
        .communities
        .iter()
        .map(|c| (c.id, (c.cohesion, c.label.clone())))
        .collect();

    let graph = assemble(nodes, edges, hyperedges, &community_meta, &mut stats)?;
    Ok((graph, stats))
}

/// Derive a unique, human-meaningful namespace tag per input graph path.
///
/// A `graph.json` lives at `<repo>/graphify-out/graph.json`, so the repo directory
/// is two levels up. That name alone is not unique across inputs — `src/graphify-out`
/// and `frontend/src/graphify-out` both yield `src`, and tagging both node sets
/// `src::` puts the collisions right back. Colliding tags are widened with their
/// own parent directory (`frontend_src`), then an index suffix guarantees
/// uniqueness so no two graphs can ever share a prefix.
pub fn distinct_repo_tags<P: AsRef<Path>>(paths: &[P]) -> Vec<String> {
    let repo_dirs: Vec<Option<&Path>> = paths
        .iter()
        .map(|p| p.as_ref().parent().and_then(Path::parent))
        .collect();

    let dir_name = |d: Option<&Path>| -> String {
        d.and_then(Path::file_name)
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "repo".to_string())
    };

    let mut tags: Vec<String> = repo_dirs.iter().map(|d| dir_name(*d)).collect();
    if tags.iter().collect::<HashSet<_>>().len() != tags.len() {
        tags = repo_dirs
            .iter()
            .map(|d| {
                let name = dir_name(*d);
                match d.and_then(Path::parent).and_then(Path::file_name) {
                    Some(parent) if !parent.is_empty() => {
                        format!("{}_{name}", parent.to_string_lossy())
                    }
                    _ => name,
                }
            })
            .collect();
    }

    let mut seen: HashMap<String, usize> = HashMap::new();
    tags.into_iter()
        .map(|t| {
            let count = seen.entry(t.clone()).or_insert(0);
            *count += 1;
            if *count == 1 {
                t
            } else {
                format!("{t}-{count}")
            }
        })
        .collect()
}

/// Remove every node contributed by `tag`, plus the edges incident to them.
///
/// Returns the pruned graph and how many nodes it dropped.
///
/// This is the counterpart to adding a tagged graph with [`MergeInput::tagged`]:
/// re-adding a project has to *replace* its contribution rather than pile a
/// second copy on top, and dropping a project has to leave nothing of it behind.
///
/// A new graph comes back instead of an in-place edit because [`KnowledgeGraph`]
/// deliberately exposes no node removal — its `add_edge` resolves endpoints
/// through an id index, so a partial removal would strand edges pointing at ids
/// that no longer resolve. Rebuilding makes the invariant structural: an edge
/// survives only if both of its endpoints did.
///
/// Ownership is read from the `repo` field that [`merge_graphs`] stamps on every
/// namespaced node, falling back to the `tag::` id prefix so a global graph
/// written before that field existed still prunes correctly. A node that
/// explicitly claims a *different* `repo` is never pruned on prefix alone: the
/// stated owner is better evidence than a string match on the id.
pub fn prune_repo_from_graph(graph: &KnowledgeGraph, tag: &str) -> (KnowledgeGraph, usize) {
    let prefix = format!("{tag}::");
    let survivors: HashSet<&str> = graph
        .nodes()
        .into_iter()
        .filter(|n| !belongs_to_repo(n, tag, &prefix))
        .map(|n| n.id.as_str())
        .collect();
    let removed = graph.node_count() - survivors.len();

    let mut pruned = KnowledgeGraph::new();
    for node in graph.nodes() {
        if survivors.contains(node.id.as_str()) {
            // Ids are unique within the source graph, so this cannot collide.
            let _ = pruned.add_node(node.clone());
        }
    }
    for edge in graph.edges() {
        if survivors.contains(edge.source.as_str()) && survivors.contains(edge.target.as_str()) {
            // Both endpoints were just inserted, so this cannot dangle.
            let _ = pruned.add_edge(edge.clone());
        }
    }

    // A hyperedge is a single fact about a *set* of nodes; with a member gone it
    // no longer states that fact, so it goes rather than quietly shrinking.
    pruned.set_hyperedges(
        graph
            .hyperedges
            .iter()
            .filter(|h| h.nodes.iter().all(|n| survivors.contains(n.as_str())))
            .cloned()
            .collect(),
    );

    pruned.communities = graph
        .communities
        .iter()
        .filter_map(|c| {
            let nodes: Vec<String> = c
                .nodes
                .iter()
                .filter(|n| survivors.contains(n.as_str()))
                .cloned()
                .collect();
            // A community that lost every member is not an empty community, it
            // is a community that no longer exists.
            (!nodes.is_empty()).then(|| CommunityInfo {
                id: c.id,
                nodes,
                cohesion: c.cohesion,
                label: c.label.clone(),
            })
        })
        .collect();

    (pruned, removed)
}

// --- internals ---------------------------------------------------------------

/// Whether `node` was contributed by `tag`. See [`prune_repo_from_graph`].
fn belongs_to_repo(node: &GraphNode, tag: &str, prefix: &str) -> bool {
    match node.extra.get("repo").and_then(Value::as_str) {
        Some(repo) => repo == tag,
        None => node.id.starts_with(prefix),
    }
}

/// Prepend `tag::` to an id, or return it unchanged when untagged.
fn namespaced(tag: Option<&str>, id: &str) -> String {
    match tag {
        Some(t) => format!("{t}::{id}"),
        None => id.to_string(),
    }
}

fn edge_key(edge: &GraphEdge) -> EdgeKey {
    (
        edge.source.clone(),
        edge.target.clone(),
        edge.relation.clone(),
    )
}

fn hyperedge_key(hyper: &Hyperedge) -> HyperKey {
    (
        hyper.nodes.clone(),
        hyper.relation.clone(),
        hyper.label.clone(),
    )
}

fn index_nodes(graph: &KnowledgeGraph) -> HashMap<&str, &GraphNode> {
    graph
        .nodes()
        .into_iter()
        .map(|n| (n.id.as_str(), n))
        .collect()
}

fn index_edges(graph: &KnowledgeGraph) -> HashMap<EdgeKey, &GraphEdge> {
    graph
        .edges()
        .into_iter()
        .map(|e| (edge_key(e), e))
        .collect()
}

/// Compare everything about a node *except* its community.
///
/// Community ids are renumbered by every clustering run, so including them would
/// mark every node as changed and drown out the attribute differences that
/// actually indicate the two sides disagree.
fn same_node_attrs(a: &GraphNode, b: &GraphNode) -> bool {
    a.id == b.id
        && a.label == b.label
        && a.source_file == b.source_file
        && a.source_location == b.source_location
        && a.node_type == b.node_type
        && a.extra == b.extra
}

/// Three-way pick for a node present on both sides. See [`three_way_merge`].
fn pick_node<'a>(
    base: Option<&'a GraphNode>,
    ours: &'a GraphNode,
    theirs: &'a GraphNode,
    stats: &mut MergeStats,
) -> &'a GraphNode {
    if same_node_attrs(ours, theirs) {
        return ours;
    }
    match base {
        // Unchanged on our side, so their edit is the only edit.
        Some(b) if same_node_attrs(ours, b) => theirs,
        Some(b) if same_node_attrs(theirs, b) => ours,
        _ => {
            stats.node_conflicts += 1;
            debug!("merge-driver: divergent node {}, keeping ours", ours.id);
            ours
        }
    }
}

/// Three-way pick for an edge present on both sides. Same shape as [`pick_node`],
/// minus the conflict accounting: an edge's identity *is* its key, so a
/// disagreement can only be in metadata (confidence, weight, provenance) and is
/// not worth reporting to the user as a conflict.
fn pick_edge<'a>(
    base: Option<&'a GraphEdge>,
    ours: &'a GraphEdge,
    theirs: &'a GraphEdge,
) -> &'a GraphEdge {
    if ours == theirs {
        return ours;
    }
    match base {
        Some(b) if ours == b => theirs,
        Some(b) if theirs == b => ours,
        _ => ours,
    }
}

/// Allocate (or reuse) a merged community id for `old` coming from one input.
fn remap_community(
    old: usize,
    tag: Option<&str>,
    src_meta: &HashMap<usize, &CommunityInfo>,
    remap: &mut HashMap<usize, usize>,
    next: &mut usize,
    out_meta: &mut HashMap<usize, (f64, Option<String>)>,
) -> usize {
    if let Some(&already) = remap.get(&old) {
        return already;
    }
    let fresh = *next;
    *next += 1;
    remap.insert(old, fresh);

    let info = src_meta.get(&old);
    let cohesion = info.map_or(0.0, |c| c.cohesion);
    let label = info.and_then(|c| c.label.clone()).map(|l| match tag {
        Some(t) => format!("{t}: {l}"),
        None => l,
    });
    out_meta.insert(fresh, (cohesion, label));
    fresh
}

/// Turn resolved node/edge/hyperedge sets into a graph, dropping anything that
/// references a node which did not survive, and rebuilding the community index.
fn assemble(
    nodes: Vec<GraphNode>,
    edges: Vec<GraphEdge>,
    hyperedges: Vec<Hyperedge>,
    community_meta: &HashMap<usize, (f64, Option<String>)>,
    stats: &mut MergeStats,
) -> Result<KnowledgeGraph> {
    let mut graph = KnowledgeGraph::new();
    let ids: HashSet<String> = nodes.iter().map(|n| n.id.clone()).collect();

    // Group before moving `nodes` into the graph.
    let mut grouped: HashMap<usize, Vec<String>> = HashMap::new();
    for node in &nodes {
        if let Some(cid) = node.community {
            grouped.entry(cid).or_default().push(node.id.clone());
        }
    }

    for node in nodes {
        graph.add_node(node)?;
    }

    for edge in edges {
        if ids.contains(&edge.source) && ids.contains(&edge.target) {
            graph.add_edge(edge)?;
        } else {
            stats.dangling_dropped += 1;
        }
    }

    let surviving: Vec<Hyperedge> = hyperedges
        .into_iter()
        .filter(|h| {
            let ok = h.nodes.iter().all(|n| ids.contains(n));
            if !ok {
                stats.dangling_dropped += 1;
            }
            ok
        })
        .collect();
    graph.set_hyperedges(surviving);

    let mut communities: Vec<CommunityInfo> = grouped
        .into_iter()
        .map(|(id, nodes)| {
            let (cohesion, label) = community_meta.get(&id).cloned().unwrap_or((0.0, None));
            CommunityInfo {
                id,
                nodes,
                cohesion,
                label,
            }
        })
        .collect();
    communities.sort_by_key(|c| c.id);
    graph.communities = communities;

    stats.nodes = graph.node_count();
    stats.edges = graph.edge_count();
    stats.hyperedges = graph.hyperedges.len();
    if stats.dangling_dropped > 0 {
        debug!("merge: dropped {} dangling item(s)", stats.dangling_dropped);
    }
    Ok(graph)
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphify_core::confidence::Confidence;
    use graphify_core::model::NodeType;

    fn node(id: &str) -> GraphNode {
        GraphNode {
            id: id.into(),
            label: id.into(),
            source_file: "test.rs".into(),
            source_location: None,
            node_type: NodeType::Class,
            community: None,
            extra: HashMap::new(),
        }
    }

    fn edge(src: &str, tgt: &str) -> GraphEdge {
        GraphEdge {
            source: src.into(),
            target: tgt.into(),
            relation: "calls".into(),
            confidence: Confidence::Extracted,
            confidence_score: 1.0,
            source_file: "test.rs".into(),
            source_location: None,
            weight: 1.0,
            provenance: None,
            extra: HashMap::new(),
        }
    }

    /// Build a graph from loose nodes/edges, panicking on programming errors.
    fn graph_of(nodes: Vec<GraphNode>, edges: Vec<GraphEdge>) -> KnowledgeGraph {
        let mut g = KnowledgeGraph::new();
        for n in nodes {
            g.add_node(n).unwrap();
        }
        for e in edges {
            g.add_edge(e).unwrap();
        }
        g
    }

    // --- merge_graphs ---------------------------------------------------------

    #[test]
    fn disjoint_graphs_union_cleanly() {
        let a = graph_of(vec![node("a"), node("b")], vec![edge("a", "b")]);
        let b = graph_of(vec![node("c"), node("d")], vec![edge("c", "d")]);

        let (merged, stats) =
            merge_graphs(&[MergeInput::untagged(&a), MergeInput::untagged(&b)]).unwrap();

        assert_eq!(stats.nodes, 4);
        assert_eq!(stats.edges, 2);
        assert_eq!(stats.duplicate_nodes, 0);
        assert_eq!(stats.node_conflicts, 0);
        assert_eq!(merged.node_count(), 4);
    }

    #[test]
    fn overlapping_nodes_collapse_once() {
        let a = graph_of(vec![node("shared"), node("a")], vec![edge("shared", "a")]);
        let b = graph_of(vec![node("shared"), node("b")], vec![edge("shared", "b")]);

        let (merged, stats) =
            merge_graphs(&[MergeInput::untagged(&a), MergeInput::untagged(&b)]).unwrap();

        assert_eq!(merged.node_count(), 3);
        assert_eq!(merged.edge_count(), 2);
        assert_eq!(stats.duplicate_nodes, 1);
        // Identical records, so nothing to reconcile.
        assert_eq!(stats.node_conflicts, 0);
    }

    #[test]
    fn conflicting_attributes_keep_first_input() {
        let mut first = node("dup");
        first.label = "FromA".into();
        first.source_file = "a.rs".into();
        let mut second = node("dup");
        second.label = "FromB".into();
        second.source_file = "b.rs".into();

        let a = graph_of(vec![first], vec![]);
        let b = graph_of(vec![second], vec![]);

        let (merged, stats) =
            merge_graphs(&[MergeInput::untagged(&a), MergeInput::untagged(&b)]).unwrap();

        assert_eq!(stats.node_conflicts, 1);
        let kept = merged.get_node("dup").unwrap();
        assert_eq!(kept.label, "FromA");
        // No blending: the whole record comes from one input.
        assert_eq!(kept.source_file, "a.rs");
    }

    #[test]
    fn tagging_keeps_same_named_nodes_apart() {
        let a = graph_of(vec![node("main")], vec![]);
        let b = graph_of(vec![node("main")], vec![]);

        let (merged, stats) =
            merge_graphs(&[MergeInput::tagged("api", &a), MergeInput::tagged("web", &b)]).unwrap();

        assert_eq!(stats.nodes, 2);
        assert_eq!(stats.duplicate_nodes, 0);
        let api = merged.get_node("api::main").unwrap();
        assert_eq!(api.extra["repo"], Value::String("api".into()));
        assert_eq!(api.extra["local_id"], Value::String("main".into()));
        assert!(merged.get_node("web::main").is_some());
    }

    #[test]
    fn tagged_edges_follow_their_endpoints() {
        let a = graph_of(vec![node("x"), node("y")], vec![edge("x", "y")]);
        let (merged, stats) = merge_graphs(&[MergeInput::tagged("repo1", &a)]).unwrap();

        assert_eq!(stats.edges, 1);
        assert_eq!(stats.dangling_dropped, 0);
        let e = merged.edges()[0];
        assert_eq!(e.source, "repo1::x");
        assert_eq!(e.target, "repo1::y");
    }

    #[test]
    fn duplicate_edges_collapse_keeping_best_confidence() {
        let mut weak = edge("a", "b");
        weak.confidence = Confidence::Inferred;
        weak.confidence_score = 0.4;
        let strong = edge("a", "b");

        let a = graph_of(vec![node("a"), node("b")], vec![weak]);
        let b = graph_of(vec![node("a"), node("b")], vec![strong]);

        let (merged, stats) =
            merge_graphs(&[MergeInput::untagged(&a), MergeInput::untagged(&b)]).unwrap();

        assert_eq!(merged.edge_count(), 1);
        assert_eq!(stats.duplicate_edges, 1);
        assert_eq!(merged.edges()[0].confidence_score, 1.0);
    }

    #[test]
    fn opposite_direction_edges_are_distinct() {
        let a = graph_of(vec![node("a"), node("b")], vec![edge("a", "b")]);
        let b = graph_of(vec![node("a"), node("b")], vec![edge("b", "a")]);

        let (merged, _) =
            merge_graphs(&[MergeInput::untagged(&a), MergeInput::untagged(&b)]).unwrap();
        assert_eq!(merged.edge_count(), 2);
    }

    #[test]
    fn communities_are_renumbered_into_disjoint_ranges() {
        let mut a0 = node("a");
        a0.community = Some(0);
        let mut a1 = node("b");
        a1.community = Some(1);
        let mut ga = graph_of(vec![a0, a1], vec![]);
        ga.communities = vec![
            CommunityInfo {
                id: 0,
                nodes: vec!["a".into()],
                cohesion: 0.9,
                label: Some("auth".into()),
            },
            CommunityInfo {
                id: 1,
                nodes: vec!["b".into()],
                cohesion: 0.5,
                label: None,
            },
        ];

        // Same community ids, entirely different meaning.
        let mut b0 = node("c");
        b0.community = Some(0);
        let mut gb = graph_of(vec![b0], vec![]);
        gb.communities = vec![CommunityInfo {
            id: 0,
            nodes: vec!["c".into()],
            cohesion: 0.7,
            label: Some("auth".into()),
        }];

        let (merged, _) = merge_graphs(&[
            MergeInput::tagged("api", &ga),
            MergeInput::tagged("web", &gb),
        ])
        .unwrap();

        let api_a = merged.get_node("api::a").unwrap().community.unwrap();
        let web_c = merged.get_node("web::c").unwrap().community.unwrap();
        assert_ne!(
            api_a, web_c,
            "per-graph community ids must not be conflated"
        );
        assert_eq!(merged.communities.len(), 3);

        // Labels stay distinguishable and cohesion rides along.
        let labels: Vec<Option<String>> =
            merged.communities.iter().map(|c| c.label.clone()).collect();
        assert!(labels.contains(&Some("api: auth".into())));
        assert!(labels.contains(&Some("web: auth".into())));
        let api_info = merged
            .communities
            .iter()
            .find(|c| c.id == api_a)
            .expect("community present");
        assert_eq!(api_info.cohesion, 0.9);
    }

    #[test]
    fn hyperedges_dedupe_and_drop_when_members_vanish() {
        let mut a = graph_of(vec![node("a"), node("b")], vec![]);
        a.set_hyperedges(vec![
            Hyperedge {
                nodes: vec!["a".into(), "b".into()],
                relation: "coexist".into(),
                label: "pair".into(),
            },
            Hyperedge {
                nodes: vec!["a".into(), "ghost".into()],
                relation: "coexist".into(),
                label: "broken".into(),
            },
        ]);
        let mut b = graph_of(vec![node("a"), node("b")], vec![]);
        b.set_hyperedges(vec![Hyperedge {
            nodes: vec!["a".into(), "b".into()],
            relation: "coexist".into(),
            label: "pair".into(),
        }]);

        let (merged, stats) =
            merge_graphs(&[MergeInput::untagged(&a), MergeInput::untagged(&b)]).unwrap();

        assert_eq!(merged.hyperedges.len(), 1, "duplicate hyperedge collapsed");
        assert_eq!(stats.dangling_dropped, 1, "hyperedge with a missing member");
    }

    #[test]
    fn merging_a_single_graph_is_a_no_op() {
        let a = graph_of(vec![node("a"), node("b")], vec![edge("a", "b")]);
        let (merged, stats) = merge_graphs(&[MergeInput::untagged(&a)]).unwrap();
        assert_eq!(stats.inputs, 1);
        assert_eq!(merged.node_count(), 2);
        assert_eq!(merged.edge_count(), 1);
    }

    #[test]
    fn merging_nothing_yields_an_empty_graph() {
        let (merged, stats) = merge_graphs(&[]).unwrap();
        assert_eq!(merged.node_count(), 0);
        assert_eq!(stats.nodes, 0);
    }

    // --- three_way_merge ------------------------------------------------------

    #[test]
    fn both_sides_added_different_nodes() {
        let base = graph_of(vec![node("shared")], vec![]);
        let ours = graph_of(
            vec![node("shared"), node("ours_only")],
            vec![edge("shared", "ours_only")],
        );
        let theirs = graph_of(
            vec![node("shared"), node("theirs_only")],
            vec![edge("shared", "theirs_only")],
        );

        let (merged, stats) = three_way_merge(&base, &ours, &theirs).unwrap();

        assert_eq!(merged.node_count(), 3);
        assert_eq!(merged.edge_count(), 2);
        assert_eq!(stats.deletions_honored, 0);
        assert_eq!(stats.node_conflicts, 0);
        assert!(merged.get_node("ours_only").is_some());
        assert!(merged.get_node("theirs_only").is_some());
    }

    #[test]
    fn deletion_on_one_side_is_honored() {
        let base = graph_of(vec![node("a"), node("gone")], vec![edge("a", "gone")]);
        let ours = graph_of(vec![node("a"), node("gone")], vec![edge("a", "gone")]);
        let theirs = graph_of(vec![node("a")], vec![]);

        let (merged, stats) = three_way_merge(&base, &ours, &theirs).unwrap();

        assert!(merged.get_node("gone").is_none());
        assert_eq!(merged.node_count(), 1);
        assert_eq!(merged.edge_count(), 0);
        assert!(stats.deletions_honored >= 1);
    }

    #[test]
    fn one_sided_edit_wins_over_the_unchanged_side() {
        let base = graph_of(vec![node("a")], vec![]);
        let ours = graph_of(vec![node("a")], vec![]);
        let mut renamed = node("a");
        renamed.label = "Renamed".into();
        let theirs = graph_of(vec![renamed], vec![]);

        let (merged, stats) = three_way_merge(&base, &ours, &theirs).unwrap();

        assert_eq!(merged.get_node("a").unwrap().label, "Renamed");
        assert_eq!(
            stats.node_conflicts, 0,
            "not a conflict — only one side moved"
        );
    }

    #[test]
    fn divergent_edits_resolve_to_ours() {
        let base = graph_of(vec![node("a")], vec![]);
        let mut mine = node("a");
        mine.label = "Mine".into();
        let mut yours = node("a");
        yours.label = "Yours".into();

        let (merged, stats) = three_way_merge(
            &base,
            &graph_of(vec![mine], vec![]),
            &graph_of(vec![yours], vec![]),
        )
        .unwrap();

        assert_eq!(merged.get_node("a").unwrap().label, "Mine");
        assert_eq!(stats.node_conflicts, 1);
    }

    #[test]
    fn theirs_only_nodes_are_left_unclustered() {
        let base = KnowledgeGraph::new();
        let mut ours_node = node("a");
        ours_node.community = Some(0);
        let ours = graph_of(vec![ours_node], vec![]);
        let mut theirs_node = node("b");
        theirs_node.community = Some(0); // same id, different meaning
        let theirs = graph_of(vec![theirs_node], vec![]);

        let (merged, _) = three_way_merge(&base, &ours, &theirs).unwrap();

        assert_eq!(merged.get_node("a").unwrap().community, Some(0));
        assert_eq!(
            merged.get_node("b").unwrap().community,
            None,
            "theirs' numbering is incomparable with ours'"
        );
    }

    #[test]
    fn unclustered_ours_adopts_their_communities() {
        let base = KnowledgeGraph::new();
        let ours = graph_of(vec![node("a")], vec![]);
        let mut theirs_a = node("a");
        theirs_a.community = Some(2);
        let mut theirs_b = node("b");
        theirs_b.community = Some(2);
        let mut theirs = graph_of(vec![theirs_a, theirs_b], vec![]);
        theirs.communities = vec![CommunityInfo {
            id: 2,
            nodes: vec!["a".into(), "b".into()],
            cohesion: 0.8,
            label: Some("core".into()),
        }];

        let (merged, _) = three_way_merge(&base, &ours, &theirs).unwrap();

        assert_eq!(merged.get_node("a").unwrap().community, Some(2));
        assert_eq!(merged.get_node("b").unwrap().community, Some(2));
        assert_eq!(merged.communities.len(), 1);
        assert_eq!(merged.communities[0].label.as_deref(), Some("core"));
    }

    #[test]
    fn empty_base_falls_back_to_plain_union() {
        let base = KnowledgeGraph::new();
        let ours = graph_of(vec![node("a")], vec![]);
        let theirs = graph_of(vec![node("b")], vec![]);

        let (merged, stats) = three_way_merge(&base, &ours, &theirs).unwrap();
        assert_eq!(merged.node_count(), 2);
        assert_eq!(stats.deletions_honored, 0);
    }

    #[test]
    fn edges_added_on_both_sides_survive_together() {
        let base = graph_of(vec![node("a"), node("b"), node("c")], vec![]);
        let ours = graph_of(vec![node("a"), node("b"), node("c")], vec![edge("a", "b")]);
        let theirs = graph_of(vec![node("a"), node("b"), node("c")], vec![edge("b", "c")]);

        let (merged, _) = three_way_merge(&base, &ours, &theirs).unwrap();
        assert_eq!(merged.edge_count(), 2);
    }

    #[test]
    fn edges_dangling_after_a_delete_are_dropped() {
        // Their side removed `b`; our `a -> b` edge has nowhere to land.
        let base = graph_of(vec![node("a"), node("b")], vec![]);
        let ours = graph_of(vec![node("a"), node("b")], vec![edge("a", "b")]);
        let theirs = graph_of(vec![node("a")], vec![]);

        let (merged, stats) = three_way_merge(&base, &ours, &theirs).unwrap();
        assert_eq!(merged.node_count(), 1);
        assert_eq!(merged.edge_count(), 0);
        assert_eq!(stats.dangling_dropped, 1);
    }

    #[test]
    fn hyperedge_added_on_their_side_survives() {
        let base = graph_of(vec![node("a"), node("b")], vec![]);
        let ours = graph_of(vec![node("a"), node("b")], vec![]);
        let mut theirs = graph_of(vec![node("a"), node("b")], vec![]);
        theirs.set_hyperedges(vec![Hyperedge {
            nodes: vec!["a".into(), "b".into()],
            relation: "coexist".into(),
            label: "pair".into(),
        }]);

        let (merged, _) = three_way_merge(&base, &ours, &theirs).unwrap();
        assert_eq!(merged.hyperedges.len(), 1);
    }

    #[test]
    fn hyperedge_deleted_on_their_side_stays_deleted() {
        let pair = Hyperedge {
            nodes: vec!["a".into(), "b".into()],
            relation: "coexist".into(),
            label: "pair".into(),
        };
        let mut base = graph_of(vec![node("a"), node("b")], vec![]);
        base.set_hyperedges(vec![pair.clone()]);
        let mut ours = graph_of(vec![node("a"), node("b")], vec![]);
        ours.set_hyperedges(vec![pair]);
        let theirs = graph_of(vec![node("a"), node("b")], vec![]);

        let (merged, stats) = three_way_merge(&base, &ours, &theirs).unwrap();
        assert!(merged.hyperedges.is_empty());
        assert_eq!(stats.deletions_honored, 1);
    }

    #[test]
    fn identical_sides_merge_to_themselves() {
        let g = || graph_of(vec![node("a"), node("b")], vec![edge("a", "b")]);
        let (merged, stats) = three_way_merge(&g(), &g(), &g()).unwrap();
        assert_eq!(merged.node_count(), 2);
        assert_eq!(merged.edge_count(), 1);
        assert_eq!(stats.node_conflicts, 0);
        assert_eq!(stats.deletions_honored, 0);
    }

    // --- distinct_repo_tags ---------------------------------------------------

    #[test]
    fn repo_tags_use_the_directory_above_graphify_out() {
        let tags =
            distinct_repo_tags(&["api/graphify-out/graph.json", "web/graphify-out/graph.json"]);
        assert_eq!(tags, vec!["api", "web"]);
    }

    #[test]
    fn colliding_repo_tags_are_widened_then_suffixed() {
        let tags = distinct_repo_tags(&[
            "backend/src/graphify-out/graph.json",
            "frontend/src/graphify-out/graph.json",
        ]);
        assert_eq!(tags, vec!["backend_src", "frontend_src"]);

        // Identical paths cannot be widened apart, so the index suffix takes over.
        let same = distinct_repo_tags(&["a/graphify-out/graph.json", "a/graphify-out/graph.json"]);
        assert_eq!(same, vec!["a", "a-2"]);
    }

    #[test]
    fn rootless_paths_fall_back_to_a_placeholder_tag() {
        let tags = distinct_repo_tags(&["graph.json", "other.json"]);
        assert_eq!(tags, vec!["repo", "repo-2"]);
    }

    // --- prune_repo_from_graph ------------------------------------------------

    /// A node as `merge_graphs` writes it for a tagged input.
    fn owned_node(tag: &str, id: &str) -> GraphNode {
        let mut n = node(&format!("{tag}::{id}"));
        n.extra
            .insert("repo".into(), Value::String(tag.to_string()));
        n.extra.insert("local_id".into(), Value::String(id.into()));
        n
    }

    fn sorted_ids(graph: &KnowledgeGraph) -> Vec<String> {
        let mut ids = graph.node_ids();
        ids.sort();
        ids
    }

    #[test]
    fn pruning_drops_a_repos_nodes_and_every_edge_touching_them() {
        let graph = graph_of(
            vec![owned_node("api", "a"), owned_node("web", "b")],
            vec![
                edge("api::a", "web::b"), // cross-repo: dies with its endpoint
                edge("web::b", "api::a"),
            ],
        );

        let (pruned, removed) = prune_repo_from_graph(&graph, "api");

        assert_eq!(removed, 1);
        assert_eq!(sorted_ids(&pruned), vec!["web::b"]);
        assert_eq!(pruned.edge_count(), 0);
    }

    #[test]
    fn pruning_falls_back_to_the_id_prefix_for_older_graphs() {
        // Written before nodes carried `repo`: only the prefix identifies them.
        let graph = graph_of(
            vec![node("api::a"), node("web::b")],
            vec![edge("api::a", "web::b")],
        );

        let (pruned, removed) = prune_repo_from_graph(&graph, "api");

        assert_eq!(removed, 1);
        assert_eq!(sorted_ids(&pruned), vec!["web::b"]);
    }

    #[test]
    fn a_node_claiming_another_repo_survives_a_matching_prefix() {
        // The stated owner wins: this node happens to be named `api::…` but was
        // contributed by `web`, and pruning `api` must not take it.
        let mut impostor = node("api::a");
        impostor
            .extra
            .insert("repo".into(), Value::String("web".into()));
        let graph = graph_of(vec![impostor, owned_node("api", "b")], vec![]);

        let (pruned, removed) = prune_repo_from_graph(&graph, "api");

        assert_eq!(removed, 1);
        assert_eq!(sorted_ids(&pruned), vec!["api::a"]);
    }

    #[test]
    fn pruning_an_untracked_tag_changes_nothing() {
        let graph = graph_of(
            vec![owned_node("api", "a"), owned_node("api", "b")],
            vec![edge("api::a", "api::b")],
        );

        let (pruned, removed) = prune_repo_from_graph(&graph, "nope");

        assert_eq!(removed, 0);
        assert_eq!(pruned.node_count(), 2);
        assert_eq!(pruned.edge_count(), 1);
    }

    #[test]
    fn pruning_drops_hyperedges_and_communities_that_lost_members() {
        let mut graph = graph_of(
            vec![
                owned_node("api", "a"),
                owned_node("web", "b"),
                owned_node("web", "c"),
            ],
            vec![],
        );
        graph.set_hyperedges(vec![
            Hyperedge {
                nodes: vec!["api::a".into(), "web::b".into()],
                relation: "co-change".into(),
                label: "mixed".into(),
            },
            Hyperedge {
                nodes: vec!["web::b".into(), "web::c".into()],
                relation: "co-change".into(),
                label: "web only".into(),
            },
        ]);
        graph.communities = vec![
            CommunityInfo {
                id: 0,
                nodes: vec!["api::a".into()],
                cohesion: 0.5,
                label: Some("api::core".into()),
            },
            CommunityInfo {
                id: 1,
                nodes: vec!["api::a".into(), "web::b".into()],
                cohesion: 0.25,
                label: Some("shared".into()),
            },
        ];

        let (pruned, _) = prune_repo_from_graph(&graph, "api");

        // The all-api community is gone; the mixed one keeps only what is left.
        assert_eq!(pruned.communities.len(), 1);
        assert_eq!(pruned.communities[0].id, 1);
        assert_eq!(pruned.communities[0].nodes, vec!["web::b".to_string()]);
        assert_eq!(pruned.hyperedges.len(), 1);
        assert_eq!(pruned.hyperedges[0].label, "web only");
    }
}
