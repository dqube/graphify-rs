//! Python-parity `graph.html` writer.
//!
//! Ports `graphify.exporters.html.to_html` from the Python reference so the
//! rendered visualization — vis.js network, sidebar, tooltips, legend — is
//! visually identical to what the Python tool produces for the same graph.
//!
//! The template is inlined as raw string literals so no external assets are
//! needed and the output is a single self-contained HTML file.

use std::collections::{HashMap, HashSet};

use graphify_core::graph::KnowledgeGraph;
use graphify_core::model::{GraphEdge, GraphNode};
use graphify_core::py_compat::{file_type_for, normalize_source_path};
use serde_json::{Value, json};

/// Categorical palette used for community coloring — verbatim from
/// `graphify.exporters.base.COMMUNITY_COLORS`.
const COMMUNITY_COLORS: &[&str] = &[
    "#4E79A7", "#F28E2B", "#E15759", "#76B7B2", "#59A14F",
    "#EDC948", "#B07AA1", "#FF9DA7", "#9C755F", "#BAB0AC",
];

/// HTML-escape for values that end up inside HTML text/attribute contexts.
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// `sanitize_label` in Python strips control chars; the rust extractors
/// already keep labels clean so this is a light pass — collapse newlines and
/// tabs to spaces so multi-line labels don't break the tooltip / info panel.
fn sanitize_label(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\n' | '\r' | '\t' => ' ',
            other if (other as u32) < 0x20 => ' ',
            other => other,
        })
        .collect::<String>()
        .trim()
        .to_string()
}

/// Escape `</` sequences inside embedded JSON so a payload cannot close the
/// enclosing `<script>` tag. Matches Python's `_js_safe`.
fn js_safe(v: &Value) -> String {
    serde_json::to_string(v)
        .unwrap_or_else(|_| "null".into())
        .replace("</", "<\\/")
}

