//! D3 collapsible-tree HTML export.
//!
//! Port of the Python `graphify tree` view, re-rooted on the graph's own
//! structure instead of the filesystem: the Python builder walks directories
//! because it only has `graph.json`, while here the community assignment is
//! already available and is the more useful top level — it groups files that
//! belong together even when they live in different directories.
//!
//! The hierarchy is therefore `root -> community -> file -> symbol`, and every
//! interior node carries the descendant-leaf count so a collapsed branch can
//! still show how much is hidden underneath it.
//!
//! Output is a single self-contained `tree.html` in the same dark theme as
//! [`crate::html`] and [`crate::callflow`], with D3 v7 loaded from a CDN.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use graphify_core::graph::KnowledgeGraph;
use graphify_core::model::GraphNode;
use serde_json::Value;
use tracing::{info, warn};

/// Maximum children rendered under any one parent. Wide directories and huge
/// communities are otherwise unusable in a tree layout; the overflow collapses
/// into a single `(+N more)` leaf that still reports the hidden count.
const MAX_CHILDREN: usize = 200;

/// Global cap on rendered symbol leaves. D3 lays out every node it is given
/// even while collapsed, so an uncapped tree of a large monorepo takes seconds
/// to a first paint.
const MAX_LEAVES: usize = 20_000;

/// A `{name, count, children}` hierarchy node, mirroring the Python tree shape.
#[derive(Debug)]
struct TreeNode {
    name: String,
    /// Number of symbols underneath this node (1 for a symbol leaf).
    count: usize,
    children: Vec<TreeNode>,
}

impl TreeNode {
    fn leaf(name: String, count: usize) -> Self {
        Self {
            name,
            count,
            children: Vec::new(),
        }
    }

    /// Interior node whose count is the sum of its children's counts.
    fn branch(name: String, children: Vec<TreeNode>) -> Self {
        let count = children.iter().map(|c| c.count).sum::<usize>().max(1);
        Self {
            name,
            count,
            children,
        }
    }

    fn to_json(&self) -> Value {
        let mut map = serde_json::Map::with_capacity(3);
        map.insert("name".into(), Value::String(self.name.clone()));
        map.insert("count".into(), Value::from(self.count));
        if !self.children.is_empty() {
            map.insert(
                "children".into(),
                Value::Array(self.children.iter().map(TreeNode::to_json).collect()),
            );
        }
        Value::Object(map)
    }
}

/// Export a collapsible D3 tree of the graph as `tree.html`.
///
/// `communities` and `community_labels` come from the clustering pass; nodes
/// missing from both that map and their own `community` field are collected
/// under a trailing "Unassigned" branch rather than dropped.
pub fn export_tree_html(
    graph: &KnowledgeGraph,
    communities: &HashMap<usize, Vec<String>>,
    community_labels: &HashMap<usize, String>,
    output_dir: &Path,
) -> anyhow::Result<PathBuf> {
    let (root, truncated) = build_tree(graph, communities, community_labels);
    if truncated {
        warn!(
            nodes = graph.node_count(),
            "tree view truncated: some branches exceed the {MAX_CHILDREN}-child / \
             {MAX_LEAVES}-leaf render caps"
        );
    }

    let file_count = distinct_file_count(graph);
    let community_count = root.children.len();
    let subtitle = format!(
        "{} symbols · {} files · {} communities",
        graph.node_count(),
        file_count,
        community_count,
    );
    let banner = if truncated {
        format!(
            "<div class=\"banner\">Large graph: branches are capped at {MAX_CHILDREN} children \
             and {MAX_LEAVES} leaves overall. Hidden entries are summarised as \
             <code>(+N more)</code>.</div>"
        )
    } else {
        String::new()
    };

    // `</` inside the blob would close the surrounding <script> element.
    let data_json = serde_json::to_string(&root.to_json())?.replace("</", "<\\/");

    let html = TEMPLATE
        .replace("{{TITLE}}", &escape_html("graphify — tree view"))
        .replace("{{HEADER}}", &escape_html("Knowledge Graph Tree"))
        .replace("{{SUBTITLE}}", &escape_html(&subtitle))
        .replace("{{BANNER}}", &banner)
        .replace("{{DATA}}", &data_json);

    fs::create_dir_all(output_dir)?;
    let path = output_dir.join("tree.html");
    fs::write(&path, &html)?;
    info!(path = %path.display(), nodes = graph.node_count(), "exported D3 tree view");
    Ok(path)
}

