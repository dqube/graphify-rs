//! Path command: shortest connection between two nodes.
//!
//! The graph is undirected for traversal, so a path may cross an edge against
//! the direction it was recorded in. Each hop is rendered with the stored
//! direction so callers can tell "A calls B" from "B calls A".

use anyhow::{Context, Result, bail};
use colored::Colorize;
use std::collections::HashMap;
use std::path::Path;

use graphify_core::graph::KnowledgeGraph;
use graphify_core::model::GraphEdge;

use crate::cmd_explain::{rank_nodes, resolve_node};

/// A runner-up this close to the top match means the query was ambiguous.
const AMBIGUITY_RATIO: f64 = 0.10;

/// Print the shortest path between two nodes.
pub fn cmd_path(source_query: &str, target_query: &str, graph_path: &str) -> Result<()> {
    let path = Path::new(graph_path);
    if !path.exists() {
        bail!(
            "graph not found at {} — run `graphify-rs build` first (or pass --graph)",
            path.display()
        );
    }
    let graph = graphify_serve::load_graph(path)
        .with_context(|| format!("failed to load graph from {}", path.display()))?;

    let source = resolve_node(&graph, source_query)?;
    let target = resolve_node(&graph, target_query)?;

    // Both queries landing on one node makes the answer trivially zero hops,
    // which is almost never what the caller meant.
    if source.id == target.id {
        bail!(
            "'{source_query}' and '{target_query}' both resolved to the same node '{}'. \
             Use a more specific label or the exact node ID.",
            source.id
        );
    }

    for (name, query) in [("source", source_query), ("target", target_query)] {
        if let Some(warning) = ambiguity_warning(&graph, query) {
            eprintln!(
                "{} {name} match was ambiguous ({warning})",
                "warning:".yellow()
            );
        }
    }

    let Some(hops) = graph.shortest_path(&source.id, &target.id) else {
        println!(
            "No path found between '{}' and '{}'. They are in disconnected parts of the graph.",
            source.label, target.label
        );
        return Ok(());
    };

    print_path(&graph, &hops);
    Ok(())
}

/// Describe the runner-up when the top match was not decisive.
fn ambiguity_warning(graph: &KnowledgeGraph, query: &str) -> Option<String> {
    let ranked = rank_nodes(graph, query);
    let [(top, top_node), (runner, runner_node), ..] = ranked[..] else {
        return None;
    };
    if !top.is_close_to(runner, AMBIGUITY_RATIO) {
        return None;
    }
    Some(format!(
        "chose '{}' over '{}'",
        top_node.label, runner_node.label
    ))
}

/// The edge joining two nodes, and whether it points `from` -> `to`.
///
/// Parallel edges are possible, so prefer the most confident one and break
/// ties on relation name to keep output stable.
fn edge_between<'g>(
    edges: &HashMap<(&str, &str), Vec<&'g GraphEdge>>,
    from: &str,
    to: &str,
) -> Option<(&'g GraphEdge, bool)> {
    let best = |list: Option<&Vec<&'g GraphEdge>>| -> Option<&'g GraphEdge> {
        list?.iter().copied().max_by(|a, b| {
            a.confidence_score
                .partial_cmp(&b.confidence_score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.relation.cmp(&a.relation))
        })
    };
    if let Some(edge) = best(edges.get(&(from, to))) {
        return Some((edge, true));
    }
    best(edges.get(&(to, from))).map(|edge| (edge, false))
}

