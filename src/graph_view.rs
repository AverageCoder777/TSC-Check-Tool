//! Self-contained dependency-graph viewer.
//!
//! Extracts the local dependency subgraph around a selected asset, compiles a
//! self-contained HTML page with an interactive `vis-network` physics graph,
//! saves it to the system temp directory and opens it in the default browser
//! via the `open` crate.

use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const VIS_NETWORK_CDN: &str =
    "https://unpkg.com/vis-network/standalone/umd/vis-network.min.js";

const GRAPH_HTML_NAME: &str = "tsc_graph_view.html";
const GLOBAL_GRAPH_HTML_NAME: &str = "tsc_global_graph.html";
const FULL_GRAPH_HTML_NAME: &str = "tsc_full_graph.html";

/// Visual categories for graph nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeKind {
    /// The selected (central) asset.
    Center,
    /// Blueprint referencing the selected asset (inbound).
    Inbound,
    /// Asset referenced by the selected asset (outbound).
    Outbound,
    /// Dependency that could not be resolved on disk.
    Missing,
}

impl NodeKind {
    fn color(self) -> &'static str {
        match self {
            NodeKind::Center => "#3b82f6",     // blue
            NodeKind::Inbound => "#10b981",    // green
            NodeKind::Outbound => "#f59e0b",   // amber
            NodeKind::Missing => "#ef4444",    // red
        }
    }

    fn shape(self) -> &'static str {
        match self {
            NodeKind::Center => "box",
            NodeKind::Inbound | NodeKind::Outbound | NodeKind::Missing => "ellipse",
        }
    }

    fn label_prefix(self) -> Option<&'static str> {
        match self {
            NodeKind::Missing => Some("[MISSING] "),
            _ => None,
        }
    }
}

/// Serialized node payload for vis-network.
#[derive(Serialize)]
struct GraphNode {
    id: usize,
    label: String,
    /// Full path, rendered as the Vis.js hover tooltip.
    title: String,
    color: String,
    shape: &'static str,
    font: GraphFont,
    #[serde(skip_serializing_if = "Option::is_none")]
    border_width: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    border_dash: Option<[usize; 2]>,
}

#[derive(Serialize)]
struct GraphFont {
    size: usize,
    color: String,
}

/// Serialized edge payload for vis-network.
#[derive(Serialize)]
struct GraphEdge {
    from: usize,
    to: usize,
    arrows: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    dashes: Option<[usize; 2]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    color: Option<String>,
}

/// Build a single standalone HTML document embedding the vis-network graph.
fn build_html(selected_path: &Path,
              inbound_refs: &[PathBuf],
              outbound_refs: &[PathBuf],
              missing_refs: &[PathBuf]) -> String
{
    let mut nodes: Vec<GraphNode> = Vec::new();
    let mut edges: Vec<GraphEdge> = Vec::new();
    let mut index_of: HashMap<PathBuf, usize> = HashMap::new();

    let tooltip_color = "#e5e7eb";

    /// Register a node (deduplicated by normalized path) and return its id.
    macro_rules! add_node {
        ($path:expr, $kind:expr) => {{
            let norm = crate::fs_utils::normalize_path($path);
            if let Some(&id) = index_of.get(&norm) {
                id
            } else {
                let id = nodes.len();
                index_of.insert(norm.clone(), id);
                let label = norm
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| norm.display().to_string());
                let label = match $kind.label_prefix() {
                    Some(prefix) => format!("{prefix}{label}"),
                    None => label,
                };
                let title = crate::fs_utils::to_assets_relative_display(&norm);
                let (border_width, border_dash) = match $kind {
                    NodeKind::Center => (Some(3usize), None),
                    NodeKind::Missing => (None, Some([5usize, 2usize])),
                    _ => (None, None),
                };
                nodes.push(GraphNode {
                    id,
                    label,
                    title,
                    color: $kind.color().to_string(),
                    shape: $kind.shape(),
                    font: GraphFont { size: 14, color: tooltip_color.to_string() },
                    border_width,
                    border_dash,
                });
                id
            }
        }};
    }

    let center_id = add_node!(selected_path, NodeKind::Center);

    for path in inbound_refs {
        let id = add_node!(path, NodeKind::Inbound);
        edges.push(GraphEdge { from: id, to: center_id, arrows: "to", dashes: None, color: None });
    }
    for path in outbound_refs {
        let id = add_node!(path, NodeKind::Outbound);
        edges.push(GraphEdge { from: center_id, to: id, arrows: "to", dashes: None, color: None });
    }
    for path in missing_refs {
        let id = add_node!(path, NodeKind::Missing);
        edges.push(GraphEdge {
            from: center_id,
            to: id,
            arrows: "to",
            dashes: Some([4usize, 2usize]),
            color: Some(NodeKind::Missing.color().to_string()),
        });
    }

    let nodes_json = serde_json::to_string(&nodes).expect("nodes serialize");
    let edges_json = serde_json::to_string(&edges).expect("edges serialize");

    let mut html = r##"<!DOCTYPE html>