/// Build the `root -> community -> file -> symbol` hierarchy.
///
/// Returns the root plus a flag saying whether any cap fired, so the caller can
/// warn and render the explanatory banner.
fn build_tree(
    graph: &KnowledgeGraph,
    communities: &HashMap<usize, Vec<String>>,
    community_labels: &HashMap<usize, String>,
) -> (TreeNode, bool) {
    let node_community = graphify_core::build_node_to_community(communities);

    // community id (None = unassigned) -> source file -> nodes
    let mut grouped: BTreeMap<Option<usize>, BTreeMap<String, Vec<&GraphNode>>> = BTreeMap::new();
    for node in graph.nodes() {
        let cid = node
            .community
            .or_else(|| node_community.get(node.id.as_str()).copied());
        grouped
            .entry(cid)
            .or_default()
            .entry(file_key(node))
            .or_default()
            .push(node);
    }

    let mut truncated = false;
    let mut budget = MAX_LEAVES;

    let mut community_nodes: Vec<(Option<usize>, TreeNode)> = Vec::new();
    for (cid, files) in grouped {
        let mut file_nodes: Vec<TreeNode> = Vec::new();
        for (file, nodes) in files {
            file_nodes.push(build_file_node(&file, &nodes, &mut budget, &mut truncated));
        }
        // Biggest files first so the interesting branches are at the top.
        file_nodes.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));
        let file_nodes = cap_children(file_nodes, &mut truncated);
        community_nodes.push((
            cid,
            TreeNode::branch(community_name(cid, community_labels), file_nodes),
        ));
    }

    // Largest communities first; the unassigned bucket always sorts last.
    community_nodes.sort_by(|a, b| {
        a.0.is_none()
            .cmp(&b.0.is_none())
            .then_with(|| b.1.count.cmp(&a.1.count))
            .then_with(|| a.1.name.cmp(&b.1.name))
    });
    let children: Vec<TreeNode> = community_nodes.into_iter().map(|(_, n)| n).collect();
    let children = cap_children(children, &mut truncated);

    let mut root = TreeNode::branch("Knowledge Graph".to_string(), children);
    if root.children.is_empty() {
        root.count = 0;
    }
    (root, truncated)
}

/// The map key a node is filed under — kept in one place so the insert and the
/// lookup can never drift apart.
fn file_key(node: &GraphNode) -> String {
    if node.source_file.is_empty() {
        "(no source file)".to_string()
    } else {
        node.source_file.clone()
    }
}

/// Build one file branch, spending from the global leaf budget.
fn build_file_node(
    file: &str,
    nodes: &[&GraphNode],
    budget: &mut usize,
    truncated: &mut bool,
) -> TreeNode {
    // Extractors emit a node for the file itself; as the parent branch already
    // carries that name, repeating it as a child is pure noise.
    let base = base_name(file);
    let mut symbols: Vec<String> = nodes
        .iter()
        .filter(|n| n.label != base)
        .map(|n| n.label.clone())
        .collect();
    // Private/dunder names last, then alphabetical — the Python tree's order.
    symbols.sort_by(|a, b| {
        a.starts_with('_')
            .cmp(&b.starts_with('_'))
            .then_with(|| a.to_lowercase().cmp(&b.to_lowercase()))
            .then_with(|| a.cmp(b))
    });

    let total = symbols.len();
    if total == 0 {
        // A file whose only node is the file node still deserves a leaf.
        return TreeNode::leaf(file.to_string(), 1);
    }

    let take = total.min(MAX_CHILDREN).min(*budget);
    *budget -= take;
    let mut children: Vec<TreeNode> = symbols
        .into_iter()
        .take(take)
        .map(|s| TreeNode::leaf(s, 1))
        .collect();
    if take < total {
        *truncated = true;
        let extra = total - take;
        children.push(TreeNode::leaf(format!("(+{extra} more)"), extra));
    }
    TreeNode::branch(file.to_string(), children)
}

