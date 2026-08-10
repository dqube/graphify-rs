//! Interactive vis.js HTML export.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as FmtWrite;
use std::fs;
use std::path::{Path, PathBuf};

use graphify_core::confidence::Confidence;
use graphify_core::graph::KnowledgeGraph;
use tracing::{info, warn};

#[path = "html_templates.rs"]
mod html_templates;
use html_templates::{VIS_SCRIPT_TAG, build_html_template, escape_html, escape_js, js_safe_json};

const COMMUNITY_COLORS: &[&str] = &[
    "#4E79A7", "#F28E2B", "#E15759", "#76B7B2", "#59A14F", "#EDC948", "#B07AA1", "#FF9DA7",
    "#9C755F", "#BAB0AC",
];

/// Soft limit: above this we prune to top nodes (default, overridable via `max_nodes`).
const DEFAULT_MAX_VIS_NODES: usize = 2000;

/// Export an interactive HTML visualization of the knowledge graph.
///
/// For large graphs (> `max_nodes` nodes), automatically prunes to the most
/// important nodes: highest-degree nodes plus community representatives.
/// Pass `None` for `max_nodes` to use the default of 2000.
pub fn export_html(
    graph: &KnowledgeGraph,
    communities: &HashMap<usize, Vec<String>>,
    community_labels: &HashMap<usize, String>,
    output_dir: &Path,
    max_nodes: Option<usize>,
) -> anyhow::Result<PathBuf> {
    let max_vis = max_nodes.unwrap_or(DEFAULT_MAX_VIS_NODES);
    let total_nodes = graph.node_count();
    let total_edges = graph.edge_count();

    let (included_nodes, pruned) = if total_nodes > max_vis {
        warn!(
            total_nodes,
            threshold = max_vis,
            "graph too large for interactive viz, pruning to top {} nodes",
            max_vis
        );
        (prune_nodes(graph, communities, max_vis), true)
    } else {
        (
            graph.node_ids().into_iter().collect::<HashSet<String>>(),
            false,
        )
    };

    let node_community = graphify_core::build_node_to_community(communities);

    // One degree pass: sizes and label visibility are both relative to the
    // best-connected node on the page, so max_degree must be known before
    // any node is serialized.
    let rendered: Vec<(&graphify_core::model::GraphNode, usize, Option<usize>)> = graph
        .nodes()
        .into_iter()
        .filter(|n| included_nodes.contains(&n.id))
        .map(|n| {
            let cid = n
                .community
                .or_else(|| node_community.get(n.id.as_str()).copied());
            (n, graph.degree(&n.id), cid)
        })
        .collect();
    let max_degree = rendered.iter().map(|(_, d, _)| *d).max().unwrap_or(1).max(1) as f64;

    let vis_nodes: Vec<serde_json::Value> = rendered
        .iter()
        .map(|(node, degree, cid)| {
            let degree = *degree;
            let color = cid.map_or("#888888", |c| COMMUNITY_COLORS[c % COMMUNITY_COLORS.len()]);
            // Normalized 10..40 so one mega-hub cannot dwarf the page, and
            // labels only on genuine hubs — every other name lives in the
            // tooltip and info panel. Full labeling is a wall of text past
            // ~150 nodes.
            let size = 10.0 + 30.0 * degree as f64 / max_degree;
            let font_size = if degree as f64 >= 0.15 * max_degree { 12 } else { 0 };
            let community_name = cid.map(|c| {
                community_labels
                    .get(&c)
                    .cloned()
                    .unwrap_or_else(|| format!("Community {c}"))
            });
            // vis renders titles as HTML: escape the parts, join with <br>.
            let title = format!(
                "<b>{}</b><br>{}<br>Type: {} · Degree: {}",
                escape_html(&node.label),
                escape_html(&node.source_file),
                escape_html(&node.node_type.to_string()),
                degree
            );
            serde_json::json!({
                "id": node.id,
                "label": node.label,
                "title": title,
                "color": {
                    "background": color,
                    "border": color,
                    "highlight": { "background": "#ffffff", "border": color },
                },
                "size": (size * 10.0).round() / 10.0,
                "font": { "size": font_size, "color": "#e2e8f0" },
                "community": cid.unwrap_or(0),
                "community_name": community_name,
                "source_file": node.source_file,
                "file_type": node.node_type.to_string(),
                "degree": degree,
            })
        })
        .collect();

    let vis_edges: Vec<serde_json::Value> = graph
        .edges()
        .into_iter()
        .filter(|e| included_nodes.contains(&e.source) && included_nodes.contains(&e.target))
        .map(|edge| {
            let extracted = matches!(edge.confidence, Confidence::Extracted);
            let title = format!(
                "{}: {} → {}<br>Confidence: {} ({:.2})",
                escape_html(&edge.relation),
                escape_html(&edge.source),
                escape_html(&edge.target),
                edge.confidence,
                edge.confidence_score
            );
            serde_json::json!({
                "from": edge.source,
                "to": edge.target,
                "title": title,
                "relation": edge.relation,
                "dashes": !extracted,
                "width": ((1.0 + edge.confidence_score * 2.0) * 10.0).round() / 10.0,
                // Inferred edges recede instead of competing with extracted ones.
                "color": {
                    "color": "#475569",
                    "highlight": "#38bdf8",
                    "hover": "#38bdf8",
                    "opacity": if extracted { 0.7 } else { 0.35 },
                },
            })
        })
        .collect();

    // The legend lists every community in the graph, not every community
    // that happens to have a name: a graph that never went through `label`
    // has an empty labels map, and keying on it rendered an empty legend.
    // Sorted by id so the legend is stable between builds; a HashMap walk
    // reshuffled it on every run.
    let mut legend_cids: Vec<usize> = communities
        .keys()
        .chain(community_labels.keys())
        .copied()
        .collect::<HashSet<usize>>()
        .into_iter()
        .collect();
    legend_cids.sort_unstable();
    let legend: Vec<serde_json::Value> = legend_cids
        .iter()
        .map(|&cid| {
            let label = community_labels
                .get(&cid)
                .cloned()
                .unwrap_or_else(|| format!("Community {cid}"));
            serde_json::json!({
                "cid": cid,
                "color": COMMUNITY_COLORS[cid % COMMUNITY_COLORS.len()],
                "label": label,
                "count": communities.get(&cid).map_or(0, Vec::len),
            })
        })
        .collect();

    let mut hyperedge_html = String::new();
    for he in &graph.hyperedges {
        write!(
            hyperedge_html,
            "<li><b>{}</b>: {} ({})</li>",
            escape_html(&he.relation),
            escape_html(&he.label),
            escape_html(&he.nodes.join(", ")),
        )?;
    }

    let prune_banner = if pruned {
        format!(
            r#"<div id="prune-banner">Showing top {} of {} nodes ({} edges total). Only highest-degree nodes and community representatives are displayed.</div>"#,
            included_nodes.len(),
            total_nodes,
            total_edges,
        )
    } else {
        String::new()
    };

    let stats_html = format!(
        "{} nodes &middot; {} edges &middot; {} communities",
        rendered.len(),
        vis_edges.len(),
        communities.len(),
    );

    let is_large = included_nodes.len() > 500;
    let html = build_html_template(
        &js_safe_json(&serde_json::Value::Array(vis_nodes)),
        &js_safe_json(&serde_json::Value::Array(vis_edges)),
        &js_safe_json(&serde_json::Value::Array(legend)),
        &hyperedge_html,
        &prune_banner,
        &stats_html,
        is_large,
    );

    fs::create_dir_all(output_dir)?;
    let path = output_dir.join("graph.html");
    fs::write(&path, &html)?;
    info!(path = %path.display(), nodes = included_nodes.len(), "exported interactive HTML visualization");
    Ok(path)
}