<html lang="ru">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Граф связей зависимостей</title>
<script src="__VIS_CDN__"></script>
<style>
    html, body { margin: 0; padding: 0; width: 100%; height: 100%; overflow: hidden; background: #111827; color: #e5e7eb; font-family: system-ui, -apple-system, Segoe UI, Roboto, sans-serif; }
    #network { width: 100vw; height: 100vh; background: #111827; display: block; }
    .legend {
        position: absolute;
        top: 14px;
        left: 14px;
        z-index: 10;
        background: rgba(17, 24, 39, 0.85);
        backdrop-filter: blur(6px);
        border: 1px solid #374151;
        border-radius: 8px;
        padding: 10px 12px;
        font-size: 12px;
        line-height: 1.5;
        color: #e5e7eb;
        box-shadow: 0 4px 12px rgba(0,0,0,0.4);
        user-select: none;
    }
    .legend-title { font-weight: 700; margin-bottom: 6px; color: #fbbf24; }
    .legend-item { display: flex; align-items: center; gap: 8px; margin: 3px 0; }
    .dot { width: 12px; height: 12px; border-radius: 50%; flex-shrink: 0; border: 1px solid rgba(255,255,255,0.35); }
    .dot.missing { border-style: dashed; }
    .dot.center { shape: box; border-radius: 2px; }
    .hint { margin-top: 8px; font-size: 11px; color: #9ca3af; }
</style>
</head>
<body>
<div class="legend">
    <div class="legend-title">Граф связей</div>
    <div class="legend-item"><div class="dot center" style="background:__CENTER_COLOR__"></div><div>Выбранный файл</div></div>
    <div class="legend-item"><div class="dot" style="background:__INBOUND_COLOR__"></div><div>Родители (входящие ссылки)</div></div>
    <div class="legend-item"><div class="dot" style="background:__OUTBOUND_COLOR__"></div><div>Зависимости (исходящие ссылки)</div></div>
    <div class="legend-item"><div class="dot missing" style="background:__MISSING_COLOR__"></div><div>Отсутствующие на диске</div></div>
    <div class="hint">Наведите курсор на узел для полного пути. Перетаскивайте узлы мышью.</div>
</div>
<div id="network"></div>
<script>
    const nodes = new vis.DataSet(__NODES_JSON__);
    const edges = new vis.DataSet(__EDGES_JSON__);
    const container = document.getElementById("network");
    const data = { nodes, edges };
    const options = {
        nodes: {
            font: { color: "__TOOLTIP_COLOR__", size: 13, strokeWidth: 0, strokeColor: "#111827" },
            borderWidth: 2,
            shadow: { enabled: true, color: "rgba(0,0,0,0.6)", size: 12, x: 2, y: 2 },
        },
        edges: {
            arrows: { to: { enabled: true, scaleFactor: 0.5 } },
            smooth: false,
            color: { color: "#6b7280", highlight: "#9ca3af", hover: "#9ca3af" },
            width: 1.5,
            shadow: false,
        },
        layout: { improvedLayout: true },
        physics: {
            enabled: true,
            solver: "forceAtlas2Based",
            stabilization: { enabled: true, iterations: 200, updateInterval: 25, onlyDynamicEdges: false, fit: true },
            forceAtlas2Based: {
                springLength: 110,
                springConstant: 0.05,
                damping: 0.45,
                avoidOverlap: 0.65,
                centralGravity: 0.01,
                gravitationalConstant: -40,
            },
        },
        interaction: {
            hover: true,
            tooltipDelay: 150,
            hideEdgesOnDrag: false,
            selectable: true,
            navigationButtons: true,
            keyboard: { enabled: true },
        },
        configure: { enabled: false },
    };
    new vis.Network(container, data, options);
</script>
</body>
</html>"##
    .to_string();

    html = html.replace("__VIS_CDN__", VIS_NETWORK_CDN);
    html = html.replace("__CENTER_COLOR__", NodeKind::Center.color());
    html = html.replace("__INBOUND_COLOR__", NodeKind::Inbound.color());
    html = html.replace("__OUTBOUND_COLOR__", NodeKind::Outbound.color());
    html = html.replace("__MISSING_COLOR__", NodeKind::Missing.color());
    html = html.replace("__TOOLTIP_COLOR__", tooltip_color);
    html = html.replace("__NODES_JSON__", &nodes_json);
    html = html.replace("__EDGES_JSON__", &edges_json);
    html
}

/// Serialize the local dependency subgraph to a self-contained HTML file in
/// the system temp directory and open it in the default browser.
pub fn open_dependency_graph_in_browser(
    selected_path: &Path,
    inbound_refs: &[PathBuf],
    outbound_refs: &[PathBuf],
    missing_refs: &[PathBuf],
) -> Result<(), Box<dyn std::error::Error>>
{
    let html = build_html(selected_path, inbound_refs, outbound_refs, missing_refs);
    let out_path = std::env::temp_dir().join(GRAPH_HTML_NAME);
    std::fs::write(&out_path, html)?;
    open::that(&out_path)?;
    Ok(())
}

/// Visual style for a global-graph node, derived from file kind and status.
#[derive(Debug, Clone, Copy)]
struct NodeStyle {
    color: &'static str,
    shape: &'static str,
    border_dash: Option<[usize; 2]>,
    label_prefix: Option<&'static str>,
}

/// Derive visual style from extension, problem status, and missing-on-disk status.
fn node_style(
    path: &Path,
    is_problem: bool,
    is_missing: bool,
    kind: Option<crate::auditor::AssetResourceKind>,
) -> NodeStyle {
    if is_missing {
        return NodeStyle {
            color: "#ef4444",
            shape: "box",
            border_dash: Some([5, 2]),
            label_prefix: Some("[MISSING] "),
        };
    }
    if !is_problem && kind == Some(crate::auditor::AssetResourceKind::Unused) {
        return NodeStyle {
            color: "#6b7280",
            shape: "ellipse",
            border_dash: None,
            label_prefix: None,
        };
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    match ext.as_str() {
        "bin" | "xml" => NodeStyle {
            color: "#3b82f6",
            shape: "box",
            border_dash: None,
            label_prefix: None,
        },
        "geopcdx" | "xsec" => NodeStyle {
            color: "#10b981",
            shape: "ellipse",
            border_dash: None,
            label_prefix: None,
        },
        "tgpcdx" => NodeStyle {
            color: "#f59e0b",
            shape: "ellipse",
            border_dash: None,
            label_prefix: None,
        },
        _ => NodeStyle {
            color: "#94a3b8",
            shape: "ellipse",
            border_dash: None,
            label_prefix: None,
        },
    }
}

/// Serialized node payload for the global graph.
#[derive(Serialize)]
struct GlobalGraphNode {
    id: usize,
    label: String,
    title: String,
    color: String,
    shape: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    border_dash: Option<[usize; 2]>,
}

/// Serialized edge payload for the global graph.
#[derive(Serialize)]
struct GlobalGraphEdge {
    from: usize,
    to: usize,
}

/// Build the standalone HTML document for the global issues graph.
fn build_global_html(nodes_json: &str, edges_json: &str, node_count: usize, edge_count: usize) -> String
{
    let mut html = r##"<!DOCTYPE html>
<html lang="ru">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Глобальный граф связей зависимостей</title>
<script src="__VIS_CDN__"></script>
<style>
    html, body { margin: 0; padding: 0; width: 100%; height: 100%; overflow: hidden; background: #0f172a; color: #e5e7eb; font-family: system-ui, -apple-system, Segoe UI, Roboto, sans-serif; }
    #network { width: 100vw; height: 100vh; background: #0f172a; display: block; }
    .controls {
        position: absolute;
        top: 12px;
        right: 12px;
        z-index: 50;
        background: rgba(15, 23, 42, 0.9);
        backdrop-filter: blur(8px);
        border: 1px solid #334155;
        border-radius: 10px;
        padding: 10px 12px;
        box-shadow: 0 8px 24px rgba(0,0,0,0.5);
        user-select: none;
        display: flex;
        flex-direction: column;
        gap: 8px;
        align-items: flex-start;
        min-width: 260px;
    }
    .controls-title { font-weight: 700; font-size: 13px; color: #fbbf24; margin: 0; }
    .badge-row { display: flex; gap: 10px; font-size: 12px; color: #cbd5e1; flex-wrap: wrap; }
    .badge { background: #1e293b; border: 1px solid #475569; border-radius: 6px; padding: 2px 8px; color: #e5e7eb; font-weight: 600; white-space: nowrap; }
    .badge .num { color: #60a5fa; }
    .search-box { width: 100%; display: flex; gap: 6px; margin-top: 4px; }
    .search-box input { flex: 1; background: #0f172a; border: 1px solid #475569; border-radius: 6px; padding: 6px 8px; color: #e5e7eb; font-size: 12px; outline: none; }
    .search-box input:focus { border-color: #3b82f6; }
    .search-box button { background: #3b82f6; border: none; border-radius: 6px; padding: 6px 10px; color: #ffffff; font-size: 12px; font-weight: 600; cursor: pointer; }
    .search-box button:hover { background: #60a5fa; }
    .physics-toggle { width: 100%; background: #10b981; border: none; border-radius: 6px; padding: 6px 10px; color: #ffffff; font-size: 12px; font-weight: 600; cursor: pointer; margin-top: 2px; }
    .physics-toggle:hover { background: #34d399; }
    .physics-toggle.paused { background: #ef4444; }
    .physics-toggle.paused:hover { background: #f87171; }
    .cluster-info { font-size: 12px; color: #94a3b8; margin-top: 2px; line-height: 1.4; }
    .hint { font-size: 11px; color: #94a3b8; margin-top: 4px; }
</style>
</head>
<body>
<div class="controls">
    <div class="controls-title">Глобальный граф связей</div>
    <div class="badge-row">
        <span>Узлов: <span class="badge"><span class="num" id="node-count">__NODE_COUNT__</span></span></span>
        <span>Рёбер: <span class="badge"><span class="num" id="edge-count">__EDGE_COUNT__</span></span></span>
    </div>
    <div class="search-box">
        <input type="text" id="graph-search" placeholder="Поиск узла...">
        <button id="graph-center">Центр</button>
    </div>
    <button class="physics-toggle" id="graph-pause">Приостановить физику</button>
    <div class="cluster-info">Компоненты связей расставляются 자동но (Barnes-Hut). Наведите на узел для полного пути.</div>
    <div class="hint">Введите часть имени файла и нажмите Enter или "Центр" для камеры на узле.</div>
</div>
<div id="network"></div>
<script>
    const nodes = new vis.DataSet(__NODES_JSON__);
    const edges = new vis.DataSet(__EDGES_JSON__);
    const container = document.getElementById("network");
    const data = { nodes, edges };
    const options = {
        nodes: {
            font: { color: "#e5e7eb", size: 13, strokeWidth: 0, strokeColor: "#0f172a" },
            borderWidth: 2,
            shadow: { enabled: true, color: "rgba(0,0,0,0.7)", size: 10, x: 2, y: 2 },
        },
        edges: {
            arrows: { to: { enabled: true, scaleFactor: 0.5 } },
            smooth: false,
            color: { color: "#475569", highlight: "#94a3b8", hover: "#94a3b8" },
            width: 1.2,
            shadow: false,
        },
        layout: { improvedLayout: true },
        physics: {
            enabled: true,
            solver: "barnesHut",
            barnesHut: {
                gravitationalConstant: -4000,
                centralGravity: 0.05,
                springLength: 90,
                springConstant: 0.04,
                damping: 0.09,
                avoidOverlap: 0.8,
            },
            stabilization: { enabled: true, iterations: 150, updateInterval: 25 },
        },
        interaction: {
            hover: true,
            tooltipDelay: 100,
            hideEdgesOnDrag: false,
            selectable: true,
            navigationButtons: true,
            keyboard: { enabled: true, bindToWindow: false },
        },
        configure: { enabled: false },
    };
    const network = new vis.Network(container, data, options);

    const searchInput = document.getElementById("graph-search");
    const centerBtn = document.getElementById("graph-center");
    const pauseBtn = document.getElementById("graph-pause");
    let physicsPaused = false;

    let searchMatches = [];
    let currentMatchIdx = 0;
    let lastSearchQuery = "";

    function norm(str) {
        return (str || "").toLowerCase().replace(/\\\\/g, "/");
    }

    function centerOnNode(query) {
        const q = norm(query.trim());
        if (!q) return;

        if (q !== lastSearchQuery) {
            lastSearchQuery = q;
            searchMatches = nodes.get().filter(n => {
                const label = norm(n.label);
                const title = norm(n.title);
                return label.includes(q) || title.includes(q);
            });
            currentMatchIdx = 0;
        } else {
            currentMatchIdx = (currentMatchIdx + 1) % (searchMatches.length || 1);
        }

        if (searchMatches.length === 0) {
            searchInput.style.borderColor = "#ef4444";
            setTimeout(() => { searchInput.style.borderColor = "#475569"; }, 1500);
            return;
        }

        const target = searchMatches[currentMatchIdx];
        network.focus(target.id, { scale: 1.4, animation: { duration: 600 } });
        network.selectNodes([target.id]);
    }

    centerBtn.addEventListener("click", () => centerOnNode(searchInput.value));
    searchInput.addEventListener("keydown", (e) => {
        e.stopPropagation();
        if (e.key === "Enter") centerOnNode(searchInput.value);
    });
    searchInput.addEventListener("keyup", (e) => e.stopPropagation());

    pauseBtn.addEventListener("click", () => {
        physicsPaused = !physicsPaused;
        network.setOptions({ physics: { enabled: !physicsPaused } });
        pauseBtn.textContent = physicsPaused ? "Возобновить физику" : "Приостановить физику";
        pauseBtn.classList.toggle("paused", physicsPaused);
    });
</script>
</body>
</html>"##
    .to_string();

    html = html.replace("__VIS_CDN__", VIS_NETWORK_CDN);
    html = html.replace("__NODE_COUNT__", &node_count.to_string());
    html = html.replace("__EDGE_COUNT__", &edge_count.to_string());
    html = html.replace("__NODES_JSON__", nodes_json);
    html = html.replace("__EDGES_JSON__", edges_json);
    html
}

/// Generate a global dependency graph containing ALL detected issues and their
/// immediate dependencies, then open it in the default browser.
///
/// Disconnected components (islands) are laid out side-by-side by the
/// Barnes-Hut solver with strong inter-island repulsion.
pub fn generate_global_issues_graph(
    issues: &[crate::auditor::BrokenResourceRef],
    graph: &crate::auditor::DependencyGraph,
) -> Result<(), Box<dyn std::error::Error>>
{
    // 1. Collect problem paths from issues.
    let mut problem_paths: HashSet<PathBuf> = HashSet::new();
    let mut path_kind: HashMap<PathBuf, crate::auditor::AssetResourceKind> = HashMap::new();

    for issue in issues {
        let src = crate::fs_utils::normalize_path(&issue.parent_file);
        let dst = crate::fs_utils::normalize_path(&issue.expected_path);
        problem_paths.insert(src.clone());
        problem_paths.insert(dst.clone());
        path_kind.insert(src, issue.kind);
        path_kind.insert(dst, issue.kind);
    }

    // 2. Expand to immediate neighbours from the dependency graph.
    let mut all_paths: HashSet<PathBuf> = problem_paths.clone();
    for p in &problem_paths {
        if let Some(neighbours) = graph.incoming.get(p) {
            all_paths.extend(neighbours.iter().cloned());
        }
        if let Some(neighbours) = graph.outgoing.get(p) {
            all_paths.extend(neighbours.iter().cloned());
        }
    }

    // 3. Assign indices and build nodes.
    let mut path_to_idx: HashMap<PathBuf, usize> = HashMap::new();
    let mut nodes: Vec<GlobalGraphNode> = Vec::new();

    for path in &all_paths {
        let idx = nodes.len();
        path_to_idx.insert(path.clone(), idx);

        let is_problem = problem_paths.contains(path);
        let is_missing = !path.is_file();
        let kind = path_kind.get(path).copied();
        let style = node_style(path, is_problem, is_missing, kind);

        let label = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string());
        let label = match style.label_prefix {
            Some(prefix) => format!("{prefix}{label}"),
            None => label,
        };
        let title = crate::fs_utils::to_assets_relative_display(path);

        nodes.push(GlobalGraphNode {
            id: idx,
            label,
            title,
            color: style.color.to_string(),
            shape: style.shape,
            border_dash: style.border_dash,
        });
    }

    // 4. Build edges from the dependency graph (only between nodes in our set).
    let mut edges: Vec<GlobalGraphEdge> = Vec::new();
    for (src, dsts) in &graph.outgoing {
        if let Some(&src_idx) = path_to_idx.get(src) {
            for dst in dsts {
                if let Some(&dst_idx) = path_to_idx.get(dst) {
                    edges.push(GlobalGraphEdge { from: src_idx, to: dst_idx });
                }
            }
        }
    }

    // 5. Add issue-specific edges (parent -> expected) to ensure each issue is linked.
    for issue in issues {
        let src = crate::fs_utils::normalize_path(&issue.parent_file);
        let dst = crate::fs_utils::normalize_path(&issue.expected_path);
        if let (Some(&src_idx), Some(&dst_idx)) = (path_to_idx.get(&src), path_to_idx.get(&dst)) {
            edges.push(GlobalGraphEdge { from: src_idx, to: dst_idx });
        }
    }

    // 6. Deduplicate edges.
    let mut seen_edges: HashSet<(usize, usize)> = HashSet::new();
    edges.retain(|e| seen_edges.insert((e.from, e.to)));

    let edge_count = edges.len();

    // 7. Serialize and build HTML.
    let nodes_json = serde_json::to_string(&nodes).expect("nodes serialize");
    let edges_json = serde_json::to_string(&edges).expect("edges serialize");
    let html = build_global_html(&nodes_json, &edges_json, nodes.len(), edge_count);

    let out_path = std::env::temp_dir().join(GLOBAL_GRAPH_HTML_NAME);
    std::fs::write(&out_path, html)?;
    open::that(&out_path)?;
    Ok(())
}

/// Determine the visual style for a full-directory-graph node based on extension
/// and dead/missing status.
fn full_graph_node_style(
    path: &Path,
    is_dead: bool,
    is_missing: bool,
) -> NodeStyle {
    if is_missing {
        return NodeStyle {
            color: "#ef4444",
            shape: "box",
            border_dash: Some([5, 2]),
            label_prefix: Some("[MISSING] "),
        };
    }
    if is_dead {
        return NodeStyle {
            color: "#64748b",
            shape: "box",
            border_dash: None,
            label_prefix: Some("[DEAD] "),
        };
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    match ext.as_str() {
        "bin" | "xml" => NodeStyle {
            color: "#3b82f6",
            shape: "box",
            border_dash: None,
            label_prefix: None,
        },
        "geopcdx" | "xsec" => NodeStyle {
            color: "#10b981",
            shape: "ellipse",
            border_dash: None,
            label_prefix: None,
        },
        "tgpcdx" => NodeStyle {
            color: "#f59e0b",
            shape: "ellipse",
            border_dash: None,
            label_prefix: None,
        },
        _ => NodeStyle {
            color: "#8b5cf6",
            shape: "ellipse",
            border_dash: None,
            label_prefix: None,
        },
    }
}

/// Serialized node payload for the full directory graph.
#[derive(Serialize)]
struct FullGraphNode {
    id: usize,
    label: String,
    title: String,
    color: String,
    shape: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    level: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    border_dash: Option<[usize; 2]>,
}

/// Serialized edge payload for the full directory graph.
#[derive(Serialize)]
struct FullGraphEdge {
    from: usize,
    to: usize,
}

/// Build the standalone HTML document for the full directory graph.
fn build_full_html(
    nodes_json: &str,
    edges_json: &str,
    total_files: usize,
    active_files: usize,
    dead_files: usize,
    missing_files: usize,
) -> String
{
    let mut html = r##"<!DOCTYPE html>
<html lang="ru">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Полный граф каталога зависимостей</title>
<script src="__VIS_CDN__"></script>
<style>
    html, body { margin: 0; padding: 0; width: 100%; height: 100%; overflow: hidden; background: #0f172a; color: #e5e7eb; font-family: system-ui, -apple-system, Segoe UI, Roboto, sans-serif; }
    #network { width: 100vw; height: 100vh; background: #0f172a; display: block; }
    .controls {
        position: absolute;
        top: 12px;
        left: 12px;
        z-index: 50;
        background: rgba(15, 23, 42, 0.9);
        backdrop-filter: blur(8px);
        border: 1px solid #334155;
        border-radius: 10px;
        padding: 10px 12px;
        box-shadow: 0 8px 24px rgba(0,0,0,0.5);
        user-select: none;
        display: flex;
        flex-direction: column;
        gap: 8px;
        align-items: flex-start;
        min-width: 260px;
    }
    .controls-title { font-weight: 700; font-size: 13px; color: #fbbf24; margin: 0; }
    .badge-row { display: flex; gap: 8px; font-size: 12px; color: #cbd5e1; flex-wrap: wrap; }
    .badge { background: #1e293b; border: 1px solid #475569; border-radius: 6px; padding: 2px 8px; color: #e5e7eb; font-weight: 600; white-space: nowrap; }
    .badge .num { color: #60a5fa; }
    .search-box { width: 100%; display: flex; gap: 6px; margin-top: 4px; }
    .search-box input { flex: 1; background: #0f172a; border: 1px solid #475569; border-radius: 6px; padding: 6px 8px; color: #e5e7eb; font-size: 12px; outline: none; }
    .search-box input:focus { border-color: #3b82f6; }
    .search-box button { background: #3b82f6; border: none; border-radius: 6px; padding: 6px 10px; color: #ffffff; font-size: 12px; font-weight: 600; cursor: pointer; }
    .search-box button:hover { background: #60a5fa; }
    .filter-group { display: flex; flex-direction: column; gap: 4px; width: 100%; margin-top: 4px; }
    .filter-label { font-size: 11px; color: #94a3b8; font-weight: 600; text-transform: uppercase; letter-spacing: 0.05em; }
    .filter-item { display: flex; align-items: center; gap: 6px; font-size: 12px; color: #e5e7eb; cursor: pointer; }
    .filter-item input { accent-color: #3b82f6; width: 14px; height: 14px; }
    .physics-toggle { width: 100%; background: #10b981; border: none; border-radius: 6px; padding: 6px 10px; color: #ffffff; font-size: 12px; font-weight: 600; cursor: pointer; margin-top: 2px; }
    .physics-toggle:hover { background: #34d399; }
    .physics-toggle.paused { background: #ef4444; }
    .physics-toggle.paused:hover { background: #f87171; }
    .cluster-info { font-size: 12px; color: #94a3b8; margin-top: 2px; line-height: 1.4; }
    .hint { font-size: 11px; color: #94a3b8; margin-top: 4px; }
</style>
</head>
<body>
<div class="controls">
    <div class="controls-title">Полный граф каталога</div>
    <div class="badge-row">
        <span>Всего файлов: <span class="badge"><span class="num" id="total-count">__TOTAL__</span></span></span>
        <span>Активных: <span class="badge"><span class="num" id="active-count">__ACTIVE__</span></span></span>
        <span>Мёртвых: <span class="badge"><span class="num" id="dead-count">__DEAD__</span></span></span>
        <span>Ошибок: <span class="badge"><span class="num" id="missing-count">__MISSING__</span></span></span>
    </div>
    <div class="search-box">
        <input type="text" id="graph-search" placeholder="Поиск файла...">
        <button id="graph-center">Центр</button>
    </div>
    <div class="filter-group">
        <div class="filter-label">Фильтры:</div>
        <label class="filter-item"><input type="checkbox" id="filter-bin" checked><span>Чертежи (.bin, .xml)</span></label>
        <label class="filter-item"><input type="checkbox" id="filter-mesh" checked><span>Меши (.GeoPcDx, .XSec)</span></label>
        <label class="filter-item"><input type="checkbox" id="filter-tex" checked><span>Текстуры (.TgPcDx)</span></label>
        <label class="filter-item"><input type="checkbox" id="filter-dead"><span>Мёртвые ресурсы</span></label>
    </div>
    <button class="fit-view" id="graph-fit">Сбросить камеру</button>
    <div class="cluster-info">Все сканированные файлы каталога. Компоненты расставляются автоматически (Barnes-Hut). Наведите на узел для полного пути.</div>
    <div class="hint">Введите часть имени файла и нажмите Enter или "Центр" для камеры на узле.</div>
</div>
<div id="network"></div>
<script>
    const rawNodes = __NODES_JSON__;
    const rawEdges = __EDGES_JSON__;

    const binSet = new Set();
    const binsAll = [];
    const meshes = [];
    const textures = [];
    const dead = [];

    rawNodes.forEach(n => {
        const isDead = n.label.startsWith("[DEAD] ");
        const clean = n.label.replace(/^\\[(DEAD|MISSING)\\] /, "").toLowerCase();
        if (isDead) {
            dead.push(n);
        } else if (clean.endsWith(".bin") || clean.endsWith(".xml")) {
            binsAll.push(n);
            binSet.add(n.id);
        } else if (clean.endsWith(".geopcdx") || clean.endsWith(".xsec")) {
            meshes.push(n);
        } else {
            textures.push(n);
        }
    });

    // Build in-edge lookup maps
    const inBinEdges = new Map();
    const inEdges = new Map();
    rawEdges.forEach(e => {
        if (!inEdges.has(e.to)) inEdges.set(e.to, []);
        inEdges.get(e.to).push(e.from);

        if (binSet.has(e.from) && binSet.has(e.to)) {
            if (!inBinEdges.has(e.to)) inBinEdges.set(e.to, []);
            inBinEdges.get(e.to).push(e.from);
        }
    });

    // Split blueprints into Root (inDegreeFromBins == 0) and Sub/Child (inDegreeFromBins > 0)
    const rootBins = [];
    const subBins = [];
    binsAll.forEach(b => {
        if ((inBinEdges.get(b.id)?.length || 0) === 0) {
            rootBins.push(b);
        } else {
            subBins.push(b);
        }
    });

    // 1. Position Root Blueprints (3 columns)
    const rootCols = 3;
    rootBins.forEach((n, i) => {
        n.x = (i % rootCols) * 200;
        n.y = Math.floor(i / rootCols) * 44;
    });

    // 2. Position Sub/Child Blueprints (3 columns, sorted near parent root bin)
    subBins.sort((a, b) => (inBinEdges.get(a.id)?.[0] ?? 999999) - (inBinEdges.get(b.id)?.[0] ?? 999999));
    const subCols = 3;
    const subXOffset = 640;
    subBins.forEach((n, i) => {
        n.x = subXOffset + (i % subCols) * 200;
        n.y = Math.floor(i / subCols) * 44;
    });

    // 3. Position Meshes (3 columns, sorted near parent bin)
    meshes.sort((a, b) => (inEdges.get(a.id)?.[0] ?? 999999) - (inEdges.get(b.id)?.[0] ?? 999999));
    const meshCols = 3;
    const meshXOffset = subXOffset + subCols * 200 + 140;
    meshes.forEach((n, i) => {
        n.x = meshXOffset + (i % meshCols) * 200;
        n.y = Math.floor(i / meshCols) * 42;
    });

    // 4. Position Textures (9 columns matrix, sorted near parent mesh)
    textures.sort((a, b) => (inEdges.get(a.id)?.[0] ?? 999999) - (inEdges.get(b.id)?.[0] ?? 999999));
    const texCols = 9;
    const texXOffset = meshXOffset + meshCols * 200 + 160;
    textures.forEach((n, i) => {
        n.x = texXOffset + (i % texCols) * 190;
        n.y = Math.floor(i / texCols) * 36;
    });

    // 5. Position Dead Resources at the bottom
    const maxY = Math.max(
        (rootBins.length / rootCols) * 44,
        (subBins.length / subCols) * 44,
        (meshes.length / meshCols) * 42,
        (textures.length / texCols) * 36
    );
    dead.forEach((n, i) => {
        n.x = (i % 12) * 190;
        n.y = maxY + 220 + Math.floor(i / 12) * 35;
    });
    // maxY + 250

    rawEdges.forEach((e, i) => { e.id = i; });

    const nodes = new vis.DataSet(rawNodes);
    const edges = new vis.DataSet(rawEdges);
    const container = document.getElementById("network");
    const data = { nodes, edges };
    const options = {
        nodes: {
            borderWidth: 1.5,
            font: { color: "#e5e7eb", size: 12, strokeWidth: 0 }
        },
        edges: {
            arrows: { to: { enabled: true, scaleFactor: 0.4 } },
            smooth: {
                type: "cubicBezier",
                forceDirection: "horizontal",
                roundness: 0.35
            },
            color: { color: "rgba(100, 116, 139, 0.22)", highlight: "#38bdf8", hover: "#38bdf8" },
            width: 1.0,
            selectionWidth: 2.0
        },
        physics: false,
        interaction: {
            hover: true,
            tooltipDelay: 100,
            hideEdgesOnDrag: true,
            selectable: true,
            navigationButtons: true,
            keyboard: { enabled: true, bindToWindow: false }
        }
    };
    const network = new vis.Network(container, data, options);

    // Interactive subgraph highlighting
    network.on("click", function(params) {
        if (params.nodes.length === 0) {
            nodes.update(rawNodes.map(n => ({ id: n.id, opacity: 1 })));
            edges.update(rawEdges.map(e => ({ id: e.id, color: "rgba(100, 116, 139, 0.22)", width: 1.0 })));
            return;
        }
        const clicked = params.nodes[0];
        const connectedNodes = new Set(network.getConnectedNodes(clicked));
        connectedNodes.add(clicked);
        const connectedEdges = new Set(network.getConnectedEdges(clicked));
        nodes.update(rawNodes.map(n => ({
            id: n.id,
            opacity: connectedNodes.has(n.id) ? 1 : 0.15
        })));
        edges.update(rawEdges.map(e => {
            if (connectedEdges.has(e.id)) {
                return { id: e.id, color: "#38bdf8", width: 2.0 };
            }
            return { id: e.id, color: "rgba(100, 116, 139, 0.15)", width: 1.0 };
        }));
    });

    const searchInput = document.getElementById("graph-search");
    const centerBtn = document.getElementById("graph-center");
    const fitBtn = document.getElementById("graph-fit");
    const filterBin = document.getElementById("filter-bin");
    const filterMesh = document.getElementById("filter-mesh");
    const filterTex = document.getElementById("filter-tex");
    const filterDead = document.getElementById("filter-dead");

    function extOf(label) {
        const parts = label.split(".");
        return parts.length > 1 ? parts[parts.length - 1].toLowerCase() : "";
    }

    function isMissing(label) { return label.startsWith("[MISSING] "); }
    function isDead(label) { return label.startsWith("[DEAD] "); }

    function applyFilters() {
        const showBin = filterBin.checked;
        const showMesh = filterMesh.checked;
        const showTex = filterTex.checked;
        const showDead = filterDead.checked;

        const updates = [];
        nodes.forEach(n => {
            const label = n.label;
            const missing = isMissing(label);
            const dead = isDead(label);
            const e = extOf(label.replace(/^\[(MISSING|DEAD)\] /, ""));

            let visible = true;
            if (dead) {
                visible = showDead;
            } else if (missing) {
                visible = true; // Missing references remain visible for diagnostic context
            } else {
                if (e === "bin" || e === "xml") visible = showBin;
                else if (e === "geopcdx" || e === "xsec") visible = showMesh;
                else if (e === "tgpcdx") visible = showTex;
                else visible = true;
            }

            updates.push({ id: n.id, hidden: !visible });
        });
        nodes.update(updates);
    }

    // Ensure filters apply on page load
    applyFilters();

    let searchMatches = [];
    let currentMatchIdx = 0;
    let lastSearchQuery = "";

    function norm(str) {
        return (str || "").toLowerCase().replace(/\\\\/g, "/");
    }

    function centerOnNode(query) {
        const q = norm(query.trim());
        if (!q) return;

        if (q !== lastSearchQuery) {
            lastSearchQuery = q;
            searchMatches = nodes.get().filter(n => {
                const label = norm(n.label);
                const title = norm(n.title);
                return label.includes(q) || title.includes(q);
            });
            currentMatchIdx = 0;
        } else {
            currentMatchIdx = (currentMatchIdx + 1) % (searchMatches.length || 1);
        }

        if (searchMatches.length === 0) {
            searchInput.style.borderColor = "#ef4444";
            setTimeout(() => { searchInput.style.borderColor = "#475569"; }, 1500);
            return;
        }

        const target = searchMatches[currentMatchIdx];
        network.focus(target.id, { scale: 1.4, animation: { duration: 600 } });
        network.selectNodes([target.id]);
    }

    centerBtn.addEventListener("click", () => centerOnNode(searchInput.value));
    searchInput.addEventListener("keydown", (e) => {
        e.stopPropagation();
        if (e.key === "Enter") centerOnNode(searchInput.value);
    });
    searchInput.addEventListener("keyup", (e) => e.stopPropagation());

    fitBtn.addEventListener("click", () => {
        network.fit({ animation: { duration: 400 } });
    });

    [filterBin, filterMesh, filterTex, filterDead].forEach(cb => {
        cb.addEventListener("change", applyFilters);
    });
</script>
</body>
</html>"##
    .to_string();

    html = html.replace("__VIS_CDN__", VIS_NETWORK_CDN);
    html = html.replace("__TOTAL__", &total_files.to_string());
    html = html.replace("__ACTIVE__", &active_files.to_string());
    html = html.replace("__DEAD__", &dead_files.to_string());
    html = html.replace("__MISSING__", &missing_files.to_string());
    html = html.replace("__NODES_JSON__", nodes_json);
    html = html.replace("__EDGES_JSON__", edges_json);
    html
}

/// Generate a full directory asset dependency graph containing ALL scanned files
/// (healthy, orphan, and broken) and open it in the default browser.
pub fn generate_full_directory_graph(
    graph: &crate::auditor::FullAssetGraph,
    _target_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>>
{
    // 1. Assign indices and build nodes.
    let mut path_to_idx: std::collections::HashMap<PathBuf, usize> = std::collections::HashMap::new();
    let mut nodes: Vec<FullGraphNode> = Vec::new();

    for path in &graph.files_on_disk {
        let idx = nodes.len();
        path_to_idx.insert(path.clone(), idx);

        let is_dead = graph.dead_files.contains(path);
        let is_missing = graph.missing_files.contains(path);
        let style = full_graph_node_style(path, is_dead, is_missing);

        let level = if is_dead {
            Some(3usize)
        } else {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();
            match ext.as_str() {
                "bin" | "xml" => Some(0usize),
                "geopcdx" | "xsec" => Some(1usize),
                "tgpcdx" => Some(2usize),
                _ => None,
            }
        };

        let label = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string());
        let label = match style.label_prefix {
            Some(prefix) => format!("{prefix}{label}"),
            None => label,
        };
        let title = crate::fs_utils::to_assets_relative_display(path);

        nodes.push(FullGraphNode {
            id: idx,
            label,
            title,
            color: style.color.to_string(),
            shape: style.shape,
            level,
            border_dash: style.border_dash,
        });
    }

    // 2. Add missing-file nodes (not on disk).
    for path in &graph.missing_files {
        if path_to_idx.contains_key(path) {
            continue;
        }
        let idx = nodes.len();
        path_to_idx.insert(path.clone(), idx);

        let style = full_graph_node_style(path, false, true);
        let label = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string());
        let label = format!("[MISSING] {label}");
        let title = crate::fs_utils::to_assets_relative_display(path);

        nodes.push(FullGraphNode {
            id: idx,
            label,
            title,
            color: style.color.to_string(),
            shape: style.shape,
            level: Some(2usize),
            border_dash: style.border_dash,
        });
    }

    // 3. Build edges from forward_edges.
    let mut edges: Vec<FullGraphEdge> = Vec::new();
    for (src, dsts) in &graph.forward_edges {
        if let Some(&src_idx) = path_to_idx.get(src) {
            for dst in dsts {
                if let Some(&dst_idx) = path_to_idx.get(dst) {
                    edges.push(FullGraphEdge { from: src_idx, to: dst_idx });
                }
            }
        }
    }

    // 4. Deduplicate edges.
    let mut seen_edges: std::collections::HashSet<(usize, usize)> = std::collections::HashSet::new();
    edges.retain(|e| seen_edges.insert((e.from, e.to)));

    let total_files = graph.files_on_disk.len();
    let dead_count = graph.dead_files.len();
    let missing_count = graph.missing_files.len();
    let active_files = total_files.saturating_sub(dead_count);

    let _edge_count = edges.len();

    let nodes_json = serde_json::to_string(&nodes).expect("nodes serialize");
    let edges_json = serde_json::to_string(&edges).expect("edges serialize");
    let html = build_full_html(&nodes_json, &edges_json, total_files, active_files, dead_count, missing_count);

    let out_path = std::env::temp_dir().join(FULL_GRAPH_HTML_NAME);
    std::fs::write(&out_path, html)?;
    open::that(&out_path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_html_contains_cdn_and_colors() {
        let html = build_html(
            Path::new("Assets\\Kuju\\Product\\signal_metal.TgPcDx"),
            &[],
            &[],
            &[],
        );
        assert!(html.contains(VIS_NETWORK_CDN));
        assert!(html.contains("#3b82f6"));
        assert!(html.contains("#10b981"));
        assert!(html.contains("#f59e0b"));
        assert!(html.contains("#ef4444"));
        assert!(html.contains("forceAtlas2Based"));
        assert!(html.contains("damping"));
    }

    #[test]
    fn missing_node_gets_prefix_and_dash() {
        let html = build_html(
            Path::new("Assets\\Kuju\\Product\\center.bin"),
            &[],
            &[],
            &[PathBuf::from("Assets\\Kuju\\Product\\missing.TgPcDx")],
        );
        assert!(html.contains("[MISSING] missing.TgPcDx"));
        assert!(html.contains("missing.TgPcDx"));
    }

    #[test]
    fn global_graph_uses_barneshut_and_dedupes() {
        let html = build_global_html(
            r##"[{"id":0,"label":"source.bin","title":"Assets\\Kuju\\Product\\source.bin","color":"#3b82f6","shape":"box"}]"##,
            "[]",
            1,
            0,
        );
        assert!(html.contains(VIS_NETWORK_CDN));
        assert!(html.contains("barnesHut"));
        assert!(html.contains("-4000"));
        assert!(html.contains("centralGravity"));
        assert!(html.contains("springLength"));
        assert!(html.contains("avoidOverlap"));
        assert!(html.contains("Поиск узла"));
        assert!(html.contains("Приостановить физику"));
    }

    #[test]
    fn node_style_missing_gets_red_and_dash() {
        let style = node_style(
            Path::new("Assets\\Kuju\\Product\\missing.TgPcDx"),
            true,
            true,
            Some(crate::auditor::AssetResourceKind::Texture),
        );
        assert_eq!(style.color, "#ef4444");
        assert_eq!(style.shape, "box");
        assert_eq!(style.border_dash, Some([5, 2]));
        assert_eq!(style.label_prefix, Some("[MISSING] "));
    }

    #[test]
    fn node_style_bin_gets_blue_box() {
        let style = node_style(
            Path::new("Assets\\Kuju\\Product\\source.bin"),
            true,
            false,
            Some(crate::auditor::AssetResourceKind::ChildBlueprint),
        );
        assert_eq!(style.color, "#3b82f6");
        assert_eq!(style.shape, "box");
    }

    #[test]
    fn node_style_unused_gets_gray() {
        let style = node_style(
            Path::new("Assets\\Kuju\\Product\\orphan.TgPcDx"),
            false,
            false,
            Some(crate::auditor::AssetResourceKind::Unused),
        );
        assert_eq!(style.color, "#6b7280");
    }
}
