//! Self-contained dependency-graph viewer.
//!
//! Extracts the local dependency subgraph around a selected asset, compiles a
//! self-contained HTML page with an interactive `vis-network` physics graph,
//! saves it to the system temp directory and opens it in the default browser
//! via the `open` crate.

use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const VIS_NETWORK_CDN: &str =
    "https://unpkg.com/vis-network/standalone/umd/vis-network.min.js";

const GRAPH_HTML_NAME: &str = "tsc_graph_view.html";

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
}