/// Select the most important nodes for visualization when the graph is too large.
///
/// Strategy:
/// 1. Include top N nodes by degree (hub nodes)
/// 2. Include at least 1 representative from each community
/// 3. Cap at `max_nodes`
fn prune_nodes(
    graph: &KnowledgeGraph,
    communities: &HashMap<usize, Vec<String>>,
    max_nodes: usize,
) -> HashSet<String> {
    let mut included: HashSet<String> = HashSet::new();

    let mut by_degree: Vec<(String, usize)> = graph
        .node_ids()
        .into_iter()
        .map(|id| {
            let deg = graph.degree(&id);
            (id, deg)
        })
        .collect();
    by_degree.sort_by_key(|b| std::cmp::Reverse(b.1));

    let community_slots = communities.len().min(max_nodes / 4);
    let degree_slots = max_nodes.saturating_sub(community_slots);

    for (id, _) in by_degree.iter().take(degree_slots) {
        included.insert(id.clone());
    }

    for members in communities.values() {
        if included.len() >= max_nodes {
            break;
        }
        let best = members.iter().max_by_key(|id| graph.degree(id)).cloned();
        if let Some(id) = best {
            included.insert(id);
        }
    }

    included
}

/// Export a split HTML visualization into `output_dir/html/`.
///
/// Generates:
/// - `html/index.html` — overview page where each community is a single super-node,
///   edges represent cross-community connections. Click a community to navigate.
/// - `html/community_N.html` — detail page for community N with all its internal
///   nodes and edges. Links back to index and to other communities.
///
/// Returns the path to the `html/` directory.
pub fn export_html_split(
    graph: &KnowledgeGraph,
    communities: &HashMap<usize, Vec<String>>,
    community_labels: &HashMap<usize, String>,
    output_dir: &Path,
) -> anyhow::Result<PathBuf> {
    let html_dir = output_dir.join("html");
    fs::create_dir_all(&html_dir)?;

    let node_community = graphify_core::build_node_to_community(communities);
    generate_overview(
        &html_dir,
        graph,
        communities,
        community_labels,
        &node_community,
    )?;

    let mut sorted_cids: Vec<usize> = communities.keys().copied().collect();
    sorted_cids.sort_unstable();
    for &cid in &sorted_cids {
        let members = &communities[&cid];
        let label = community_labels
            .get(&cid)
            .cloned()
            .unwrap_or_else(|| format!("Community {cid}"));
        generate_community_page(
            &html_dir,
            graph,
            cid,
            &label,
            members,
            community_labels,
            &node_community,
        )?;
    }

    info!(
        path = %html_dir.display(),
        communities = communities.len(),
        "exported split HTML visualization"
    );
    Ok(html_dir)
}