/// The CSS block Python emits verbatim.
fn html_styles() -> &'static str {
    r#"<style>
  * { box-sizing: border-box; margin: 0; padding: 0; }
  body { background: #0f0f1a; color: #e0e0e0; font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; display: flex; height: 100vh; overflow: hidden; }
  #graph { flex: 1; }
  #sidebar { width: 280px; background: #1a1a2e; border-left: 1px solid #2a2a4e; display: flex; flex-direction: column; overflow: hidden; }
  #search-wrap { padding: 12px; border-bottom: 1px solid #2a2a4e; }
  #search { width: 100%; background: #0f0f1a; border: 1px solid #3a3a5e; color: #e0e0e0; padding: 7px 10px; border-radius: 6px; font-size: 13px; outline: none; }
  #search:focus { border-color: #4E79A7; }
  #search-results { max-height: 140px; overflow-y: auto; padding: 4px 12px; border-bottom: 1px solid #2a2a4e; display: none; }
  .search-item { padding: 4px 6px; cursor: pointer; border-radius: 4px; font-size: 12px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .search-item:hover { background: #2a2a4e; }
  #info-panel { padding: 14px; border-bottom: 1px solid #2a2a4e; min-height: 140px; }
  #info-panel h3 { font-size: 13px; color: #aaa; margin-bottom: 8px; text-transform: uppercase; letter-spacing: 0.05em; }
  #info-content { font-size: 13px; color: #ccc; line-height: 1.6; }
  #info-content .field { margin-bottom: 5px; }
  #info-content .field b { color: #e0e0e0; }
  #info-content .empty { color: #555; font-style: italic; }
  .neighbor-link { display: block; padding: 2px 6px; margin: 2px 0; border-radius: 3px; cursor: pointer; font-size: 12px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; border-left: 3px solid #333; }
  .neighbor-link:hover { background: #2a2a4e; }
  #neighbors-list { max-height: 160px; overflow-y: auto; margin-top: 4px; }
  #legend-wrap { flex: 1; overflow-y: auto; padding: 12px; }
  #legend-wrap h3 { font-size: 13px; color: #aaa; margin-bottom: 10px; text-transform: uppercase; letter-spacing: 0.05em; }
  .legend-item { display: flex; align-items: center; gap: 8px; padding: 4px 0; cursor: pointer; border-radius: 4px; font-size: 12px; }
  .legend-item:hover { background: #2a2a4e; padding-left: 4px; }
  .legend-item.dimmed { opacity: 0.35; }
  .legend-dot { width: 12px; height: 12px; border-radius: 50%; flex-shrink: 0; }
  .legend-label { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .legend-count { color: #666; font-size: 11px; }
  #stats { padding: 10px 14px; border-top: 1px solid #2a2a4e; font-size: 11px; color: #555; }
  #legend-controls { display: flex; align-items: center; gap: 8px; margin-bottom: 8px; padding: 4px 0; }
  #legend-controls label { display: flex; align-items: center; gap: 6px; cursor: pointer; font-size: 12px; color: #aaa; user-select: none; }
  #legend-controls label:hover { color: #e0e0e0; }
  .legend-cb, #select-all-cb { appearance: none; -webkit-appearance: none; width: 14px; height: 14px; border: 1.5px solid #3a3a5e; border-radius: 3px; background: #0f0f1a; cursor: pointer; position: relative; flex-shrink: 0; }
  .legend-cb:checked, #select-all-cb:checked { background: #4E79A7; border-color: #4E79A7; }
  .legend-cb:checked::after, #select-all-cb:checked::after { content: ''; position: absolute; left: 3.5px; top: 1px; width: 4px; height: 7px; border: solid #fff; border-width: 0 2px 2px 0; transform: rotate(45deg); }
  #select-all-cb:indeterminate { background: #4E79A7; border-color: #4E79A7; }
  #select-all-cb:indeterminate::after { content: ''; position: absolute; left: 2px; top: 5px; width: 8px; height: 2px; background: #fff; border: none; transform: none; }
</style>"#
}

/// The vis.js glue script Python emits, with `{nodes_json}`, `{edges_json}`,
/// and `{legend_json}` substituted in.
fn html_script(nodes_json: &str, edges_json: &str, legend_json: &str) -> String {
    format!(
        r##"<script>
const RAW_NODES = {nodes_json};
const RAW_EDGES = {edges_json};
const LEGEND = {legend_json};

function esc(s) {{
  return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;').replace(/'/g,'&#39;');
}}

const nodesDS = new vis.DataSet(RAW_NODES.map(n => ({{
  id: n.id, label: n.label, color: n.color, size: n.size,
  font: n.font, title: n.title,
  _community: n.community, _community_name: n.community_name,
  _source_file: n.source_file, _file_type: n.file_type, _degree: n.degree,
}})));

const edgesDS = new vis.DataSet(RAW_EDGES.map((e, i) => ({{
  id: i, from: e.from, to: e.to,
  label: '',
  title: e.title,
  dashes: e.dashes,
  width: e.width,
  color: e.color,
  arrows: {{ to: {{ enabled: true, scaleFactor: 0.5 }} }},
}})));

const container = document.getElementById('graph');
const network = new vis.Network(container, {{ nodes: nodesDS, edges: edgesDS }}, {{
  physics: {{
    enabled: true,
    solver: 'forceAtlas2Based',
    forceAtlas2Based: {{
      gravitationalConstant: -60,
      centralGravity: 0.005,
      springLength: 120,
      springConstant: 0.08,
      damping: 0.4,
      avoidOverlap: 0.8,
    }},
    stabilization: {{ iterations: 200, fit: true }},
  }},
  interaction: {{
    hover: true,
    tooltipDelay: 100,
    hideEdgesOnDrag: true,
    navigationButtons: false,
    keyboard: false,
  }},
  nodes: {{ shape: 'dot', borderWidth: 1.5 }},
  edges: {{ smooth: {{ type: 'continuous', roundness: 0.2 }}, selectionWidth: 3 }},
}});

network.once('stabilizationIterationsDone', () => {{
  network.setOptions({{ physics: {{ enabled: false }} }});
}});

function showInfo(nodeId) {{
  const n = nodesDS.get(nodeId);
  if (!n) return;
  const neighborIds = network.getConnectedNodes(nodeId);
  const neighborItems = neighborIds.map(nid => {{
    const nb = nodesDS.get(nid);
    const color = nb ? nb.color.background : '#555';
    return `<span class="neighbor-link" style="border-left-color:${{esc(color)}}" onclick="focusNode(${{JSON.stringify(nid)}})">${{esc(nb ? nb.label : nid)}}</span>`;
  }}).join('');
  document.getElementById('info-content').innerHTML = `
    <div class="field"><b>${{esc(n.label)}}</b></div>
    <div class="field">Type: ${{esc(n._file_type || 'unknown')}}</div>
    <div class="field">Community: ${{esc(n._community_name)}}</div>
    <div class="field">Source: ${{esc(n._source_file || '-')}}</div>
    <div class="field">Degree: ${{n._degree}}</div>
    ${{neighborIds.length ? `<div class="field" style="margin-top:8px;color:#aaa;font-size:11px">Neighbors (${{neighborIds.length}})</div><div id="neighbors-list">${{neighborItems}}</div>` : ''}}
  `;
}}

function focusNode(nodeId) {{
  network.focus(nodeId, {{ scale: 1.4, animation: true }});
  network.selectNodes([nodeId]);
  showInfo(nodeId);
}}

let hoveredNodeId = null;
network.on('hoverNode', params => {{
  hoveredNodeId = params.node;
  container.style.cursor = 'pointer';
}});
network.on('blurNode', () => {{
  hoveredNodeId = null;
  container.style.cursor = 'default';
}});
container.addEventListener('click', () => {{
  if (hoveredNodeId !== null) {{
    showInfo(hoveredNodeId);
    network.selectNodes([hoveredNodeId]);
  }}
}});
network.on('click', params => {{
  if (params.nodes.length > 0) {{
    showInfo(params.nodes[0]);
  }} else if (hoveredNodeId === null) {{
    document.getElementById('info-content').innerHTML = '<span class="empty">Click a node to inspect it</span>';
  }}
}});

const searchInput = document.getElementById('search');
const searchResults = document.getElementById('search-results');
searchInput.addEventListener('input', () => {{
  const q = searchInput.value.toLowerCase().trim();
  searchResults.innerHTML = '';
  if (!q) {{ searchResults.style.display = 'none'; return; }}
  const matches = RAW_NODES.filter(n => n.label.toLowerCase().includes(q)).slice(0, 20);
  if (!matches.length) {{ searchResults.style.display = 'none'; return; }}
  searchResults.style.display = 'block';
  matches.forEach(n => {{
    const el = document.createElement('div');
    el.className = 'search-item';
    el.textContent = n.label;
    el.style.borderLeft = `3px solid ${{n.color.background}}`;
    el.style.paddingLeft = '8px';
    el.onclick = () => {{
      network.focus(n.id, {{ scale: 1.5, animation: true }});
      network.selectNodes([n.id]);
      showInfo(n.id);
      searchResults.style.display = 'none';
      searchInput.value = '';
    }};
    searchResults.appendChild(el);
  }});
}});
document.addEventListener('click', e => {{
  if (!searchResults.contains(e.target) && e.target !== searchInput)
    searchResults.style.display = 'none';
}});

const hiddenCommunities = new Set();
const selectAllCb = document.getElementById('select-all-cb');

function updateSelectAllState() {{
  const total = LEGEND.length;
  const hidden = hiddenCommunities.size;
  selectAllCb.checked = hidden === 0;
  selectAllCb.indeterminate = hidden > 0 && hidden < total;
}}

function toggleAllCommunities(hide) {{
  document.querySelectorAll('.legend-item').forEach(item => {{
    hide ? item.classList.add('dimmed') : item.classList.remove('dimmed');
  }});
  document.querySelectorAll('.legend-cb').forEach(cb => {{
    cb.checked = !hide;
  }});
  LEGEND.forEach(c => {{
    if (hide) hiddenCommunities.add(c.cid); else hiddenCommunities.delete(c.cid);
  }});
  const updates = RAW_NODES.map(n => ({{ id: n.id, hidden: hide }}));
  nodesDS.update(updates);
  updateSelectAllState();
}}

const legendEl = document.getElementById('legend');
LEGEND.forEach(c => {{
  const item = document.createElement('div');
  item.className = 'legend-item';
  const cb = document.createElement('input');
  cb.type = 'checkbox';
  cb.className = 'legend-cb';
  cb.checked = true;
  cb.addEventListener('change', (e) => {{
    e.stopPropagation();
    if (cb.checked) {{
      hiddenCommunities.delete(c.cid);
      item.classList.remove('dimmed');
    }} else {{
      hiddenCommunities.add(c.cid);
      item.classList.add('dimmed');
    }}
    const updates = RAW_NODES
      .filter(n => n.community === c.cid)
      .map(n => ({{ id: n.id, hidden: !cb.checked }}));
    nodesDS.update(updates);
    updateSelectAllState();
  }});
  item.innerHTML = `<div class="legend-dot" style="background:${{c.color}}"></div>
    <span class="legend-label">${{c.label}}</span>
    <span class="legend-count">${{c.count}}</span>`;
  item.prepend(cb);
  item.onclick = (e) => {{
    if (e.target === cb) return;
    cb.checked = !cb.checked;
    cb.dispatchEvent(new Event('change'));
  }};
  legendEl.appendChild(item);
}});
</script>"##,
        nodes_json = nodes_json,
        edges_json = edges_json,
        legend_json = legend_json,
    )
}

/// The hyperedge overlay script — draws each hyperedge as a shaded convex
/// hull labeled with its `relation`. Verbatim port of Python's
/// `_hyperedge_script`.
fn hyperedge_script(hyperedges_json: &str) -> String {
    format!(
        r##"<script>
const hyperedges = {hyperedges_json};
network.on('afterDrawing', function(ctx) {{
    hyperedges.forEach(h => {{
        const positions = h.nodes
            .map(nid => network.getPositions([nid])[nid])
            .filter(p => p !== undefined);
        if (positions.length < 2) return;
        ctx.save();
        ctx.globalAlpha = 0.12;
        ctx.fillStyle = '#6366f1';
        ctx.strokeStyle = '#6366f1';
        ctx.lineWidth = 2;
        ctx.beginPath();
        const cx = positions.reduce((s, p) => s + p.x, 0) / positions.length;
        const cy = positions.reduce((s, p) => s + p.y, 0) / positions.length;
        const expanded = positions.map(p => ({{
            x: cx + (p.x - cx) * 1.15,
            y: cy + (p.y - cy) * 1.15
        }}));
        ctx.moveTo(expanded[0].x, expanded[0].y);
        expanded.slice(1).forEach(p => ctx.lineTo(p.x, p.y));
        ctx.closePath();
        ctx.fill();
        ctx.globalAlpha = 0.4;
        ctx.stroke();
        ctx.globalAlpha = 0.8;
        ctx.fillStyle = '#4f46e5';
        ctx.font = 'bold 11px sans-serif';
        ctx.textAlign = 'center';
        ctx.fillText(h.label, cx, cy - 5);
        ctx.restore();
    }});
}});
</script>"##,
        hyperedges_json = hyperedges_json,
    )
}

/// Build the vis-node JSON array in the exact shape Python produces.
///
/// `included_nodes` bounds the render set (post-pruning). `degree_map`
/// carries the pre-computed degree of each included node so this function
/// doesn't re-scan the graph.
fn build_vis_nodes(
    nodes: &[&GraphNode],
    included_nodes: &HashSet<String>,
    degree_map: &HashMap<String, usize>,
    community_labels: &HashMap<usize, String>,
    node_community: &HashMap<String, usize>,
) -> Vec<Value> {
    let max_deg = degree_map.values().copied().max().unwrap_or(1).max(1) as f64;
    let mut out = Vec::with_capacity(included_nodes.len());
    for node in nodes {
        if !included_nodes.contains(&node.id) {
            continue;
        }
        let cid = node
            .community
            .or_else(|| node_community.get(node.id.as_str()).copied())
            .unwrap_or(0);
        let color = COMMUNITY_COLORS[cid % COMMUNITY_COLORS.len()];
        let deg = degree_map.get(&node.id).copied().unwrap_or(1);
        let size = 10.0 + 30.0 * (deg as f64 / max_deg);
        // Python: labels only for high-degree nodes (>= 15% of max).
        let font_size = if deg as f64 >= max_deg * 0.15 { 12 } else { 0 };
        let label = sanitize_label(&node.label);
        let community_name = community_labels
            .get(&cid)
            .cloned()
            .unwrap_or_else(|| format!("Community {cid}"));
        out.push(json!({
            "id": node.id,
            "label": label,
            "color": {
                "background": color,
                "border": color,
                "highlight": { "background": "#ffffff", "border": color },
            },
            "size": (size * 10.0).round() / 10.0,
            "font": { "size": font_size, "color": "#ffffff" },
            "title": html_escape(&label),
            "community": cid,
            "community_name": sanitize_label(&community_name),
            "source_file": sanitize_label(&normalize_source_path(&node.source_file)),
            "file_type": file_type_for(node),
            "degree": deg,
        }));
    }
    out
}

/// Build the vis-edge JSON array matching Python's shape.
fn build_vis_edges(edges: &[&GraphEdge], included_nodes: &HashSet<String>) -> Vec<Value> {
    let mut out = Vec::with_capacity(edges.len());
    for edge in edges {
        if !included_nodes.contains(&edge.source) || !included_nodes.contains(&edge.target) {
            continue;
        }
        let confidence_str = serde_json::to_value(&edge.confidence)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| "EXTRACTED".into());
        let extracted = confidence_str == "EXTRACTED";
        let relation = graphify_core::py_compat::translate_relation(&edge.relation);
        let title = html_escape(&format!("{relation} [{confidence_str}]"));
        out.push(json!({
            "from": edge.source,
            "to": edge.target,
            "label": relation,
            "title": title,
            "dashes": !extracted,
            "width": if extracted { 2 } else { 1 },
            "color": { "opacity": if extracted { 0.7 } else { 0.35 } },
            "confidence": confidence_str,
        }));
    }
    out
}

/// Build the legend array (`{cid, color, label, count}`) sorted by cid.
fn build_legend(
    communities: &HashMap<usize, Vec<String>>,
    community_labels: &HashMap<usize, String>,
    included_nodes: &HashSet<String>,
) -> Vec<Value> {
    let mut cids: Vec<usize> = community_labels.keys().copied().collect();
    if cids.is_empty() {
        // Fall back to whatever communities exist so the legend still renders.
        cids = communities.keys().copied().collect();
    }
    cids.sort();
    cids
        .into_iter()
        .map(|cid| {
            let color = COMMUNITY_COLORS[cid % COMMUNITY_COLORS.len()];
            let label = community_labels
                .get(&cid)
                .cloned()
                .unwrap_or_else(|| format!("Community {cid}"));
            let count = communities
                .get(&cid)
                .map(|members| {
                    members.iter().filter(|id| included_nodes.contains(*id)).count()
                })
                .unwrap_or(0);
            json!({
                "cid": cid,
                "color": color,
                "label": html_escape(&sanitize_label(&label)),
                "count": count,
            })
        })
        .collect()
}

/// Render the full `graph.html` document.
///
/// `title` is the string embedded in `<title>graphify - ...</title>` and the
/// on-page stats line reflects `nodes/edges/communities` counts. Callers
/// pass the already-computed `degree_map` from the graph so this function
/// stays a pure formatter.
pub fn render_graph_html(
    graph: &KnowledgeGraph,
    included_nodes: &HashSet<String>,
    degree_map: &HashMap<String, usize>,
    communities: &HashMap<usize, Vec<String>>,
    community_labels: &HashMap<usize, String>,
    title: &str,
) -> String {
    let all_nodes = graph.nodes();
    let all_edges = graph.edges();
    let node_community = communities
        .iter()
        .flat_map(|(cid, members)| members.iter().map(move |nid| (nid.clone(), *cid)))
        .collect::<HashMap<String, usize>>();

    let vis_nodes = build_vis_nodes(
        &all_nodes,
        included_nodes,
        degree_map,
        community_labels,
        &node_community,
    );
    let vis_edges = build_vis_edges(&all_edges, included_nodes);
    let legend = build_legend(communities, community_labels, included_nodes);
    let hyperedges_val: Value = serde_json::to_value(&graph.hyperedges).unwrap_or(Value::Array(vec![]));

    let nodes_json = js_safe(&Value::Array(vis_nodes.clone()));
    let edges_json = js_safe(&Value::Array(vis_edges.clone()));
    let legend_json = js_safe(&Value::Array(legend));
    let hyperedges_json = js_safe(&hyperedges_val);

    let n_nodes = vis_nodes.len();
    let n_edges = vis_edges.len();
    let n_communities = community_labels.len().max(communities.len());
    let stats = format!(
        "{n_nodes} nodes &middot; {n_edges} edges &middot; {n_communities} communities"
    );
    let escaped_title = html_escape(title);

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<title>graphify - {escaped_title}</title>
<script src="https://unpkg.com/vis-network@9.1.6/standalone/umd/vis-network.min.js"
        integrity="sha384-Ux6phic9PEHJ38YtrijhkzyJ8yQlH8i/+buBR8s3mAZOJrP1gwyvAcIYl3GWtpX1"
        crossorigin="anonymous"></script>
{styles}
</head>
<body>
<div id="graph"></div>
<div id="sidebar">
  <div id="search-wrap">
    <input id="search" type="text" placeholder="Search nodes..." autocomplete="off">
    <div id="search-results"></div>
  </div>
  <div id="info-panel">
    <h3>Node Info</h3>
    <div id="info-content"><span class="empty">Click a node to inspect it</span></div>
  </div>
  <div id="legend-wrap">
    <h3>Communities</h3>
    <div id="legend-controls">
      <label><input type="checkbox" id="select-all-cb" checked onchange="toggleAllCommunities(!this.checked)">Select All</label>
    </div>
    <div id="legend"></div>
  </div>
  <div id="stats">{stats}</div>
</div>
{main_script}
{he_script}
</body>
</html>"#,
        escaped_title = escaped_title,
        styles = html_styles(),
        stats = stats,
        main_script = html_script(&nodes_json, &edges_json, &legend_json),
        he_script = hyperedge_script(&hyperedges_json),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphify_core::confidence::Confidence;
    use graphify_core::model::{GraphEdge, GraphNode, NodeType};

    fn graph_with_two_nodes() -> KnowledgeGraph {
        let mut kg = KnowledgeGraph::new();
        kg.add_node(GraphNode {
            id: "a".into(),
            label: "Alpha".into(),
            source_file: "./src/a.rs".into(),
            source_location: Some("L1".into()),
            node_type: NodeType::File,
            community: Some(0),
            extra: Default::default(),
        })
        .unwrap();
        kg.add_node(GraphNode {
            id: "b".into(),
            label: "Beta".into(),
            source_file: "./src/b.rs".into(),
            source_location: Some("L2".into()),
            node_type: NodeType::Function,
            community: Some(0),
            extra: Default::default(),
        })
        .unwrap();
        kg.add_edge(GraphEdge {
            source: "a".into(),
            target: "b".into(),
            relation: "defines".into(),
            confidence: Confidence::Extracted,
            confidence_score: 1.0,
            source_file: "./src/a.rs".into(),
            source_location: None,
            weight: 1.0,
            provenance: None,
            extra: Default::default(),
        })
        .unwrap();
        kg
    }

    #[test]
    fn renders_top_level_containers_and_sidebar() {
        let kg = graph_with_two_nodes();
        let included: HashSet<String> = ["a", "b"].into_iter().map(String::from).collect();
        let mut degree = HashMap::new();
        degree.insert("a".into(), 1);
        degree.insert("b".into(), 1);
        let mut communities: HashMap<usize, Vec<String>> = HashMap::new();
        communities.insert(0, vec!["a".into(), "b".into()]);
        let mut labels = HashMap::new();
        labels.insert(0, "core".to_string());
        let html = render_graph_html(&kg, &included, &degree, &communities, &labels, "test");

        for needle in [
            "<div id=\"graph\"></div>",
            "<div id=\"sidebar\">",
            "<div id=\"search-wrap\">",
            "<div id=\"info-panel\">",
            "<div id=\"legend-wrap\">",
            "<div id=\"stats\">",
            "graphify - test",
            "vis-network@9.1.6",
        ] {
            assert!(html.contains(needle), "expected {needle:?} in HTML");
        }
    }

    #[test]
    fn tooltip_and_relation_use_py_vocab() {
        let kg = graph_with_two_nodes();
        let included: HashSet<String> = ["a", "b"].into_iter().map(String::from).collect();
        let mut degree = HashMap::new();
        degree.insert("a".into(), 1);
        degree.insert("b".into(), 1);
        let mut communities: HashMap<usize, Vec<String>> = HashMap::new();
        communities.insert(0, vec!["a".into(), "b".into()]);
        let labels = HashMap::new();
        let html = render_graph_html(&kg, &included, &degree, &communities, &labels, "test");

        // Edge label should be translated to Python vocabulary (contains, not defines).
        assert!(
            html.contains("\"label\":\"contains\""),
            "edge label should be translated to 'contains'"
        );
        assert!(
            html.contains("\"file_type\":\"code\""),
            "node.file_type should default to 'code' for File-typed nodes"
        );
    }
}
