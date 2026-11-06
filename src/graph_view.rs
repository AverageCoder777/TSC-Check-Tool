use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const VIS_NETWORK_CDN: &str = "https://unpkg.com/vis-network/standalone/umd/vis-network.min.js";
const GRAPH_HTML_NAME: &str = "tsc_graph_view.html";
const DEPENDENCY_GRAPH_TEMPLATE: &str = include_str!("templates/dependency_graph.html");
const FULL_GRAPH_TEMPLATE: &str = include_str!("templates/full_graph.html");
const FULL_GRAPH_HTML_NAME: &str = "tsc_full_graph.html";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeKind {
    Center,
    Inbound,
    Outbound,
    Missing,
}

impl NodeKind {
    fn color(self) -> &'static str {
        match self {
            NodeKind::Center => "#3b82f6",   // blue
            NodeKind::Inbound => "#10b981",  // green
            NodeKind::Outbound => "#f59e0b", // amber
            NodeKind::Missing => "#ef4444",  // red
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
            NodeKind::Missing => Some("[Missing] "),
            _ => None,
        }
    }
}

#[derive(Serialize)]
struct GraphNode {
    id: usize,
    label: String,
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

fn build_html(
    selected_path: &Path,
    inbound_refs: &[PathBuf],
    outbound_refs: &[PathBuf],
    missing_refs: &[PathBuf],
) -> String {
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
                    font: GraphFont {
                        size: 14,
                        color: tooltip_color.to_string(),
                    },
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
        edges.push(GraphEdge {
            from: id,
            to: center_id,
            arrows: "to",
            dashes: None,
            color: None,
        });
    }
    for path in outbound_refs {
        let id = add_node!(path, NodeKind::Outbound);
        edges.push(GraphEdge {
            from: center_id,
            to: id,
            arrows: "to",
            dashes: None,
            color: None,
        });
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
    let mut html = DEPENDENCY_GRAPH_TEMPLATE.to_string();

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

pub fn open_dependency_graph_in_browser(
    selected_path: &Path,
    inbound_refs: &[PathBuf],
    outbound_refs: &[PathBuf],
    missing_refs: &[PathBuf],
) -> Result<(), Box<dyn std::error::Error>> {
    let html = build_html(selected_path, inbound_refs, outbound_refs, missing_refs);
    let out_path = std::env::temp_dir().join(GRAPH_HTML_NAME);
    std::fs::write(&out_path, html)?;
    open::that(&out_path)?;
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct NodeStyle {
    color: &'static str,
    shape: &'static str,
    border_dash: Option<[usize; 2]>,
    label_prefix: Option<&'static str>,
}

fn full_graph_node_style(path: &Path, is_dead: bool, is_missing: bool) -> NodeStyle {
    if is_missing {
        return NodeStyle {
            color: "#ef4444",
            shape: "box",
            border_dash: Some([5, 2]),
            label_prefix: Some("[Missing] "),
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

#[derive(Serialize)]
struct FullGraphEdge {
    from: usize,
    to: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    dashes: Option<[usize; 2]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    color: Option<String>,
}

fn build_full_html(
    nodes_json: &str,
    edges_json: &str,
    total_files: usize,
    active_files: usize,
    dead_files: usize,
    missing_files: usize,
) -> String {
    let mut html = FULL_GRAPH_TEMPLATE.to_string();
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
) -> Result<(), Box<dyn std::error::Error>> {
    // 1. Assign indices and build nodes.
    let mut path_to_idx: std::collections::HashMap<PathBuf, usize> =
        std::collections::HashMap::new();
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
        let label = format!("[Missing] {label}");
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
    let mut edges: Vec<FullGraphEdge> = Vec::new();
    for (src, dsts) in &graph.forward_edges {
        if let Some(&src_idx) = path_to_idx.get(src) {
            for dst in dsts {
                if let Some(&dst_idx) = path_to_idx.get(dst) {
                    let is_missing = graph.missing_files.contains(dst);
                    edges.push(FullGraphEdge {
                        from: src_idx,
                        to: dst_idx,
                        dashes: if is_missing { Some([4usize, 2usize]) } else { None },
                        color: Some(if is_missing {
                            "#ef4444".to_string()
                        } else {
                            "#6b7280".to_string()
                        }),
                    });
                }
            }
        }
    }
    let mut seen_edges: std::collections::HashSet<(usize, usize)> =
        std::collections::HashSet::new();
    edges.retain(|e| seen_edges.insert((e.from, e.to)));
    let total_files = graph.files_on_disk.len();
    let dead_count = graph.dead_files.len();
    let missing_count = graph.missing_files.len();
    let active_files = total_files.saturating_sub(dead_count);
    let _edge_count = edges.len();
    let nodes_json = serde_json::to_string(&nodes).expect("nodes serialize");
    let edges_json = serde_json::to_string(&edges).expect("edges serialize");
    let html = build_full_html(
        &nodes_json,
        &edges_json,
        total_files,
        active_files,
        dead_count,
        missing_count,
    );
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
        assert!(html.contains("[Missing] missing.TgPcDx"));
        assert!(html.contains("missing.TgPcDx"));
    }
}