/// Generate the overview index.html with communities as super-nodes.
fn generate_overview(
    html_dir: &Path,
    graph: &KnowledgeGraph,
    communities: &HashMap<usize, Vec<String>>,
    community_labels: &HashMap<usize, String>,
    node_community: &HashMap<&str, usize>,
) -> anyhow::Result<()> {
    let mut vis_nodes = String::from("[");
    let mut first = true;
    for (&cid, members) in communities {
        if !first {
            vis_nodes.push(',');
        }
        first = false;
        let label = community_labels
            .get(&cid)
            .cloned()
            .unwrap_or_else(|| format!("Community {cid}"));
        let color = COMMUNITY_COLORS[cid % COMMUNITY_COLORS.len()];
        let size = 20.0 + (members.len() as f64).sqrt() * 5.0;
        // A real newline: escape_js turns it into the single `\n` vis expects.
        // The old literal `\\n` came out doubled and rendered as text.
        // "Double-click" because navigation is bound to doubleClick below.
        let title = format!("{} ({} nodes)\nDouble-click to open", label, members.len());
        write!(
            vis_nodes,
            r#"{{id:{cid},label:"{label} ({count})",title:"{title}",color:"{color}",size:{size:.1},url:"community_{cid}.html"}}"#,
            cid = cid,
            label = escape_js(&label),
            count = members.len(),
            title = escape_js(&title),
            color = color,
            size = size,
        )?;
    }
    vis_nodes.push(']');

    let mut cross_edges: HashMap<(usize, usize), usize> = HashMap::new();
    for edge in graph.edges() {
        let src_cid = node_community.get(edge.source.as_str()).copied();
        let tgt_cid = node_community.get(edge.target.as_str()).copied();
        if let (Some(sc), Some(tc)) = (src_cid, tgt_cid)
            && sc != tc
        {
            let key = if sc < tc { (sc, tc) } else { (tc, sc) };
            *cross_edges.entry(key).or_default() += 1;
        }
    }

    let mut vis_edges = String::from("[");
    first = true;
    for ((from, to), count) in &cross_edges {
        if !first {
            vis_edges.push(',');
        }
        first = false;
        let width = 1.0 + (*count as f64).sqrt();
        write!(
            vis_edges,
            r#"{{from:{from},to:{to},label:"{count}",width:{width:.1},title:"{count} cross-community edges"}}"#,
        )?;
    }
    vis_edges.push(']');

    let mut nav_html = String::new();
    let mut sorted_cids: Vec<usize> = communities.keys().copied().collect();
    sorted_cids.sort_unstable();
    for cid in &sorted_cids {
        let label = community_labels
            .get(cid)
            .cloned()
            .unwrap_or_else(|| format!("Community {cid}"));
        let color = COMMUNITY_COLORS[*cid % COMMUNITY_COLORS.len()];
        let count = communities[cid].len();
        write!(
            nav_html,
            r#"<a href="community_{cid}.html" class="nav-link"><span class="legend-dot" style="background:{color}"></span>{label} ({count})</a>"#,
            cid = cid,
            color = color,
            label = escape_html(&label),
            count = count,
        )?;
    }

    let html = format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Knowledge Graph — Overview</title>
{VIS_SCRIPT_TAG}
<style>
* {{ margin: 0; padding: 0; box-sizing: border-box; }}
body {{ background: #0f172a; color: #e2e8f0; font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif; display: flex; height: 100vh; overflow: hidden; }}
#sidebar {{ width: 320px; min-width: 320px; background: #1e293b; padding: 16px; overflow-y: auto; display: flex; flex-direction: column; gap: 16px; border-right: 1px solid #334155; }}
#sidebar h2 {{ font-size: 18px; color: #76B7B2; margin-bottom: 4px; }}
#sidebar h3 {{ font-size: 14px; color: #9ca3af; margin-bottom: 8px; }}
.nav-link {{ display: flex; align-items: center; gap: 8px; font-size: 13px; padding: 6px 8px; border-radius: 4px; color: #e2e8f0; text-decoration: none; }}
.nav-link:hover {{ background: #334155; }}
.legend-dot {{ width: 12px; height: 12px; border-radius: 50%; flex-shrink: 0; }}
#graph-container {{ flex: 1; position: relative; }}
#info {{ background: #0f172a; border-radius: 8px; padding: 12px; font-size: 13px; color: #9ca3af; }}
</style>
</head>
<body>
<div id="sidebar">
    <div>
        <h2>🧠 Overview</h2>
        <p style="font-size:12px;color:#666;">Each node is a community. Double-click to open it.</p>
    </div>
    <div id="info">{node_count} nodes, {edge_count} edges, {community_count} communities</div>
    <div>
        <h3>Communities</h3>
        {nav}
    </div>
</div>
<div id="graph-container"></div>
<script>
(function() {{
    var nodesData = {nodes};
    var edgesData = {edges};
    var container = document.getElementById('graph-container');
    var nodes = new vis.DataSet(nodesData);
    var edges = new vis.DataSet(edgesData);
    var options = {{
        physics: {{
            solver: 'forceAtlas2Based',
            forceAtlas2Based: {{ gravitationalConstant: -100, centralGravity: 0.01, springLength: 200, springConstant: 0.05, damping: 0.4 }},
            stabilization: {{ iterations: 100 }}
        }},
        nodes: {{ shape: 'dot', font: {{ color: '#e0e0e0', size: 14, multi: true }}, borderWidth: 2 }},
        edges: {{ color: {{ color: '#4a4a6a' }}, font: {{ color: '#888', size: 12 }}, smooth: {{ type: 'continuous' }} }},
        interaction: {{ hover: true, zoomView: true, dragView: true }}
    }};
    var network = new vis.Network(container, {{ nodes: nodes, edges: edges }}, options);
    network.on('stabilizationIterationsDone', function() {{ network.setOptions({{ physics: {{ enabled: false }} }}); }});
    network.on('doubleClick', function(params) {{
        if (params.nodes.length > 0) {{
            var node = nodes.get(params.nodes[0]);
            if (node && node.url) {{ window.location.href = node.url; }}
        }}
    }});
}})();
</script>
</body>
</html>"#,
        nodes = vis_nodes,
        edges = vis_edges,
        nav = nav_html,
        node_count = graph.node_count(),
        edge_count = graph.edge_count(),
        community_count = communities.len(),
    );

    fs::write(html_dir.join("index.html"), &html)?;
    Ok(())
}

/// Generate a detail page for a single community.
fn generate_community_page(
    html_dir: &Path,
    graph: &KnowledgeGraph,
    cid: usize,
    label: &str,
    members: &[String],
    community_labels: &HashMap<usize, String>,
    node_community: &HashMap<&str, usize>,
) -> anyhow::Result<()> {
    let member_set: HashSet<&str> = members.iter().map(std::string::String::as_str).collect();
    let color = COMMUNITY_COLORS[cid % COMMUNITY_COLORS.len()];

    // Same 10..40 normalization as the main page: relative to this page's
    // best-connected member, so a hub cannot dwarf its own community view.
    let page_nodes: Vec<(&graphify_core::model::GraphNode, usize)> = graph
        .nodes()
        .into_iter()
        .filter(|n| member_set.contains(n.id.as_str()))
        .map(|n| (n, graph.degree(&n.id)))
        .collect();
    let max_deg = page_nodes.iter().map(|(_, d)| *d).max().unwrap_or(1).max(1) as f64;

    let mut vis_nodes = String::from("[");
    let mut first = true;
    for (node, degree) in &page_nodes {
        if !first {
            vis_nodes.push(',');
        }
        first = false;
        let size = 10.0 + 30.0 * *degree as f64 / max_deg;
        write!(
            vis_nodes,
            r#"{{id:"{}",label:"{}",title:"{}",color:"{}",size:{:.1}}}"#,
            escape_js(&node.id),
            escape_js(&node.label),
            escape_js(&format!(
                "{}\nType: {}\nFile: {}\nDegree: {}",
                node.label, node.node_type, node.source_file, degree
            )),
            color,
            size,
        )?;
    }
    vis_nodes.push(']');

    let mut vis_edges = String::from("[");
    first = true;
    for edge in graph.edges() {
        if !member_set.contains(edge.source.as_str()) || !member_set.contains(edge.target.as_str())
        {
            continue;
        }
        if !first {
            vis_edges.push(',');
        }
        first = false;
        let dashes = match edge.confidence {
            Confidence::Extracted => "false",
            _ => "true",
        };
        write!(
            vis_edges,
            r#"{{from:"{}",to:"{}",label:"{}",dashes:{},title:"{}"}}"#,
            escape_js(&edge.source),
            escape_js(&edge.target),
            escape_js(&edge.relation),
            dashes,
            escape_js(&format!(
                "{}: {} → {}\nConfidence: {}",
                edge.relation, edge.source, edge.target, edge.confidence
            )),
        )?;
    }
    vis_edges.push(']');

    let mut external_links: HashMap<usize, usize> = HashMap::new();
    for node_id in members {
        for edge in graph.edges() {
            let other = if edge.source == *node_id {
                &edge.target
            } else if edge.target == *node_id {
                &edge.source
            } else {
                continue;
            };
            if let Some(&other_cid) = node_community.get(other.as_str())
                && other_cid != cid
            {
                *external_links.entry(other_cid).or_default() += 1;
            }
        }
    }

    let mut nav_html = String::from(
        r#"<a href="index.html" class="nav-link" style="font-weight:bold;">← Overview</a>"#,
    );
    let mut sorted_ext: Vec<(usize, usize)> = external_links.into_iter().collect();
    sorted_ext.sort_by_key(|b| std::cmp::Reverse(b.1));
    for (ext_cid, count) in &sorted_ext {
        let ext_label = community_labels
            .get(ext_cid)
            .cloned()
            .unwrap_or_else(|| format!("Community {ext_cid}"));
        let ext_color = COMMUNITY_COLORS[*ext_cid % COMMUNITY_COLORS.len()];
        write!(
            nav_html,
            r#"<a href="community_{cid}.html" class="nav-link"><span class="legend-dot" style="background:{color}"></span>{label} ({count} links)</a>"#,
            cid = ext_cid,
            color = ext_color,
            label = escape_html(&ext_label),
            count = count,
        )?;
    }

    let is_large = members.len() > 500;
    let physics = if is_large {
        "solver:'barnesHut',barnesHut:{gravitationalConstant:-3000,springLength:95,damping:0.09},stabilization:{iterations:150}"
    } else {
        "solver:'forceAtlas2Based',forceAtlas2Based:{gravitationalConstant:-50,centralGravity:0.01,springLength:120,springConstant:0.08,damping:0.4,avoidOverlap:0.5},stabilization:{iterations:200}"
    };
    let edge_font = if is_large { 0 } else { 10 };

    let html = format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
{VIS_SCRIPT_TAG}
<style>
* {{ margin: 0; padding: 0; box-sizing: border-box; }}
body {{ background: #0f172a; color: #e2e8f0; font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif; display: flex; height: 100vh; overflow: hidden; }}
#sidebar {{ width: 320px; min-width: 320px; background: #1e293b; padding: 16px; overflow-y: auto; display: flex; flex-direction: column; gap: 16px; border-right: 1px solid #334155; }}
#sidebar h2 {{ font-size: 18px; color: {color}; margin-bottom: 4px; }}
#sidebar h3 {{ font-size: 14px; color: #9ca3af; margin-bottom: 8px; }}
.nav-link {{ display: flex; align-items: center; gap: 8px; font-size: 13px; padding: 6px 8px; border-radius: 4px; color: #e2e8f0; text-decoration: none; }}
.nav-link:hover {{ background: #334155; }}
.legend-dot {{ width: 12px; height: 12px; border-radius: 50%; flex-shrink: 0; }}
#graph-container {{ flex: 1; position: relative; }}
#search {{ width: 100%; padding: 8px 12px; border-radius: 6px; border: 1px solid #334155; background: #0f172a; color: #e2e8f0; font-size: 14px; }}
#search:focus {{ outline: none; border-color: {color}; }}
#info-panel {{ background: #0f172a; border-radius: 8px; padding: 12px; font-size: 13px; line-height: 1.6; min-height: 100px; }}
#info-panel .prop {{ color: #9ca3af; }}
#info-panel .val {{ color: #e2e8f0; }}
</style>
</head>
<body>
<div id="sidebar">
    <div>
        <h2>{label}</h2>
        <p style="font-size:12px;color:#666;">{node_count} nodes · Community {cid}</p>
    </div>
    <input id="search" type="text" placeholder="Search nodes…" />
    <div>
        <h3>Node Info</h3>
        <div id="info-panel"><i style="color:#666">Click a node to see details</i></div>
    </div>
    <div>
        <h3>Navigation</h3>
        {nav}
    </div>
</div>
<div id="graph-container"></div>
<script>
(function() {{
    var nodesData = {nodes};
    var edgesData = {edges};
    var container = document.getElementById('graph-container');
    var nodes = new vis.DataSet(nodesData);
    var edges = new vis.DataSet(edgesData);
    var options = {{
        physics: {{{physics}}},
        nodes: {{ shape: 'dot', font: {{ color: '#e0e0e0', size: 12 }}, borderWidth: 2 }},
        edges: {{ color: {{ color: '#4a4a6a', highlight: '{color}', hover: '{color}' }}, font: {{ color: '#888', size: {edge_font} }}, arrows: {{ to: {{ enabled: false }} }}, smooth: {{ type: 'continuous' }} }},
        interaction: {{ hover: true, tooltipDelay: 200, zoomView: true, dragView: true }}
    }};
    var network = new vis.Network(container, {{ nodes: nodes, edges: edges }}, options);
    network.on('stabilizationIterationsDone', function() {{ network.setOptions({{ physics: {{ enabled: false }} }}); }});
    var esc = function(s) {{ var d = document.createElement('div'); d.textContent = s == null ? '' : String(s); return d.innerHTML; }};
    network.on('click', function(params) {{
        var panel = document.getElementById('info-panel');
        if (params.nodes.length > 0) {{
            var node = nodes.get(params.nodes[0]);
            if (node) {{
                panel.innerHTML = '<div><span class="prop">Label:</span> <span class="val">' + esc(node.label) + '</span></div><div><span class="prop">ID:</span> <span class="val">' + esc(node.id) + '</span></div>';
                network.focus(params.nodes[0], {{ scale: 1.2, animation: true }});
            }}
        }}
    }});
    var searchEl = document.getElementById('search');
    var sTimer = null;
    searchEl.addEventListener('input', function() {{
        clearTimeout(sTimer);
        sTimer = setTimeout(function() {{
            var term = searchEl.value.toLowerCase();
            var updates = [];
            nodes.forEach(function(n) {{
                var h = term && !n.label.toLowerCase().includes(term);
                if (n.hidden !== h) {{ updates.push({{ id: n.id, hidden: h }}); }}
            }});
            if (updates.length > 0) {{ nodes.update(updates); }}
        }}, 200);
    }});
}})();
</script>
</body>
</html>"#,
        title = escape_html(&format!("{label} — Community {cid}")),
        color = color,
        label = escape_html(label),
        cid = cid,
        node_count = members.len(),
        nodes = vis_nodes,
        edges = vis_edges,
        nav = nav_html,
        physics = physics,
        edge_font = edge_font,
    );

    fs::write(html_dir.join(format!("community_{cid}.html")), &html)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphify_core::confidence::Confidence;
    use graphify_core::graph::KnowledgeGraph;
    use graphify_core::model::{GraphEdge, GraphNode, NodeType};

    fn sample_graph() -> KnowledgeGraph {
        let mut kg = KnowledgeGraph::new();
        kg.add_node(GraphNode {
            id: "a".into(),
            label: "NodeA".into(),
            source_file: "test.rs".into(),
            source_location: None,
            node_type: NodeType::Class,
            community: Some(0),
            extra: HashMap::new(),
        })
        .unwrap();
        kg.add_node(GraphNode {
            id: "b".into(),
            label: "NodeB".into(),
            source_file: "test.rs".into(),
            source_location: None,
            node_type: NodeType::Function,
            community: Some(1),
            extra: HashMap::new(),
        })
        .unwrap();
        kg.add_edge(GraphEdge {
            source: "a".into(),
            target: "b".into(),
            relation: "calls".into(),
            confidence: Confidence::Inferred,
            confidence_score: 0.7,
            source_file: "test.rs".into(),
            source_location: None,
            weight: 1.0,
            provenance: None,
            extra: HashMap::new(),
        })
        .unwrap();
        kg
    }

    #[test]
    fn export_html_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let kg = sample_graph();
        let communities: HashMap<usize, Vec<String>> =
            [(0, vec!["a".into()]), (1, vec!["b".into()])].into();
        let labels: HashMap<usize, String> =
            [(0, "Cluster A".into()), (1, "Cluster B".into())].into();

        let path = export_html(&kg, &communities, &labels, dir.path(), None).unwrap();
        assert!(path.exists());

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("NodeA"));
        assert!(content.contains("forceAtlas2Based"));

        // Pinned CDN with integrity, not floating latest.
        assert!(content.contains("vis-network@9.1.6"));
        assert!(content.contains(r#"integrity="sha384-Ux6phic"#));

        // Selection flash, arrows, and opacity tiers reach the page.
        assert!(content.contains(r##""highlight":{"background":"#ffffff""##));
        assert!(content.contains("scaleFactor: 0.5"));
        assert!(content.contains("selectionWidth: 3"));
        assert!(content.contains("avoidOverlap: 0.8"));
        assert!(content.contains(r#""opacity":0.35"#), "inferred edge fades");

        // Sidebar chrome: stats footer, filterable legend, search dropdown.
        assert!(content.contains("2 nodes &middot; 1 edges &middot; 2 communities"));
        assert!(content.contains("Select All"));
        assert!(content.contains(r#"id="search-results""#));

        // The stabilization timeout must also stop the simulation, not just
        // hide the spinner over a still-running one.
        assert!(content.contains("setTimeout(freeze, 10000)"));
        assert!(content.contains("physics: { enabled: false }"));
    }

    #[test]
    fn labels_are_reserved_for_hubs() {
        // A 10-node star: the hub has degree 9, every leaf degree 1, and
        // 1 < 0.15 * 9 — so only the hub's name is drawn on the canvas.
        let mut kg = KnowledgeGraph::new();
        let mut members: Vec<String> = Vec::new();
        for i in 0..10 {
            let id = format!("n{i}");
            members.push(id.clone());
            kg.add_node(GraphNode {
                id,
                label: format!("Symbol{i}"),
                source_file: "star.rs".into(),
                source_location: None,
                node_type: NodeType::Function,
                community: Some(0),
                extra: HashMap::new(),
            })
            .unwrap();
        }
        for i in 1..10 {
            kg.add_edge(GraphEdge {
                source: "n0".into(),
                target: format!("n{i}"),
                relation: "calls".into(),
                confidence: Confidence::Extracted,
                confidence_score: 1.0,
                source_file: "star.rs".into(),
                source_location: None,
                weight: 1.0,
                provenance: None,
                extra: HashMap::new(),
            })
            .unwrap();
        }
        let communities: HashMap<usize, Vec<String>> = [(0, members)].into();
        let labels: HashMap<usize, String> = [(0, "Star".into())].into();

        let dir = tempfile::tempdir().unwrap();
        let path = export_html(&kg, &communities, &labels, dir.path(), None).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();

        assert!(
            content.contains(r##""font":{"color":"#e2e8f0","size":12}"##),
            "hub keeps its label"
        );
        assert!(content.contains(r#""size":0"#), "leaves hide theirs");
        // Sizes normalized: hub at the 40 cap, leaves near the 10 floor.
        assert!(content.contains(r#""size":40.0"#) || content.contains(r#""size":40"#));
        assert!(content.contains(r#""size":13.3"#), "leaf size 10 + 30*(1/9)");
    }

    #[test]
    fn hostile_labels_cannot_break_out_of_the_script_block() {
        let mut kg = KnowledgeGraph::new();
        kg.add_node(GraphNode {
            id: "evil".into(),
            label: "</script><script>alert(1)</script>".into(),
            source_file: "x.rs".into(),
            source_location: None,
            node_type: NodeType::Function,
            community: Some(0),
            extra: HashMap::new(),
        })
        .unwrap();
        let communities: HashMap<usize, Vec<String>> = [(0, vec!["evil".into()])].into();
        let labels: HashMap<usize, String> = [(0, "C".into())].into();

        let dir = tempfile::tempdir().unwrap();
        let path = export_html(&kg, &communities, &labels, dir.path(), None).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();

        assert!(
            !content.contains("</script><script>alert"),
            "raw terminator sequence must never appear in the page"
        );
        assert!(content.contains(r"<\/script>"), "guard applied inside JSON");
    }

    #[test]
    fn legend_is_sorted_with_member_counts() {
        let dir = tempfile::tempdir().unwrap();
        let kg = sample_graph();
        // Insertion order 1-then-0 must not leak into the page.
        let communities: HashMap<usize, Vec<String>> =
            [(1, vec!["b".into()]), (0, vec!["a".into()])].into();
        let labels: HashMap<usize, String> =
            [(1, "Cluster B".into()), (0, "Cluster A".into())].into();

        let path = export_html(&kg, &communities, &labels, dir.path(), None).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();

        let zero = content.find(r#""cid":0"#).expect("legend entry for cid 0");
        let one = content.find(r#""cid":1"#).expect("legend entry for cid 1");
        assert!(zero < one, "legend must be sorted by community id");
        assert!(content.contains(r#""count":1"#));
    }

    #[test]
    fn sidebar_works_even_when_the_cdn_script_fails() {
        // The legend and search must be wired up BEFORE anything touches the
        // vis global: with the CDN blocked (offline, embedded previews), the
        // sidebar must still render and the page must show a readable error
        // instead of dying at `new vis.DataSet` with an empty legend.
        let dir = tempfile::tempdir().unwrap();
        let kg = sample_graph();
        let communities: HashMap<usize, Vec<String>> =
            [(0, vec!["a".into()]), (1, vec!["b".into()])].into();

        let path = export_html(&kg, &communities, &HashMap::new(), dir.path(), None).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();

        let legend = content.find("legendDiv.innerHTML").expect("legend render");
        let guard = content
            .find("typeof vis === 'undefined'")
            .expect("CDN guard");
        let first_vis_use = content.find("new vis.DataSet").expect("vis usage");
        assert!(
            legend < guard && guard < first_vis_use,
            "legend must render before the CDN guard, and the guard before any vis call"
        );
        assert!(content.contains("Could not load the graph library"));
    }

    #[test]
    fn legend_lists_unlabeled_communities_with_fallback_names() {
        // `export` on a graph that never went through `label` has an empty
        // labels map; the legend must still show every community.
        let dir = tempfile::tempdir().unwrap();
        let kg = sample_graph();
        let communities: HashMap<usize, Vec<String>> =
            [(0, vec!["a".into()]), (1, vec!["b".into()])].into();

        let path = export_html(&kg, &communities, &HashMap::new(), dir.path(), None).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();

        assert!(!content.contains("var LEGEND = [];"), "legend must not be empty");
        assert!(content.contains(r#""label":"Community 0""#));
        assert!(content.contains(r#""label":"Community 1""#));
    }

    #[test]
    fn escape_js_special_chars() {
        assert_eq!(escape_js("a\"b"), r#"a\"b"#);
        assert_eq!(escape_js("a\nb"), r"a\nb");
    }

    #[test]
    fn escape_html_special_chars() {
        assert_eq!(escape_html("<b>hi</b>"), "&lt;b&gt;hi&lt;/b&gt;");
    }

    #[test]
    fn prune_nodes_caps_at_max() {
        let mut kg = KnowledgeGraph::new();
        for i in 0..100 {
            kg.add_node(GraphNode {
                id: format!("n{}", i),
                label: format!("Node{}", i),
                source_file: "test.rs".into(),
                source_location: None,
                node_type: NodeType::Function,
                community: Some(i % 3),
                extra: HashMap::new(),
            })
            .unwrap();
        }
        for i in 0..50 {
            let _ = kg.add_edge(GraphEdge {
                source: "n0".into(),
                target: format!("n{}", i + 1),
                relation: "calls".into(),
                confidence: Confidence::Extracted,
                confidence_score: 1.0,
                source_file: "test.rs".into(),
                source_location: None,
                weight: 1.0,
                provenance: None,
                extra: HashMap::new(),
            });
        }

        let communities: HashMap<usize, Vec<String>> = HashMap::from([
            (0, (0..34).map(|i| format!("n{}", i)).collect()),
            (1, (34..67).map(|i| format!("n{}", i)).collect()),
            (2, (67..100).map(|i| format!("n{}", i)).collect()),
        ]);

        let pruned = prune_nodes(&kg, &communities, 20);
        assert!(pruned.len() <= 20, "should cap at 20, got {}", pruned.len());
        assert!(
            pruned.contains("n0"),
            "highest-degree node should be included"
        );
    }

    #[test]
    fn export_html_split_creates_files() {
        let dir = tempfile::tempdir().unwrap();
        let kg = sample_graph();
        let communities: HashMap<usize, Vec<String>> =
            [(0, vec!["a".into()]), (1, vec!["b".into()])].into();
        let labels: HashMap<usize, String> =
            [(0, "Cluster A".into()), (1, "Cluster B".into())].into();

        let path = export_html_split(&kg, &communities, &labels, dir.path()).unwrap();
        assert!(path.exists());
        assert!(path.join("index.html").exists(), "index.html should exist");
        assert!(
            path.join("community_0.html").exists(),
            "community_0.html should exist"
        );
        assert!(
            path.join("community_1.html").exists(),
            "community_1.html should exist"
        );

        let index = std::fs::read_to_string(path.join("index.html")).unwrap();
        assert!(index.contains("Overview"));
        assert!(index.contains("Cluster A"));
        assert!(index.contains("community_0.html"));

        let c0 = std::fs::read_to_string(path.join("community_0.html")).unwrap();
        assert!(c0.contains("Cluster A"));
        assert!(c0.contains("index.html"));
    }

    #[test]
    fn export_html_respects_max_nodes() -> anyhow::Result<()> {
        let mut kg = KnowledgeGraph::new();
        for i in 0..10 {
            kg.add_node(GraphNode {
                id: format!("n{i}"),
                label: format!("Node{i}"),
                source_file: "test.rs".into(),
                source_location: None,
                node_type: NodeType::Function,
                community: Some(0),
                extra: HashMap::new(),
            })
            .unwrap();
        }
        for i in 1..10 {
            let _ = kg.add_edge(GraphEdge {
                source: "n0".into(),
                target: format!("n{i}"),
                relation: "calls".into(),
                confidence: Confidence::Extracted,
                confidence_score: 1.0,
                source_file: "test.rs".into(),
                source_location: None,
                weight: 1.0,
                provenance: None,
                extra: HashMap::new(),
            });
        }

        let communities: HashMap<usize, Vec<String>> =
            [(0, (0..10).map(|i| format!("n{i}")).collect())].into();
        let labels: HashMap<usize, String> = [(0, "All".into())].into();
        let dir = tempfile::tempdir().unwrap();

        let path = export_html(&kg, &communities, &labels, dir.path(), Some(5)).unwrap();
        assert!(path.exists());
        let html = std::fs::read_to_string(&path).unwrap();
        assert!(html.contains("Node0"));
        assert!(
            html.contains("pruned") || html.contains("Showing"),
            "should indicate pruning occurred"
        );
        Ok(())
    }
}
