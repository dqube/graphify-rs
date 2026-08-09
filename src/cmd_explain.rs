//! Explain command: show a node's metadata, community, and neighbors.

use anyhow::{Context, Result, bail};
use colored::Colorize;
use std::collections::HashMap;
use std::path::Path;

use graphify_core::graph::KnowledgeGraph;
use graphify_core::model::GraphNode;

/// Maximum neighbors shown per relation group.
const MAX_PER_GROUP: usize = 15;

/// Explain the node identified by name or ID.
pub fn cmd_explain(node_query: &str, graph_path: &str) -> Result<()> {
    let path = Path::new(graph_path);
    if !path.exists() {
        bail!(
            "graph not found at {} — run `graphify-rs build` first (or pass --graph)",
            path.display()
        );
    }
    let graph = graphify_serve::load_graph(path)
        .with_context(|| format!("failed to load graph from {}", path.display()))?;

    let node = resolve_node(&graph, node_query)?;
    print_explanation(&graph, node);
    Ok(())
}

/// Resolve a query to a node, or fail with suggestions.
pub(crate) fn resolve_node<'g>(graph: &'g KnowledgeGraph, query: &str) -> Result<&'g GraphNode> {
    if let Some(node) = find_node(graph, query) {
        return Ok(node);
    }
    let suggestions = suggest(graph, query);
    if suggestions.is_empty() {
        bail!("no node matching '{query}' found in the graph");
    }
    bail!(
        "no node matching '{query}'. Did you mean: {}",
        suggestions.join(", ")
    );
}

/// Best node matching `query`, if any.
fn find_node<'g>(graph: &'g KnowledgeGraph, query: &str) -> Option<&'g GraphNode> {
    rank_nodes(graph, query).into_iter().next().map(|(_, n)| n)
}

/// How well a node matched a query, ordered best-first by `Ord`.
///
/// Kept as separate fields rather than one blended number: the match kind and
/// node kind are categorical, so collapsing them into a score with large
/// constants would make every comparison look like a near-tie.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct MatchScore {
    /// 3 = exact ID, 2 = exact label, 1 = substring.
    pub tier: u8,
    /// Symbol nodes outrank file nodes within a tier.
    pub is_symbol: bool,
    pub degree: usize,
}

impl MatchScore {
    /// True when `other` is close enough that picking between them is a
    /// coin flip: same kind of match, on a node of the same kind, with a
    /// degree within `ratio` of this one.
    pub(crate) fn is_close_to(self, other: Self, ratio: f64) -> bool {
        if self.tier != other.tier || self.is_symbol != other.is_symbol {
            return false;
        }
        if self.degree == 0 {
            return other.degree == 0;
        }
        let gap = self.degree.saturating_sub(other.degree) as f64;
        (gap / self.degree as f64) < ratio
    }
}

/// Score every node matching `query`, best first.
///
/// Tiers: exact ID, then exact label (case-insensitive), then substring.
/// Within a tier, symbol nodes outrank file nodes, then higher degree wins.
pub(crate) fn rank_nodes<'g>(
    graph: &'g KnowledgeGraph,
    query: &str,
) -> Vec<(MatchScore, &'g GraphNode)> {
    let score = |n: &GraphNode, tier: u8| MatchScore {
        tier,
        is_symbol: !matches!(n.node_type, graphify_core::model::NodeType::File),
        degree: graph.degree(&n.id),
    };

    if let Some(node) = graph.get_node(query) {
        return vec![(score(node, 3), node)];
    }
    let q = query.to_lowercase();
    let mut scored: Vec<(MatchScore, &GraphNode)> = graph
        .nodes()
        .into_iter()
        .filter_map(|n| {
            let label = n.label.to_lowercase();
            let tier = if label == q {
                2
            } else if label.contains(&q) {
                1
            } else {
                return None;
            };
            Some((score(n, tier), n))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.id.cmp(&b.1.id)));
    scored
}

/// Up to five label suggestions for a failed lookup.
pub(crate) fn suggest(graph: &KnowledgeGraph, query: &str) -> Vec<String> {
    let q = query.to_lowercase();
    let mut partial: Vec<(&GraphNode, usize)> = graph
        .nodes()
        .into_iter()
        .filter(|n| {
            let label = n.label.to_lowercase();
            label.contains(&q)
                || q.split(['_', '-', ' ', ':'])
                    .filter(|p| p.len() >= 3)
                    .any(|part| label.contains(part))
        })
        .map(|n| (n, graph.degree(&n.id)))
        .collect();
    partial.sort_by_key(|(_, d)| std::cmp::Reverse(*d));
    partial.truncate(5);
    partial.into_iter().map(|(n, _)| n.label.clone()).collect()
}