fn print_path(graph: &KnowledgeGraph, hops: &[String]) {
    let mut by_pair: HashMap<(&str, &str), Vec<&GraphEdge>> = HashMap::new();
    for (src, dst, edge) in graph.edges_with_endpoints() {
        by_pair.entry((src, dst)).or_default().push(edge);
    }

    let label_of = |id: &str| -> String {
        graph
            .get_node(id)
            .map_or_else(|| id.to_string(), |n| n.label.clone())
    };

    let count = hops.len() - 1;
    println!(
        "\n{} ({} {}):",
        "Shortest path".bold(),
        count.to_string().bold(),
        if count == 1 { "hop" } else { "hops" }
    );

    let mut line = String::from("  ");
    line.push_str(&label_of(&hops[0]).cyan().to_string());
    for pair in hops.windows(2) {
        let (from, to) = (pair[0].as_str(), pair[1].as_str());
        let segment = match edge_between(&by_pair, from, to) {
            Some((edge, forward)) => {
                let annotation = format!("{} [{}]", edge.relation, edge.confidence);
                if forward {
                    format!(" --{}--> ", annotation.dimmed())
                } else {
                    format!(" <--{}-- ", annotation.dimmed())
                }
            }
            // Should not happen for a BFS path, but never drop a hop.
            None => " --?--> ".to_string(),
        };
        line.push_str(&segment);
        line.push_str(&label_of(to).cyan().to_string());
    }
    println!("{line}\n");
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphify_core::confidence::Confidence;
    use graphify_core::model::{GraphNode, NodeType};

    fn node(id: &str, label: &str, node_type: NodeType) -> GraphNode {
        GraphNode {
            id: id.into(),
            label: label.into(),
            source_file: "src/lib.rs".into(),
            source_location: None,
            node_type,
            community: None,
            extra: HashMap::new(),
        }
    }

    fn edge(src: &str, dst: &str, relation: &str) -> GraphEdge {
        GraphEdge {
            source: src.into(),
            target: dst.into(),
            relation: relation.into(),
            confidence: Confidence::Extracted,
            confidence_score: 1.0,
            source_file: "src/lib.rs".into(),
            source_location: None,
            weight: 1.0,
            provenance: None,
            extra: HashMap::new(),
        }
    }

    /// a -> b -> c, with the last edge stored as c -> b (backwards along the path).
    fn chain() -> KnowledgeGraph {
        let mut g = KnowledgeGraph::new();
        g.add_node(node("id-a", "alpha", NodeType::Function))
            .unwrap();
        g.add_node(node("id-b", "beta", NodeType::Function))
            .unwrap();
        g.add_node(node("id-c", "gamma", NodeType::Function))
            .unwrap();
        g.add_edge(edge("id-a", "id-b", "calls")).unwrap();
        g.add_edge(edge("id-c", "id-b", "imports")).unwrap();
        g
    }

    #[test]
    fn finds_edge_direction_along_the_path() {
        let g = chain();
        let mut by_pair: HashMap<(&str, &str), Vec<&GraphEdge>> = HashMap::new();
        for (src, dst, e) in g.edges_with_endpoints() {
            by_pair.entry((src, dst)).or_default().push(e);
        }

        let (edge, forward) = edge_between(&by_pair, "id-a", "id-b").unwrap();
        assert_eq!(edge.relation, "calls");
        assert!(forward, "a -> b is stored in that direction");

        let (edge, forward) = edge_between(&by_pair, "id-b", "id-c").unwrap();
        assert_eq!(edge.relation, "imports");
        assert!(
            !forward,
            "the stored edge is c -> b, so this hop is backwards"
        );
    }

    #[test]
    fn prefers_the_most_confident_parallel_edge() {
        let mut g = KnowledgeGraph::new();
        g.add_node(node("id-a", "alpha", NodeType::Function))
            .unwrap();
        g.add_node(node("id-b", "beta", NodeType::Function))
            .unwrap();
        let mut weak = edge("id-a", "id-b", "references");
        weak.confidence = Confidence::Inferred;
        weak.confidence_score = 0.4;
        g.add_edge(weak).unwrap();
        g.add_edge(edge("id-a", "id-b", "calls")).unwrap();

        let mut by_pair: HashMap<(&str, &str), Vec<&GraphEdge>> = HashMap::new();
        for (src, dst, e) in g.edges_with_endpoints() {
            by_pair.entry((src, dst)).or_default().push(e);
        }
        let (edge, _) = edge_between(&by_pair, "id-a", "id-b").unwrap();
        assert_eq!(edge.relation, "calls");
    }

    #[test]
    fn traverses_edges_against_their_direction() {
        // BFS is undirected, so alpha reaches gamma even though the second
        // edge points the other way.
        let g = chain();
        let hops = g.shortest_path("id-a", "id-c").expect("path exists");
        assert_eq!(hops, vec!["id-a", "id-b", "id-c"]);
    }

    #[test]
    fn reports_no_path_between_disconnected_nodes() {
        let mut g = KnowledgeGraph::new();
        g.add_node(node("id-a", "alpha", NodeType::Function))
            .unwrap();
        g.add_node(node("id-z", "omega", NodeType::Function))
            .unwrap();
        assert!(g.shortest_path("id-a", "id-z").is_none());
    }

    #[test]
    fn flags_only_genuinely_ambiguous_matches() {
        let mut g = KnowledgeGraph::new();
        // Two substring matches on symbol nodes, both degree 0: a coin flip.
        g.add_node(node("id-1", "handler_one", NodeType::Function))
            .unwrap();
        g.add_node(node("id-2", "handler_two", NodeType::Function))
            .unwrap();
        assert!(ambiguity_warning(&g, "handler").is_some());

        // An exact ID hit is decisive: only one candidate.
        assert!(ambiguity_warning(&g, "id-1").is_none());

        // A clearly better-connected match is not ambiguous, even though the
        // two candidates are in the same tier.
        let mut g2 = KnowledgeGraph::new();
        g2.add_node(node("id-hot", "handler_one", NodeType::Function))
            .unwrap();
        g2.add_node(node("id-cold", "handler_two", NodeType::Function))
            .unwrap();
        for i in 0..8 {
            let id = format!("id-n{i}");
            g2.add_node(node(&id, &format!("n{i}"), NodeType::Function))
                .unwrap();
            g2.add_edge(edge("id-hot", &id, "calls")).unwrap();
        }
        assert!(
            ambiguity_warning(&g2, "handler").is_none(),
            "degree 8 vs 0 is a decisive win"
        );

        // An exact label beats a substring outright: different tiers.
        let mut g3 = KnowledgeGraph::new();
        g3.add_node(node("id-x", "handler", NodeType::Function))
            .unwrap();
        g3.add_node(node("id-y", "handler_two", NodeType::Function))
            .unwrap();
        assert!(ambiguity_warning(&g3, "handler").is_none());
    }

    #[test]
    fn ranks_exact_matches_and_symbols_first() {
        let mut g = KnowledgeGraph::new();
        g.add_node(node("id-file", "build", NodeType::File))
            .unwrap();
        g.add_node(node("id-fn", "build", NodeType::Function))
            .unwrap();
        g.add_node(node("id-sub", "rebuild_all", NodeType::Function))
            .unwrap();

        let ranked = rank_nodes(&g, "build");
        // Exact label beats substring; symbol beats file within the same tier.
        assert_eq!(ranked[0].1.id, "id-fn");
        assert_eq!(ranked[1].1.id, "id-file");
        assert_eq!(ranked[2].1.id, "id-sub");
        assert_eq!(ranked[0].0.tier, 2);
        assert_eq!(ranked[2].0.tier, 1);
    }
}
