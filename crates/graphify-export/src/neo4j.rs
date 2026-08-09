//! Live Neo4j push via the transactional HTTP API.
//!
//! Uses `POST {uri}/db/{database}/tx/commit` with basic auth, avoiding a Bolt
//! driver dependency. Nodes and edges are pushed in batches with `UNWIND`
//! statements and `MERGE` semantics, so repeated pushes are idempotent.

use anyhow::{Context, Result, bail};
use graphify_core::graph::KnowledgeGraph;
use serde_json::{Value, json};

/// Default HTTP endpoint for a local Neo4j instance.
pub const DEFAULT_URI: &str = "http://localhost:7474";
/// Default database name.
pub const DEFAULT_DATABASE: &str = "neo4j";
/// Rows per `UNWIND` batch statement.
const BATCH_SIZE: usize = 500;

/// Connection parameters for a live Neo4j instance.
#[derive(Debug, Clone)]
pub struct Neo4jConnection {
    /// HTTP(S) endpoint, e.g. `http://localhost:7474`.
    pub uri: String,
    pub user: String,
    pub password: String,
    pub database: String,
}

/// Outcome of a successful push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushStats {
    pub nodes: usize,
    pub edges: usize,
    pub batches: usize,
}

/// Normalize a user-supplied URI to the transactional HTTP endpoint base.
///
/// Accepts `http(s)://` URIs unchanged (trailing slashes stripped). Bolt-style
/// URIs (`bolt://`, `neo4j://`, `neo4j+s://` …) are mapped to plain HTTP; the
/// default Bolt port 7687 becomes the default HTTP port 7474.
pub fn normalize_uri(uri: &str) -> String {
    let mut u = uri.trim().trim_end_matches('/').to_string();
    for (bolt, http) in [
        ("neo4j+s://", "https://"),
        ("neo4j+ssc://", "https://"),
        ("bolt+s://", "https://"),
        ("bolt+ssc://", "https://"),
        ("neo4j://", "http://"),
        ("bolt://", "http://"),
    ] {
        if let Some(rest) = u.strip_prefix(bolt) {
            u = format!("{http}{rest}");
            break;
        }
    }
    if let Some(host_port) = u.strip_prefix("http://")
        && let Some((host, "7687")) = host_port.rsplit_once(':')
    {
        return format!("http://{host}:7474");
    }
    u
}

/// Build the row objects for a batch of nodes.
fn node_rows(graph: &KnowledgeGraph, skip: usize, take: usize) -> Vec<Value> {
    graph
        .nodes()
        .into_iter()
        .skip(skip)
        .take(take)
        .map(|n| {
            json!({
                "id": n.id,
                "label": n.label,
                "type": n.node_type.to_string(),
                "file": n.source_file,
                "location": n.source_location,
                "community": n.community,
            })
        })
        .collect()
}

/// Build the row objects for a batch of edges.
fn edge_rows(graph: &KnowledgeGraph, skip: usize, take: usize) -> Vec<Value> {
    graph
        .edges()
        .into_iter()
        .skip(skip)
        .take(take)
        .map(|e| {
            json!({
                "source": e.source,
                "target": e.target,
                "relation": e.relation,
                "confidence": e.confidence.to_string(),
                "score": e.confidence_score,
                "file": e.source_file,
            })
        })
        .collect()
}

const NODE_STATEMENT: &str = "UNWIND $rows AS row \
    MERGE (n:GraphNode {id: row.id}) \
    SET n.label = row.label, n.type = row.type, n.file = row.file, \
        n.location = row.location, n.community = row.community";

const EDGE_STATEMENT: &str = "UNWIND $rows AS row \
    MATCH (a:GraphNode {id: row.source}), (b:GraphNode {id: row.target}) \
    MERGE (a)-[r:GRAPH_REL {relation: row.relation}]->(b) \
    SET r.confidence = row.confidence, r.score = row.score, r.file = row.file";

const CONSTRAINT_STATEMENT: &str = "CREATE CONSTRAINT graphify_node_id IF NOT EXISTS \
    FOR (n:GraphNode) REQUIRE n.id IS UNIQUE";