fn print_explanation(graph: &KnowledgeGraph, node: &GraphNode) {
    println!(
        "\n{} {} {}",
        "●".cyan().bold(),
        node.label.bold(),
        format!("({})", node.node_type).dimmed()
    );
    println!("  {} {}", "ID:".dimmed(), node.id);
    let location = match &node.source_location {
        Some(loc) => format!("{}:{}", node.source_file, loc),
        None => node.source_file.clone(),
    };
    println!("  {} {}", "File:".dimmed(), location);

    if let Some(cid) = node.community {
        let (size, label) = graph
            .communities
            .iter()
            .find(|c| c.id == cid)
            .map(|c| (c.nodes.len(), c.label.clone()))
            .unwrap_or((0, None));
        let label_str = label.map(|l| format!(" \"{l}\"")).unwrap_or_default();
        println!(
            "  {} {cid}{label_str} ({size} nodes)",
            "Community:".dimmed()
        );
    }

    let mut outgoing: NeighborGroups = HashMap::new();
    let mut incoming: NeighborGroups = HashMap::new();
    for (src, dst, edge) in graph.edges_with_endpoints() {
        if src == node.id {
            let label = graph.get_node(dst).map_or(dst, |n| n.label.as_str());
            outgoing.entry(edge.relation.as_str()).or_default().push((
                label.to_string(),
                edge.confidence_score,
                edge.confidence.to_string(),
            ));
        } else if dst == node.id {
            let label = graph.get_node(src).map_or(src, |n| n.label.as_str());
            incoming.entry(edge.relation.as_str()).or_default().push((
                label.to_string(),
                edge.confidence_score,
                edge.confidence.to_string(),
            ));
        }
    }

    let out_count: usize = outgoing.values().map(Vec::len).sum();
    let in_count: usize = incoming.values().map(Vec::len).sum();
    println!(
        "  {} {} ({} outgoing, {} incoming)",
        "Degree:".dimmed(),
        (out_count + in_count).to_string().bold(),
        out_count,
        in_count
    );

    print_groups(&outgoing, "Outgoing", "→");
    print_groups(&incoming, "Incoming", "←");
    println!();
}

/// A neighbor entry: (label, confidence score, confidence level).
type NeighborEntry = (String, f64, String);
/// Neighbors grouped by relation name.
type NeighborGroups<'a> = HashMap<&'a str, Vec<NeighborEntry>>;

fn print_groups(groups: &NeighborGroups<'_>, title: &str, arrow: &str) {
    if groups.is_empty() {
        return;
    }
    println!("\n  {} edges:", title.bold());
    let mut rels: Vec<(&&str, &Vec<NeighborEntry>)> = groups.iter().collect();
    rels.sort_by_key(|(rel, items)| (std::cmp::Reverse(items.len()), **rel));
    for (rel, items) in rels {
        println!("    {} {} ({})", arrow, rel.cyan(), items.len());
        for (label, score, confidence) in items.iter().take(MAX_PER_GROUP) {
            println!(
                "      {} {} {}",
                label,
                format!("[{confidence}]").dimmed(),
                format!("{score:.2}").dimmed()
            );
        }
        if items.len() > MAX_PER_GROUP {
            println!("      … +{} more", items.len() - MAX_PER_GROUP);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphify_core::confidence::Confidence;
    use graphify_core::model::{GraphEdge, GraphNode, NodeType};

    fn make_graph() -> KnowledgeGraph {
        let mut g = KnowledgeGraph::new();
        let node = |name: &str, file: &str| GraphNode {
            id: format!("id-{name}"),
            label: name.into(),
            source_file: file.into(),
            source_location: None,
            node_type: NodeType::Function,
            community: None,
            extra: HashMap::new(),
        };
        g.add_node(node("alpha", "a.rs")).unwrap();
        g.add_node(node("beta", "b.rs")).unwrap();
        g.add_node(node("alphabet", "c.rs")).unwrap();
        g.add_edge(GraphEdge {
            source: "id-alpha".into(),
            target: "id-beta".into(),
            relation: "calls".into(),
            confidence: Confidence::Extracted,
            confidence_score: 1.0,
            source_file: "a.rs".into(),
            source_location: None,
            weight: 1.0,
            provenance: None,
            extra: HashMap::new(),
        })
        .unwrap();
        g
    }

    #[test]
    fn finds_by_exact_id() {
        let g = make_graph();
        assert_eq!(find_node(&g, "id-alpha").unwrap().label, "alpha");
    }

    #[test]
    fn finds_by_label_case_insensitive() {
        let g = make_graph();
        assert_eq!(find_node(&g, "BETA").unwrap().id, "id-beta");
    }

    #[test]
    fn substring_prefers_higher_degree() {
        let g = make_graph();
        // "alph" matches both "alpha" and "alphabet"; alpha has degree 1.
        assert_eq!(find_node(&g, "alph").unwrap().label, "alpha");
    }

    #[test]
    fn unknown_returns_none() {
        let g = make_graph();
        assert!(find_node(&g, "zzz").is_none());
    }

    #[test]
    fn suggestions_for_partial_match() {
        let g = make_graph();
        let s = suggest(&g, "bet");
        assert!(s.contains(&"beta".to_string()) || s.contains(&"alphabet".to_string()));
    }
}
