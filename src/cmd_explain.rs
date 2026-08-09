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

    let node = match find_node(&graph, node_query) {
        Some(n) => n,
        None => {
            let suggestions = suggest(&graph, node_query);
            if suggestions.is_empty() {
                bail!("no node matching '{node_query}' found in the graph");
            }
            bail!(
                "no node matching '{node_query}'. Did you mean: {}",
                suggestions.join(", ")
            );
        }
    };

    print_explanation(&graph, node);
    Ok(())
}

/// Resolve a query string to a node: exact ID, exact label (case-insensitive),
/// then substring match. On ties, prefer symbol nodes over file nodes, then
/// the highest-degree candidate.
fn find_node<'g>(graph: &'g KnowledgeGraph, query: &str) -> Option<&'g GraphNode> {
    if let Some(n) = graph.get_node(query) {
        return Some(n);
    }
    let rank = |n: &GraphNode| {
        let is_file = matches!(n.node_type, graphify_core::model::NodeType::File);
        (!is_file, graph.degree(&n.id))
    };
    let q = query.to_lowercase();
    let exact: Vec<&GraphNode> = graph
        .nodes()
        .into_iter()
        .filter(|n| n.label.to_lowercase() == q)
        .collect();
    if !exact.is_empty() {
        return exact.into_iter().max_by_key(|n| rank(n));
    }
    graph
        .nodes()
        .into_iter()
        .filter(|n| n.label.to_lowercase().contains(&q))
        .max_by_key(|n| rank(n))
}

/// Up to five label suggestions for a failed lookup.
fn suggest(graph: &KnowledgeGraph, query: &str) -> Vec<String> {
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

fn print_groups(
    groups: &NeighborGroups<'_>,
    title: &str,
    arrow: &str,
) {
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
