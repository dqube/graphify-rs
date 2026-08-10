//! The single-page interactive visualization template.
//!
//! Structure: two static blocks (`GRAPH_CSS`, `GRAPH_JS`) that never see
//! `format!`, plus one small skeleton that interpolates data. All graph data
//! enters through a dedicated `<script>` block as real JSON produced by
//! [`js_safe_json`], so escaping is `serde_json`'s problem, not a template
//! concern — the one thing JSON cannot defend against inside a script tag is
//! a literal `</script>` in a string, which `js_safe_json` neutralizes.
//!
//! Design: the same slate/sky token palette as `callflow.html`, so every page
//! graphify produces reads as one product. Tableau-10 community colors sit on
//! top of it — they are data encoding, not chrome, and must stay saturated.

pub(crate) fn escape_js(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

pub(crate) fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Serialize a value for embedding inside a `<script>` block.
///
/// `</` is split so a string containing `</script>` cannot terminate the
/// surrounding tag — the classic script-breakout. JSON string escaping covers
/// everything else.
pub(crate) fn js_safe_json(value: &serde_json::Value) -> String {
    serde_json::to_string(value)
        .unwrap_or_else(|_| "null".into())
        .replace("</", "<\\/")
}

/// Pinned vis-network with subresource integrity, mirroring the Python
/// exporter. Unpinned "latest" means a major bump silently breaks every
/// previously generated file.
pub(crate) const VIS_SCRIPT_TAG: &str = r#"<script src="https://unpkg.com/vis-network@9.1.6/standalone/umd/vis-network.min.js" integrity="sha384-Ux6phic9PEHJ38YtrijhkzyJ8yQlH8i/+buBR8s3mAZOJrP1gwyvAcIYl3GWtpX1" crossorigin="anonymous"></script>"#;

const GRAPH_CSS: &str = r#":root {
  --bg: #0f172a;
  --surface: #1e293b;
  --border: #334155;
  --text: #e2e8f0;
  --muted: #94a3b8;
  --accent: #38bdf8;
  --warn: #fbbf24;
  --hover: rgba(148, 163, 184, 0.08);
  --accent-soft: rgba(56, 189, 248, 0.1);
}
* { margin: 0; padding: 0; box-sizing: border-box; }
body {
  background: var(--bg); color: var(--text);
  font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', 'Inter', Roboto, sans-serif;
  font-size: 14px; display: flex; height: 100vh; overflow: hidden;
  -webkit-font-smoothing: antialiased;
}
#sidebar {
  width: 300px; min-width: 300px; background: var(--surface);
  display: flex; flex-direction: column; gap: 18px; padding: 18px 16px;
  overflow-y: auto; border-right: 1px solid var(--border);
  scrollbar-width: thin; scrollbar-color: var(--border) transparent;
}
#sidebar::-webkit-scrollbar { width: 8px; }
#sidebar::-webkit-scrollbar-thumb { background: var(--border); border-radius: 4px; }
#brand { display: flex; align-items: center; gap: 10px; }
#brand svg { flex-shrink: 0; }
#brand h1 { font-size: 15px; font-weight: 600; letter-spacing: -0.01em; }
#brand p { font-size: 12px; color: var(--muted); margin-top: 2px; }
.section-label {
  font-size: 11px; font-weight: 600; color: var(--muted);
  text-transform: uppercase; letter-spacing: 0.08em; margin-bottom: 8px;
}
#search-wrap { position: relative; }
#search {
  width: 100%; padding: 9px 12px 9px 34px; border-radius: 8px;
  border: 1px solid var(--border); background: var(--bg); color: var(--text);
  font-size: 13px; font-family: inherit; transition: border-color .15s;
}
#search::placeholder { color: var(--muted); }
#search:focus { outline: none; border-color: var(--accent); box-shadow: 0 0 0 3px var(--accent-soft); }
#search-icon {
  position: absolute; left: 11px; top: 50%; transform: translateY(-50%);
  color: var(--muted); pointer-events: none; display: flex;
}
#search-results {
  display: none; position: absolute; top: calc(100% + 6px); left: 0; right: 0;
  max-height: 280px; overflow-y: auto; background: var(--surface);
  border: 1px solid var(--border); border-radius: 10px; z-index: 30;
  box-shadow: 0 12px 32px rgba(2, 6, 23, 0.6); padding: 4px;
}
.search-result {
  padding: 7px 10px; font-size: 13px; cursor: pointer; border-radius: 6px;
  border-left: 3px solid transparent; overflow: hidden; text-overflow: ellipsis;
  white-space: nowrap;
}
.search-result:hover { background: var(--hover); }
.search-empty { padding: 8px 10px; font-size: 13px; color: var(--muted); }
.card {
  background: var(--bg); border: 1px solid var(--border);
  border-radius: 10px; padding: 12px;
}
#info-panel { font-size: 13px; line-height: 1.65; min-height: 108px; }
#info-panel .placeholder { color: var(--muted); font-style: italic; font-size: 12.5px; }
#info-panel .prop { color: var(--muted); }
#info-panel .val { color: var(--text); }
#info-panel .val.mono { font-family: ui-monospace, 'SF Mono', 'Cascadia Code', Menlo, monospace; font-size: 12px; word-break: break-all; }
.node-title { font-weight: 600; font-size: 13.5px; margin-bottom: 8px; padding-bottom: 8px; border-bottom: 1px solid var(--border); }
.neighbor-link {
  padding: 4px 9px; margin-top: 4px; font-size: 12px; cursor: pointer;
  border-left: 3px solid #888; background: var(--surface); border-radius: 0 6px 6px 0;
  overflow: hidden; text-overflow: ellipsis; white-space: nowrap; transition: background .12s;
}
.neighbor-link:hover { background: var(--hover); }
#legend-controls {
  display: flex; align-items: center; gap: 8px; font-size: 12.5px; color: var(--muted);
  padding-bottom: 8px; margin-bottom: 6px; border-bottom: 1px solid var(--border); cursor: pointer;
}
#legend { display: flex; flex-direction: column; gap: 1px; }
.legend-item {
  display: flex; align-items: center; gap: 8px; font-size: 13px;
  padding: 4px 6px; cursor: pointer; border-radius: 6px; transition: background .12s, opacity .15s;
}
.legend-item:hover { background: var(--hover); }
.legend-item.dimmed { opacity: 0.35; }
input[type="checkbox"] { accent-color: var(--accent); width: 14px; height: 14px; flex-shrink: 0; cursor: pointer; }
.legend-dot { width: 11px; height: 11px; border-radius: 50%; flex-shrink: 0; box-shadow: 0 0 0 2px rgba(255,255,255,0.07); }
.legend-label { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.legend-count {
  font-size: 11px; color: var(--muted); background: var(--hover);
  padding: 1px 7px; border-radius: 999px; font-variant-numeric: tabular-nums;
}
#hyperedges { font-size: 13px; }
#hyperedges ul { padding-left: 18px; color: var(--muted); }
#hyperedges li { margin-bottom: 4px; }
#prune-banner {
  background: rgba(251, 191, 36, 0.07); border: 1px solid rgba(251, 191, 36, 0.35);
  border-radius: 10px; padding: 10px 12px; font-size: 12px; line-height: 1.5; color: var(--warn);
}
#stats {
  margin-top: auto; padding-top: 12px; border-top: 1px solid var(--border);
  font-size: 12px; color: var(--muted); text-align: center; font-variant-numeric: tabular-nums;
}
#graph-container { flex: 1; position: relative; background: var(--bg); }
#toolbar { position: absolute; top: 14px; right: 14px; display: flex; gap: 6px; z-index: 20; }
.tbtn {
  min-width: 32px; height: 32px; padding: 0 9px; border-radius: 8px;
  background: rgba(30, 41, 59, 0.92); border: 1px solid var(--border);
  color: var(--text); cursor: pointer; font-size: 14px; font-family: inherit;
  display: flex; align-items: center; justify-content: center;
  transition: border-color .15s, color .15s;
}
.tbtn:hover { border-color: var(--accent); color: var(--accent); }
#loading {
  position: absolute; top: 50%; left: 50%; transform: translate(-50%, -50%);
  display: flex; flex-direction: column; align-items: center; gap: 12px;
  background: var(--surface); border: 1px solid var(--border); border-radius: 12px;
  padding: 22px 30px; z-index: 10; box-shadow: 0 12px 32px rgba(2, 6, 23, 0.5);
}
#loading span { font-size: 13px; color: var(--muted); }
.spinner {
  width: 26px; height: 26px; border-radius: 50%;
  border: 3px solid var(--border); border-top-color: var(--accent);
  animation: spin 0.8s linear infinite;
}
@keyframes spin { to { transform: rotate(360deg); } }"#;