/// Fold everything past [`MAX_CHILDREN`] into a single `(+N more)` leaf.
fn cap_children(mut children: Vec<TreeNode>, truncated: &mut bool) -> Vec<TreeNode> {
    if children.len() <= MAX_CHILDREN {
        return children;
    }
    *truncated = true;
    let overflow = children.split_off(MAX_CHILDREN);
    let hidden = overflow.len();
    let count: usize = overflow.iter().map(|c| c.count).sum();
    children.push(TreeNode::leaf(format!("(+{hidden} more)"), count.max(1)));
    children
}

/// Human-readable name for a community branch.
fn community_name(cid: Option<usize>, labels: &HashMap<usize, String>) -> String {
    match cid {
        None => "Unassigned".to_string(),
        Some(id) => labels
            .get(&id)
            .filter(|l| !l.is_empty())
            .cloned()
            .unwrap_or_else(|| format!("Community {id}")),
    }
}

/// Number of distinct source files represented in the graph.
fn distinct_file_count(graph: &KnowledgeGraph) -> usize {
    graph
        .nodes()
        .into_iter()
        .map(file_key)
        .collect::<std::collections::HashSet<String>>()
        .len()
}

/// Final path component, tolerating both separators.
fn base_name(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    normalized
        .rsplit('/')
        .next()
        .unwrap_or(normalized.as_str())
        .to_string()
}

/// HTML-escape text destined for element content or an attribute value.
fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            other => out.push(other),
        }
    }
    out
}

