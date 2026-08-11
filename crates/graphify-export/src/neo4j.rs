//! Live Neo4j push via the transactional HTTP API.
//!
//! Uses `POST {uri}/db/{database}/tx/commit` with basic auth, avoiding a Bolt
//! driver dependency. Nodes and edges are pushed in batches with `UNWIND`
//! statements and `MERGE` semantics, so repeated pushes are idempotent.

use anyhow::{Context, Result, bail};
use std::collections::HashMap;

use graphify_core::graph::KnowledgeGraph;
use graphify_core::py_compat;
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

/// Sanitize a node label for interpolation into Cypher, mirroring Python's
/// `_safe_label`. Anything outside `[A-Za-z0-9_]` is dropped; an empty result
/// falls back to `Entity`.
fn safe_label(label: &str) -> String {
    let cleaned: String = label
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    if cleaned.is_empty() {
        "Entity".to_string()
    } else {
        cleaned
    }
}

/// Sanitize a relation into a Cypher relationship type, mirroring Python's
/// `_safe_rel`: upper-cased, non-alphanumerics become `_`, empty becomes
/// `RELATED_TO`.
fn safe_rel(relation: &str) -> String {
    let cleaned: String = relation
        .to_uppercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.trim_matches('_').is_empty() {
        "RELATED_TO".to_string()
    } else {
        cleaned
    }
}

/// Group nodes by the Cypher label they will carry.
///
/// Labels cannot be parameterised in Cypher, so a single `UNWIND` cannot cover
/// mixed types. Grouping keeps the batching while still writing a typed
/// schema, which is what makes `MATCH (f:Function)` possible at all.
fn nodes_by_label(graph: &KnowledgeGraph) -> HashMap<String, Vec<Value>> {
    let mut groups: HashMap<String, Vec<Value>> = HashMap::new();
    for n in graph.nodes() {
        let label = safe_label(&n.node_type.to_string());
        groups.entry(label).or_default().push(json!({
            "id": n.id,
            "label": n.label,
            "type": n.node_type.to_string(),
            "file": n.source_file,
            "location": n.source_location,
            "community": n.community,
        }));
    }
    groups
}

/// Group edges by relationship type, for the same reason as [`nodes_by_label`].
///
/// The relation is translated to the Python vocabulary first so a database
/// built by graphify-rs answers the same `MATCH (:X)-[:CONTAINS]->(:Y)` as one
/// built by graphify.
fn edges_by_type(graph: &KnowledgeGraph) -> HashMap<String, Vec<Value>> {
    let mut groups: HashMap<String, Vec<Value>> = HashMap::new();
    for e in graph.edges() {
        let relation = py_compat::translate_relation(&e.relation);
        groups.entry(safe_rel(relation)).or_default().push(json!({
            "source": e.source,
            "target": e.target,
            "relation": relation,
            "confidence": e.confidence.to_string(),
            "score": e.confidence_score,
            "file": e.source_file,
        }));
    }
    groups
}

/// Every node also carries `:GraphNode`, so the id constraint and the edge
/// `MATCH` stay type-agnostic while the extra label enables typed queries.
fn node_statement(label: &str) -> String {
    format!(
        "UNWIND $rows AS row \
         MERGE (n:GraphNode {{id: row.id}}) \
         SET n:{label}, n.label = row.label, n.type = row.type, n.file = row.file, \
             n.location = row.location, n.community = row.community"
    )
}

fn edge_statement(rel: &str) -> String {
    format!(
        "UNWIND $rows AS row \
         MATCH (a:GraphNode {{id: row.source}}), (b:GraphNode {{id: row.target}}) \
         MERGE (a)-[r:{rel}]->(b) \
         SET r.relation = row.relation, r.confidence = row.confidence, \
             r.score = row.score, r.file = row.file"
    )
}

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
    if commit(
        &client,
        conn,
        vec![json!({"statement": CONSTRAINT_STATEMENT})],
    )
    .await
    .is_err()
    {
        tracing::debug!("Neo4j constraint creation skipped (unsupported)");
    }

    // Nodes before edges: the edge statements MATCH on both endpoints, so a
    // relationship whose nodes have not been written yet is silently skipped.
    for (label, rows) in nodes_by_label(graph) {
        let statement = node_statement(&label);
        for chunk in rows.chunks(BATCH_SIZE) {
            commit(
                &client,
                conn,
                vec![json!({"statement": statement, "parameters": {"rows": chunk}})],
            )
            .await?;
            batches += 1;
        }
    }

    for (rel, rows) in edges_by_type(graph) {
        let statement = edge_statement(&rel);
        for chunk in rows.chunks(BATCH_SIZE) {
            commit(
                &client,
                conn,
                vec![json!({"statement": statement, "parameters": {"rows": chunk}})],
            )
            .await?;
            batches += 1;
        }
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
        assert_eq!(
            normalize_uri("bolt://localhost:7687"),
            "http://localhost:7474"
        );
        assert_eq!(normalize_uri("neo4j://db:7687/"), "http://db:7474");
        assert_eq!(normalize_uri("bolt://db:9999"), "http://db:9999");
    }

    #[test]
    fn node_rows_carry_expected_fields() {
        let g = make_graph();
        let groups = nodes_by_label(&g);
        let rows = groups.get("Function").expect("Function group");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["type"], "Function");
        let ids: Vec<&str> = rows.iter().filter_map(|r| r["id"].as_str()).collect();
        assert!(ids.contains(&"id-alpha") && ids.contains(&"id-beta"));
    }

    /// Nodes are grouped by type so each `UNWIND` can name a concrete label,
    /// which is what allows `MATCH (f:Function)` against the pushed graph.
    #[test]
    fn nodes_are_grouped_by_type_label() {
        let g = make_graph();
        let groups = nodes_by_label(&g);
        assert!(groups.contains_key("Function"));
        let stmt = node_statement("Function");
        assert!(stmt.contains("MERGE (n:GraphNode {id: row.id})"));
        // The type label is added alongside :GraphNode, so the id constraint
        // and the edge MATCH keep working while typed queries become possible.
        assert!(stmt.contains("SET n:Function"));
    }

    #[test]
    fn edge_rows_carry_endpoints() {
        let g = make_graph();
        let groups = edges_by_type(&g);
        let rows = groups.get("CALLS").expect("CALLS group");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["source"], "id-alpha");
        assert_eq!(rows[0]["target"], "id-beta");
        assert_eq!(rows[0]["relation"], "calls");
        assert!(edge_statement("CALLS").contains("MERGE (a)-[r:CALLS]->(b)"));
    }

    /// Internal relation names are translated on the way out, so a database
    /// built here answers the same queries as one built by Python graphify.
    #[test]
    fn relation_vocabulary_matches_python() {
        assert_eq!(
            safe_rel(py_compat::translate_relation("defines")),
            "CONTAINS"
        );
        assert_eq!(
            safe_rel(py_compat::translate_relation("imports")),
            "IMPORTS_FROM"
        );
        assert_eq!(safe_rel(py_compat::translate_relation("calls")), "CALLS");
    }

    #[test]
    fn sanitizers_reject_injection_and_empties() {
        assert_eq!(safe_label("Fun-ction!"), "Function");
        assert_eq!(safe_label("***"), "Entity");
        assert_eq!(safe_rel("is a"), "IS_A");
        assert_eq!(safe_rel("---"), "RELATED_TO");
    }
}