const GRAPH_JS: &str = r#"(function() {
    var esc = function(s) {
        var d = document.createElement('div');
        d.textContent = s == null ? '' : String(s);
        return d.innerHTML;
    };
    var PLACEHOLDER = '<div class="placeholder">Click a node to see details</div>';

    var container = document.getElementById('graph-container');
    var loading = document.getElementById('loading');
    var panel = document.getElementById('info-panel');

    // Everything below the CDN guard needs vis-network; everything above must
    // not. The sidebar is rendered first so a blocked or offline CDN leaves a
    // usable legend and a readable error instead of a dead page.
    var network = null;
    var nodes = null;

    function nodeColor(n) {
        return (n && n.color && n.color.background) || '#888888';
    }

    // ── Legend (data-only: works without vis) ──────────────────────────
    var legendDiv = document.getElementById('legend');
    var selectAll = document.getElementById('select-all-cb');

    function setCommunityVisible(cid, visible) {
        if (!nodes) { return; }
        var updates = [];
        nodes.forEach(function(n) {
            if (n.community === cid && Boolean(n.hidden) === visible) {
                updates.push({ id: n.id, hidden: !visible });
            }
        });
        if (updates.length > 0) { nodes.update(updates); }
    }

    function updateSelectAll() {
        var cbs = legendDiv.querySelectorAll('.legend-cb');
        var checked = 0;
        cbs.forEach(function(cb) { if (cb.checked) { checked += 1; } });
        selectAll.checked = checked === cbs.length;
        selectAll.indeterminate = checked > 0 && checked < cbs.length;
    }

    legendDiv.innerHTML = LEGEND.map(function(item) {
        return '<label class="legend-item" data-cid="' + item.cid + '">' +
            '<input type="checkbox" class="legend-cb" checked>' +
            '<span class="legend-dot" style="background:' + item.color + '"></span>' +
            '<span class="legend-label" title="' + esc(item.label) + '">' + esc(item.label) + '</span>' +
            '<span class="legend-count">' + item.count + '</span></label>';
    }).join('');
    legendDiv.querySelectorAll('.legend-item').forEach(function(row) {
        var cid = parseInt(row.getAttribute('data-cid'), 10);
        var cb = row.querySelector('.legend-cb');
        cb.addEventListener('change', function() {
            setCommunityVisible(cid, cb.checked);
            row.classList.toggle('dimmed', !cb.checked);
            updateSelectAll();
        });
    });
    selectAll.addEventListener('change', function() {
        var on = selectAll.checked;
        legendDiv.querySelectorAll('.legend-item').forEach(function(row) {
            var cb = row.querySelector('.legend-cb');
            if (cb.checked !== on) {
                cb.checked = on;
                setCommunityVisible(parseInt(row.getAttribute('data-cid'), 10), on);
                row.classList.toggle('dimmed', !on);
            }
        });
        selectAll.indeterminate = false;
    });

    // ── Search (matches from the raw data: works without vis) ──────────
    var searchInput = document.getElementById('search');
    var resultsBox = document.getElementById('search-results');
    var searchTimer = null;
    searchInput.addEventListener('input', function() {
        clearTimeout(searchTimer);
        searchTimer = setTimeout(function() {
            var term = searchInput.value.trim().toLowerCase();
            if (!term) {
                resultsBox.style.display = 'none';
                resultsBox.innerHTML = '';
                return;
            }
            var matches = [];
            for (var i = 0; i < RAW_NODES.length && matches.length < 20; i++) {
                var n = RAW_NODES[i];
                if (String(n.label).toLowerCase().indexOf(term) !== -1 ||
                    String(n.id).toLowerCase().indexOf(term) !== -1) {
                    matches.push(n);
                }
            }
            if (matches.length === 0) {
                resultsBox.innerHTML = '<div class="search-empty">No matches</div>';
            } else {
                resultsBox.innerHTML = matches.map(function(n) {
                    return '<div class="search-result" data-id="' + esc(n.id) + '" style="border-left-color:' + nodeColor(n) + '">' + esc(n.label) + '</div>';
                }).join('');
                resultsBox.querySelectorAll('.search-result').forEach(function(el) {
                    el.addEventListener('click', function() {
                        resultsBox.style.display = 'none';
                        focusNode(el.getAttribute('data-id'));
                    });
                });
            }
            resultsBox.style.display = 'block';
        }, 150);
    });
    document.addEventListener('click', function(ev) {
        if (ev.target !== searchInput && !resultsBox.contains(ev.target)) {
            resultsBox.style.display = 'none';
        }
    });

    // ── CDN guard ───────────────────────────────────────────────────────
    if (typeof vis === 'undefined') {
        loading.innerHTML =
            '<span style="color:var(--warn);font-size:14px">Could not load the graph library</span>' +
            '<span>vis-network did not load from the CDN.<br>' +
            'This page needs internet access, and some embedded previews block external scripts —<br>' +
            'try opening it in a regular browser.</span>';
        return;
    }

    nodes = new vis.DataSet(RAW_NODES);
    var edges = new vis.DataSet(RAW_EDGES);

    var options = {
        layout: IS_LARGE ? { improvedLayout: false } : {},
        physics: IS_LARGE ? {
            solver: 'barnesHut',
            barnesHut: {
                gravitationalConstant: -8000,
                centralGravity: 0.1,
                springLength: 120,
                springConstant: 0.04,
                damping: 0.3,
                avoidOverlap: 0.5
            },
            stabilization: { iterations: 150, fit: true },
            adaptiveTimestep: true
        } : {
            solver: 'forceAtlas2Based',
            forceAtlas2Based: {
                gravitationalConstant: -60,
                centralGravity: 0.005,
                springLength: 120,
                springConstant: 0.08,
                damping: 0.4,
                avoidOverlap: 0.8
            },
            stabilization: { iterations: 200, fit: true }
        },
        nodes: { shape: 'dot', borderWidth: 1.5 },
        edges: {
            color: { color: '#475569', highlight: '#38bdf8', hover: '#38bdf8' },
            arrows: { to: { enabled: true, scaleFactor: 0.5 } },
            smooth: { type: 'continuous', roundness: 0.2 },
            selectionWidth: 3
        },
        interaction: {
            hover: true,
            tooltipDelay: 100,
            hideEdgesOnDrag: true,
            zoomView: true,
            dragView: true
        }
    };

    network = new vis.Network(container, { nodes: nodes, edges: edges }, options);

    // Both paths must freeze physics: on a slow layout the timeout used to
    // hide the spinner while the simulation kept burning CPU behind it.
    var frozen = false;
    function freeze() {
        if (frozen) { return; }
        frozen = true;
        loading.style.display = 'none';
        network.setOptions({ physics: { enabled: false } });
    }
    network.on('stabilizationIterationsDone', freeze);
    setTimeout(freeze, 10000);

    // Canvas toolbar: zoom, fit, and a re-run of the layout.
    document.getElementById('zoom-in').addEventListener('click', function() {
        network.moveTo({ scale: network.getScale() * 1.4, animation: { duration: 200, easingFunction: 'easeInOutQuad' } });
    });
    document.getElementById('zoom-out').addEventListener('click', function() {
        network.moveTo({ scale: network.getScale() / 1.4, animation: { duration: 200, easingFunction: 'easeInOutQuad' } });
    });
    document.getElementById('zoom-fit').addEventListener('click', function() {
        network.fit({ animation: { duration: 300, easingFunction: 'easeInOutQuad' } });
    });
    document.getElementById('relayout').addEventListener('click', function() {
        frozen = false;
        loading.style.display = 'flex';
        network.setOptions({ physics: { enabled: true } });
        network.stabilize(IS_LARGE ? 150 : 200);
    });

    function showInfo(nodeId) {
        var node = nodes.get(nodeId);
        if (!node) { return; }
        var html =
            '<div class="node-title" style="color:' + nodeColor(node) + '">' + esc(node.label) + '</div>' +
            '<div><span class="prop">Type</span> · <span class="val">' + esc(node.file_type || 'unknown') + '</span></div>' +
            '<div><span class="prop">Community</span> · <span class="val">' + esc(node.community_name || ('Community ' + node.community)) + '</span></div>' +
            '<div><span class="prop">Degree</span> · <span class="val">' + esc(node.degree) + '</span></div>';
        if (node.source_file) {
            html += '<div class="val mono" style="margin-top:4px">' + esc(node.source_file) + '</div>';
        }
        var neighborIds = network.getConnectedNodes(nodeId);
        if (neighborIds.length > 0) {
            html += '<div class="prop" style="margin-top:10px">Connected (' + neighborIds.length + ')</div>';
            neighborIds.slice(0, 15).forEach(function(nid) {
                var nb = nodes.get(nid);
                if (!nb) { return; }
                html += '<div class="neighbor-link" data-id="' + esc(nid) + '" style="border-left-color:' + nodeColor(nb) + '">' + esc(nb.label) + '</div>';
            });
            if (neighborIds.length > 15) {
                html += '<div class="prop" style="margin-top:4px">… +' + (neighborIds.length - 15) + ' more</div>';
            }
        }
        panel.innerHTML = html;
        panel.querySelectorAll('.neighbor-link').forEach(function(el) {
            el.addEventListener('click', function() { focusNode(el.getAttribute('data-id')); });
        });
    }

    function focusNode(id) {
        if (!network || !nodes || !nodes.get(id)) { return; }
        network.focus(id, { scale: 1.5, animation: true });
        network.selectNodes([id]);
        showInfo(id);
    }

    // vis click params occasionally miss the node under the cursor; track
    // hover as a fallback so clicks always land.
    var hoveredNodeId = null;
    network.on('hoverNode', function(p) { hoveredNodeId = p.node; container.style.cursor = 'pointer'; });
    network.on('blurNode', function() { hoveredNodeId = null; container.style.cursor = 'default'; });
    network.on('click', function(params) {
        var id = params.nodes.length > 0 ? params.nodes[0] : hoveredNodeId;
        if (id != null && nodes.get(id)) {
            showInfo(id);
        } else {
            panel.innerHTML = PLACEHOLDER;
        }
    });

})();"#;