// `r##` rather than `r#`: the JS colour literals contain `"#`, which would
// close a single-hash raw string.
const TEMPLATE: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{{TITLE}}</title>
<style>
:root {
  --bg: #0f172a; --surface: #1e293b; --border: #334155;
  --text: #e2e8f0; --muted: #94a3b8; --accent: #38bdf8;
}
* { box-sizing: border-box; margin: 0; padding: 0; }
body { font-family: 'Segoe UI', system-ui, -apple-system, sans-serif; background: var(--bg); color: var(--text); }
header { padding: 26px 28px 0; }
h1 { font-size: 1.9rem; background: linear-gradient(135deg, var(--accent), #a78bfa); -webkit-background-clip: text; -webkit-text-fill-color: transparent; }
.subtitle { color: var(--muted); margin-top: 6px; font-size: 0.95rem; }
.banner { margin: 16px 28px 0; padding: 10px 14px; border: 1px solid #b45309; background: rgba(120,53,15,0.35); color: #fbbf24; border-radius: 8px; font-size: 0.88rem; }
.banner code { background: rgba(255,255,255,0.08); padding: 1px 5px; border-radius: 3px; }
.controls { display: flex; gap: 10px; flex-wrap: wrap; padding: 18px 28px; }
button { padding: 7px 16px; background: var(--surface); color: var(--text); border: 1px solid var(--border); border-radius: 6px; font: 600 0.85rem system-ui, sans-serif; cursor: pointer; }
button:hover { border-color: var(--accent); color: var(--accent); }
.hint { color: var(--muted); font-size: 0.82rem; align-self: center; }
#viewport { margin: 0 28px 28px; height: 76vh; overflow: auto; background: var(--surface); border: 1px solid var(--border); border-radius: 12px; }
svg { display: block; }
path.link { fill: none; stroke: #334155; stroke-width: 1.4px; }
g.node text { font: 12px 'Segoe UI', system-ui, sans-serif; fill: var(--text); }
g.node tspan.count { fill: var(--muted); }
g.node circle { stroke-width: 2px; }
</style>
</head>
<body>
<header>
  <h1>{{HEADER}}</h1>
  <div class="subtitle">{{SUBTITLE}}</div>
</header>
{{BANNER}}
<div class="controls">
  <button id="btn-expand">Expand all</button>
  <button id="btn-collapse">Collapse all</button>
  <button id="btn-reset">Reset view</button>
  <span class="hint">Click a node to toggle its branch · scroll to zoom · drag to pan</span>
</div>
<div id="viewport"><svg id="tree"></svg></div>
<script src="https://d3js.org/d3.v7.min.js"></script>
<script>
const DATA = {{DATA}};

// Depth 0 root, 1 community, 2 file, 3+ symbol.
const DEPTH_COLORS = ["#f8fafc", "#38bdf8", "#34d399", "#a78bfa", "#fbbf24"];
const DX = 22;      // vertical spacing between sibling rows
const DY = 320;     // horizontal spacing between depths
const DURATION = 250;
const MAX_LABEL = 64;

const svg = d3.select("#tree");
const zoomLayer = svg.append("g");
const layout = zoomLayer.append("g");
const gLink = layout.append("g");
const gNode = layout.append("g");

const tree = d3.tree().nodeSize([DX, DY]);
const diagonal = d3.linkHorizontal().x(d => d.y).y(d => d.x);

const root = d3.hierarchy(DATA);
root.x0 = 0;
root.y0 = 0;
let uid = 0;
root.descendants().forEach(d => {
  d.id = uid++;
  d._children = d.children;
  if (d.depth >= 1) d.children = null;   // open on the communities only
});

const zoom = d3.zoom().scaleExtent([0.12, 2.5])
  .on("zoom", event => zoomLayer.attr("transform", event.transform));
svg.call(zoom);

function colorFor(d) {
  return DEPTH_COLORS[Math.min(d.depth, DEPTH_COLORS.length - 1)];
}

function labelFor(d) {
  const name = d.data.name || "";
  return name.length > MAX_LABEL ? name.slice(0, MAX_LABEL - 1) + "…" : name;
}

// Walk collapsed branches too — d3's descendants() only follows `children`.
function walk(d, fn) {
  fn(d);
  const kids = d.children || d._children || [];
  kids.forEach(k => walk(k, fn));
}

function update(source) {
  tree(root);
  const nodes = root.descendants().reverse();
  const links = root.links();

  let left = root, right = root, maxDepth = 0;
  root.eachBefore(n => {
    if (n.x < left.x) left = n;
    if (n.x > right.x) right = n;
    if (n.depth > maxDepth) maxDepth = n.depth;
  });
  svg.attr("width", maxDepth * DY + 560).attr("height", right.x - left.x + 140);
  layout.attr("transform", "translate(240," + (-left.x + 70) + ")");

  const node = gNode.selectAll("g.node").data(nodes, d => d.id);

  const nodeEnter = node.enter().append("g")
    .attr("class", "node")
    .attr("transform", "translate(" + source.y0 + "," + source.x0 + ")")
    .attr("fill-opacity", 0)
    .attr("stroke-opacity", 0)
    .style("cursor", d => d._children ? "pointer" : "default")
    .on("click", (event, d) => {
      if (!d._children) return;
      d.children = d.children ? null : d._children;
      update(d);
    });

  nodeEnter.append("circle")
    .attr("r", 5)
    .attr("stroke", colorFor);

  nodeEnter.append("title").text(d => d.data.name);

  const text = nodeEnter.append("text")
    .attr("dy", "0.32em")
    .attr("x", d => d._children ? -10 : 10)
    .attr("text-anchor", d => d._children ? "end" : "start");
  text.append("tspan").text(labelFor);
  text.append("tspan").attr("class", "count")
    .text(d => (d._children ? "  (" + d.data.count + ")" : ""));

  const nodeUpdate = nodeEnter.merge(node);
  nodeUpdate.transition().duration(DURATION)
    .attr("transform", d => "translate(" + d.y + "," + d.x + ")")
    .attr("fill-opacity", 1)
    .attr("stroke-opacity", 1);
  // Filled = collapsed branch, hollow = expanded or leaf.
  nodeUpdate.select("circle")
    .attr("fill", d => (d._children && !d.children) ? colorFor(d) : "#0f172a");

  node.exit().transition().duration(DURATION)
    .attr("transform", "translate(" + source.y + "," + source.x + ")")
    .attr("fill-opacity", 0)
    .attr("stroke-opacity", 0)
    .remove();

  const link = gLink.selectAll("path.link").data(links, d => d.target.id);
  const collapsedAt = p => ({ source: p, target: p });

  link.enter().append("path")
    .attr("class", "link")
    .attr("d", diagonal(collapsedAt({ x: source.x0, y: source.y0 })))
    .merge(link)
    .transition().duration(DURATION)
    .attr("d", diagonal);

  link.exit().transition().duration(DURATION)
    .attr("d", diagonal(collapsedAt({ x: source.x, y: source.y })))
    .remove();

  root.eachBefore(d => { d.x0 = d.x; d.y0 = d.y; });
}

document.getElementById("btn-expand").addEventListener("click", () => {
  walk(root, d => { if (d._children) d.children = d._children; });
  update(root);
});
document.getElementById("btn-collapse").addEventListener("click", () => {
  walk(root, d => { if (d.depth >= 1) d.children = null; });
  root.children = null;
  update(root);
});
document.getElementById("btn-reset").addEventListener("click", () => {
  walk(root, d => { if (d.depth >= 1) d.children = null; });
  root.children = root._children;
  svg.call(zoom.transform, d3.zoomIdentity);
  update(root);
});

update(root);
</script>
</body>
</html>
"##;

#[cfg(test)]
mod tests {
    use super::*;
    use graphify_core::confidence::Confidence;
    use graphify_core::model::{GraphEdge, GraphNode, NodeType};

    fn node(id: &str, label: &str, file: &str, community: Option<usize>) -> GraphNode {
        GraphNode {
            id: id.into(),
            label: label.into(),
            source_file: file.into(),
            source_location: None,
            node_type: NodeType::Function,
            community,
            extra: HashMap::new(),
        }
    }

    fn sample_graph() -> KnowledgeGraph {
        let mut kg = KnowledgeGraph::new();
        kg.add_node(node("a", "MyClass", "src/main.rs", Some(0)))
            .unwrap();
        kg.add_node(node("b", "helper", "src/main.rs", Some(0)))
            .unwrap();
        // The redundant file node an extractor emits for src/main.rs.
        kg.add_node(node("f", "main.rs", "src/main.rs", Some(0)))
            .unwrap();
        kg.add_node(node("c", "render", "src/ui.rs", Some(1)))
            .unwrap();
        kg.add_node(node("d", "orphan", "src/lost.rs", None))
            .unwrap();
        kg.add_edge(GraphEdge {
            source: "a".into(),
            target: "b".into(),
            relation: "calls".into(),
            confidence: Confidence::Extracted,
            confidence_score: 1.0,
            source_file: "src/main.rs".into(),
            source_location: None,
            weight: 1.0,
            provenance: None,
            extra: HashMap::new(),
        })
        .unwrap();
        kg
    }

    fn labels() -> HashMap<usize, String> {
        HashMap::from([(0, "Core".to_string())])
    }

    fn communities() -> HashMap<usize, Vec<String>> {
        HashMap::from([
            (0, vec!["a".to_string(), "b".to_string(), "f".to_string()]),
            (1, vec!["c".to_string()]),
        ])
    }

    /// Pull the `const DATA = ...;` blob back out of the rendered page.
    fn extract_data(html: &str) -> Value {
        let start = html.find("const DATA = ").unwrap() + "const DATA = ".len();
        let rest = &html[start..];
        let end = rest.find(";\n").unwrap();
        serde_json::from_str(&rest[..end].replace("<\\/", "</")).unwrap()
    }

    #[test]
    fn export_tree_html_writes_expected_hierarchy() {
        let dir = tempfile::tempdir().unwrap();
        let path =
            export_tree_html(&sample_graph(), &communities(), &labels(), dir.path()).unwrap();
        assert_eq!(path.file_name().unwrap(), "tree.html");

        let html = fs::read_to_string(&path).unwrap();
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains("https://d3js.org/d3.v7.min.js"));
        assert!(html.contains("<title>graphify — tree view</title>"));
        assert!(html.contains("5 symbols · 3 files · 3 communities"));
        // Caps did not fire, so no banner.
        assert!(!html.contains("class=\"banner\""));

        let data = extract_data(&html);
        assert_eq!(data["name"], "Knowledge Graph");

        let comms = data["children"].as_array().unwrap();
        let names: Vec<&str> = comms.iter().map(|c| c["name"].as_str().unwrap()).collect();
        // Labelled community first (largest), then Community 1, Unassigned last.
        assert_eq!(names, vec!["Core", "Community 1", "Unassigned"]);

        let core = &comms[0];
        assert_eq!(core["count"], 2);
        let files = core["children"].as_array().unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0]["name"], "src/main.rs");

        let symbols: Vec<&str> = files[0]["children"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap())
            .collect();
        // "main.rs" is dropped as the redundant file node.
        assert_eq!(symbols, vec!["helper", "MyClass"]);
    }

    #[test]
    fn export_tree_html_empty_graph() {
        let dir = tempfile::tempdir().unwrap();
        let kg = KnowledgeGraph::new();
        let path = export_tree_html(&kg, &HashMap::new(), &HashMap::new(), dir.path()).unwrap();

        let html = fs::read_to_string(&path).unwrap();
        assert!(html.contains("0 symbols · 0 files · 0 communities"));

        let data = extract_data(&html);
        assert_eq!(data["name"], "Knowledge Graph");
        assert_eq!(data["count"], 0);
        assert!(data.get("children").is_none());
    }

    #[test]
    fn wide_file_is_capped_with_a_more_leaf() {
        let mut kg = KnowledgeGraph::new();
        let total = MAX_CHILDREN + 15;
        for i in 0..total {
            kg.add_node(node(
                &format!("s{i:04}"),
                &format!("sym{i:04}"),
                "src/big.rs",
                Some(0),
            ))
            .unwrap();
        }

        let dir = tempfile::tempdir().unwrap();
        let path = export_tree_html(&kg, &HashMap::new(), &HashMap::new(), dir.path()).unwrap();
        let html = fs::read_to_string(&path).unwrap();
        assert!(html.contains("class=\"banner\""));

        let data = extract_data(&html);
        let file = &data["children"][0]["children"][0];
        let kids = file["children"].as_array().unwrap();
        assert_eq!(kids.len(), MAX_CHILDREN + 1);
        assert_eq!(kids[MAX_CHILDREN]["name"], "(+15 more)");
        assert_eq!(kids[MAX_CHILDREN]["count"], 15);
        // The count still reflects every symbol, hidden ones included.
        assert_eq!(file["count"], total);
    }

    #[test]
    fn nodes_without_a_community_land_under_unassigned() {
        let mut kg = KnowledgeGraph::new();
        kg.add_node(node("x", "loose", "src/x.rs", None)).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let path = export_tree_html(&kg, &HashMap::new(), &HashMap::new(), dir.path()).unwrap();
        let data = extract_data(&fs::read_to_string(&path).unwrap());
        assert_eq!(data["children"][0]["name"], "Unassigned");
    }

    #[test]
    fn community_membership_map_is_used_when_the_node_field_is_unset() {
        let mut kg = KnowledgeGraph::new();
        kg.add_node(node("x", "loose", "src/x.rs", None)).unwrap();
        let comms = HashMap::from([(7, vec!["x".to_string()])]);
        let labels = HashMap::from([(7, "Seven".to_string())]);

        let dir = tempfile::tempdir().unwrap();
        let path = export_tree_html(&kg, &comms, &labels, dir.path()).unwrap();
        let data = extract_data(&fs::read_to_string(&path).unwrap());
        assert_eq!(data["children"][0]["name"], "Seven");
    }

    #[test]
    fn script_terminator_in_a_label_cannot_break_out_of_the_script_tag() {
        let mut kg = KnowledgeGraph::new();
        kg.add_node(node("x", "</script><img src=x>", "src/x.rs", None))
            .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let path = export_tree_html(&kg, &HashMap::new(), &HashMap::new(), dir.path()).unwrap();
        let html = fs::read_to_string(&path).unwrap();

        assert!(html.contains("<\\/script>"));
        // Exactly the two script elements the template itself opens and closes.
        assert_eq!(html.matches("</script>").count(), 2);
    }

    #[test]
    fn nodes_with_no_source_file_get_their_own_branch() {
        let mut kg = KnowledgeGraph::new();
        kg.add_node(node("x", "synthetic", "", Some(0))).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let path = export_tree_html(&kg, &HashMap::new(), &HashMap::new(), dir.path()).unwrap();
        let data = extract_data(&fs::read_to_string(&path).unwrap());
        assert_eq!(
            data["children"][0]["children"][0]["name"],
            "(no source file)"
        );
    }

    #[test]
    fn base_name_handles_both_separators() {
        assert_eq!(base_name("src/main.rs"), "main.rs");
        assert_eq!(base_name("src\\win.rs"), "win.rs");
        assert_eq!(base_name("bare.rs"), "bare.rs");
    }

    #[test]
    fn escape_html_covers_attribute_characters() {
        assert_eq!(
            escape_html("<a href=\"x\">&'</a>"),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#x27;&lt;/a&gt;"
        );
    }
}