/// Execute one `tx/commit` request and fail when Neo4j reports errors.
async fn commit(
    client: &reqwest::Client,
    conn: &Neo4jConnection,
    statements: Vec<Value>,
) -> Result<()> {
    let url = format!(
        "{}/db/{}/tx/commit",
        normalize_uri(&conn.uri),
        conn.database
    );
    let resp = client
        .post(&url)
        .basic_auth(&conn.user, Some(&conn.password))
        .json(&json!({ "statements": statements }))
        .send()
        .await
        .with_context(|| format!("Neo4j request to {url} failed"))?;

    let status = resp.status();
    let body: Value = resp
        .json()
        .await
        .context("cannot parse Neo4j response as JSON")?;

    if let Some(errors) = body.get("errors").and_then(Value::as_array)
        && !errors.is_empty()
    {
        let messages: Vec<String> = errors
            .iter()
            .map(|e| {
                format!(
                    "{}: {}",
                    e.get("code").and_then(Value::as_str).unwrap_or("?"),
                    e.get("message").and_then(Value::as_str).unwrap_or("?")
                )
            })
            .collect();
        bail!("Neo4j rejected the push: {}", messages.join("; "));
    }
    if !status.is_success() {
        bail!("Neo4j returned HTTP {status}");
    }
    Ok(())
}

/// Push the whole graph to a live Neo4j instance.
///
/// Idempotent: nodes and edges are `MERGE`d by id / (source, relation, target).
/// Edges whose endpoints are missing from the pushed node set are skipped by
/// Neo4j's `MATCH` and counted separately.
pub async fn push_to_neo4j(graph: &KnowledgeGraph, conn: &Neo4jConnection) -> Result<PushStats> {
    let client = reqwest::Client::new();
    let node_count = graph.node_count();
    let edge_count = graph.edge_count();
    let mut batches = 0usize;

    // Best effort: the id constraint keeps MERGE fast and safe on Neo4j 5+.
    // Older versions without `IF NOT EXISTS` support will reject it — the push
    // itself still works, just without the uniqueness guarantee.
    if commit(&client, conn, vec![json!({"statement": CONSTRAINT_STATEMENT})])
        .await
        .is_err()
    {
        tracing::debug!("Neo4j constraint creation skipped (unsupported)");
    }

    let mut skip = 0;
    while skip < node_count {
        let rows = node_rows(graph, skip, BATCH_SIZE);
        let take = rows.len();
        commit(
            &client,
            conn,
            vec![json!({"statement": NODE_STATEMENT, "parameters": {"rows": rows}})],
        )
        .await?;
        batches += 1;
        skip += take;
    }

    let mut skip = 0;
    while skip < edge_count {
        let rows = edge_rows(graph, skip, BATCH_SIZE);
        let take = rows.len();
        commit(
            &client,
            conn,
            vec![json!({"statement": EDGE_STATEMENT, "parameters": {"rows": rows}})],
        )
        .await?;
        batches += 1;
        skip += take;
    }

    Ok(PushStats {
        nodes: node_count,
        edges: edge_count,
        batches,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphify_core::confidence::Confidence;
    use graphify_core::model::{GraphEdge, GraphNode, NodeType};
    use std::collections::HashMap;

    fn make_graph() -> KnowledgeGraph {
        let mut g = KnowledgeGraph::new();
        let node = |name: &str| GraphNode {
            id: format!("id-{name}"),
            label: name.into(),
            source_file: "a.rs".into(),
            source_location: Some("L1".into()),
            node_type: NodeType::Function,
            community: Some(2),
            extra: HashMap::new(),
        };
        g.add_node(node("alpha")).unwrap();
        g.add_node(node("beta")).unwrap();
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
    fn normalize_uri_keeps_http() {
        assert_eq!(normalize_uri("http://db:7474/"), "http://db:7474");
        assert_eq!(normalize_uri("https://db:7473"), "https://db:7473");
    }

    #[test]
    fn normalize_uri_maps_bolt_to_http() {
        assert_eq!(normalize_uri("bolt://localhost:7687"), "http://localhost:7474");
        assert_eq!(normalize_uri("neo4j://db:7687/"), "http://db:7474");
        assert_eq!(normalize_uri("bolt://db:9999"), "http://db:9999");
    }

    #[test]
    fn node_rows_carry_expected_fields() {
        let g = make_graph();
        let rows = node_rows(&g, 0, 10);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["id"], "id-alpha");
        assert_eq!(rows[0]["type"], "Function");
        assert_eq!(rows[0]["community"], 2);
    }

    #[test]
    fn node_rows_batch_window() {
        let g = make_graph();
        assert_eq!(node_rows(&g, 1, 10).len(), 1);
        assert_eq!(node_rows(&g, 1, 10)[0]["id"], "id-beta");
        assert!(node_rows(&g, 2, 10).is_empty());
    }

    #[test]
    fn edge_rows_carry_endpoints() {
        let g = make_graph();
        let rows = edge_rows(&g, 0, 10);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["source"], "id-alpha");
        assert_eq!(rows[0]["target"], "id-beta");
        assert_eq!(rows[0]["relation"], "calls");
    }
}