/// A small node-link glyph for the sidebar header — chrome the product owns,
/// rather than an emoji that renders differently on every platform.
const BRAND_SVG: &str = r##"<svg width="26" height="26" viewBox="0 0 26 26" fill="none" xmlns="http://www.w3.org/2000/svg"><circle cx="6" cy="20" r="3.2" fill="#38bdf8"/><circle cx="20" cy="18" r="2.6" fill="#818cf8"/><circle cx="13" cy="6" r="3.2" fill="#34d399"/><path d="M8 17.5 11.5 9M15.5 8l3.5 7.5M9 19.5l8-1.2" stroke="#475569" stroke-width="1.6" stroke-linecap="round"/></svg>"##;

pub(crate) fn build_html_template(
    nodes_json: &str,
    edges_json: &str,
    legend_json: &str,
    hyperedge_html: &str,
    prune_banner: &str,
    stats_html: &str,
    is_large: bool,
) -> String {
    // A heading over an empty list is clutter; most graphs have no hyperedges.
    let hyperedge_section = if hyperedge_html.is_empty() {
        String::new()
    } else {
        format!(
            r#"<div id="hyperedges">
        <div class="section-label">Hyperedges</div>
        <ul>{hyperedge_html}</ul>
    </div>"#
        )
    };

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Knowledge Graph</title>
{VIS_SCRIPT_TAG}
<style>
{GRAPH_CSS}
</style>
</head>
<body>
<div id="sidebar">
    <div id="brand">
        {BRAND_SVG}
        <div>
            <h1>Knowledge Graph</h1>
            <p>Click a node to inspect · Scroll to zoom</p>
        </div>
    </div>
    {prune_banner}
    <div id="search-wrap">
        <span id="search-icon"><svg width="14" height="14" viewBox="0 0 14 14" fill="none" xmlns="http://www.w3.org/2000/svg"><circle cx="6" cy="6" r="4.5" stroke="currentColor" stroke-width="1.5"/><path d="m9.5 9.5 3 3" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/></svg></span>
        <input id="search" type="text" placeholder="Search nodes…" autocomplete="off" />
        <div id="search-results"></div>
    </div>
    <div>
        <div class="section-label">Node Info</div>
        <div id="info-panel" class="card"><div class="placeholder">Click a node to see details</div></div>
    </div>
    <div>
        <div class="section-label">Communities</div>
        <label id="legend-controls"><input type="checkbox" id="select-all-cb" checked> Select All</label>
        <div id="legend"></div>
    </div>
    {hyperedge_section}
    <div id="stats">{stats_html}</div>
</div>
<div id="graph-container">
    <div id="toolbar">
        <button class="tbtn" id="zoom-in" title="Zoom in">+</button>
        <button class="tbtn" id="zoom-out" title="Zoom out">−</button>
        <button class="tbtn" id="zoom-fit" title="Fit graph to view">Fit</button>
        <button class="tbtn" id="relayout" title="Re-run layout">⟳</button>
    </div>
    <div id="loading"><div class="spinner"></div><span>Laying out graph…</span></div>
</div>
<script>
var RAW_NODES = {nodes_json};
var RAW_EDGES = {edges_json};
var LEGEND = {legend_json};
var IS_LARGE = {is_large};
</script>
<script>
{GRAPH_JS}
</script>
</body>
</html>"#,
    )
}
