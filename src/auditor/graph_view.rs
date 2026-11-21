use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::common::fs_utils::{normalize_path, to_assets_relative_display};

const VIS_NETWORK_CDN: &str = "https://unpkg.com/vis-network/standalone/umd/vis-network.min.js";
const GRAPH_HTML_NAME: &str = "tsc_graph_view.html";
const DEPENDENCY_GRAPH_TEMPLATE: &str = include_str!("../templates/dependency_graph.html");
const FULL_GRAPH_TEMPLATE: &str = include_str!("../templates/full_graph.html");
const FULL_GRAPH_HTML_NAME: &str = "tsc_full_graph.html";
const TOOLTIP_COLOR: &str = "#e5e7eb";

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
    color: &'static str,
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
    color: &'static str,
}

#[derive(Serialize)]
struct GraphEdge {
    from: usize,
    to: usize,
    arrows: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    dashes: Option<[usize; 2]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    color: Option<&'static str>,
}

fn format_node_label(path: &Path, prefix: Option<&str>) -> String {
    let base = path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_else(|| path.to_string_lossy());
    match prefix {
        Some(prefix) => format!("{prefix}{base}"),
        None => base.into_owned(),
    }
}

fn add_dependency_node(
    path: &Path,
    kind: NodeKind,
    nodes: &mut Vec<GraphNode>,
    index_of: &mut HashMap<PathBuf, usize>,
) -> usize {
    let norm = normalize_path(path);
    if let Some(&id) = index_of.get(&norm) {
        return id;
    }
    let id = nodes.len();
    let label = format_node_label(&norm, kind.label_prefix());
    let title = to_assets_relative_display(&norm);
    index_of.insert(norm, id);

    let (border_width, border_dash) = match kind {
        NodeKind::Center => (Some(3usize), None),
        NodeKind::Missing => (None, Some([5usize, 2usize])),
        _ => (None, None),
    };
    nodes.push(GraphNode {
        id,
        label,
        title,
        color: kind.color(),
        shape: kind.shape(),
        font: GraphFont {
            size: 14,
            color: TOOLTIP_COLOR,
        },
        border_width,
        border_dash,
    });
    id
}

fn inject_json_payloads(template: &str, nodes_json: &str, edges_json: &str) -> String {
    let mut out = String::with_capacity(template.len() + nodes_json.len() + edges_json.len());
    let mut rest = template;
    while let Some(pos) = rest.find("__") {
        out.push_str(&rest[..pos]);
        let after = &rest[pos..];
        if let Some(tail) = after.strip_prefix("__NODES_JSON__") {
            out.push_str(nodes_json);
            rest = tail;
        } else if let Some(tail) = after.strip_prefix("__EDGES_JSON__") {
            out.push_str(edges_json);
            rest = tail;
        } else {
            out.push_str("__");
            rest = &after[2..];
        }
    }
    out.push_str(rest);
    out
}

fn build_html(
    selected_path: &Path,
    inbound_refs: &[PathBuf],
    outbound_refs: &[PathBuf],
    missing_refs: &[PathBuf],
) -> String {
    let edge_cap = inbound_refs.len() + outbound_refs.len() + missing_refs.len();
    let mut nodes: Vec<GraphNode> = Vec::with_capacity(edge_cap + 1);
    let mut edges: Vec<GraphEdge> = Vec::with_capacity(edge_cap);
    let mut index_of: HashMap<PathBuf, usize> = HashMap::with_capacity(edge_cap + 1);

    let center_id = add_dependency_node(selected_path, NodeKind::Center, &mut nodes, &mut index_of);
    for path in inbound_refs {
        let id = add_dependency_node(path, NodeKind::Inbound, &mut nodes, &mut index_of);
        edges.push(GraphEdge {
            from: id,
            to: center_id,
            arrows: "to",
            dashes: None,
            color: None,
        });
    }
    for path in outbound_refs {
        let id = add_dependency_node(path, NodeKind::Outbound, &mut nodes, &mut index_of);
        edges.push(GraphEdge {
            from: center_id,
            to: id,
            arrows: "to",
            dashes: None,
            color: None,
        });
    }
    for path in missing_refs {
        let id = add_dependency_node(path, NodeKind::Missing, &mut nodes, &mut index_of);
        edges.push(GraphEdge {
            from: center_id,
            to: id,
            arrows: "to",
            dashes: Some([4usize, 2usize]),
            color: Some(NodeKind::Missing.color()),
        });
    }

    let nodes_json = serde_json::to_string(&nodes).expect("nodes serialize");
    let edges_json = serde_json::to_string(&edges).expect("edges serialize");
    let html = DEPENDENCY_GRAPH_TEMPLATE
        .replace("__VIS_CDN__", VIS_NETWORK_CDN)
        .replace("__CENTER_COLOR__", NodeKind::Center.color())
        .replace("__INBOUND_COLOR__", NodeKind::Inbound.color())
        .replace("__OUTBOUND_COLOR__", NodeKind::Outbound.color())
        .replace("__MISSING_COLOR__", NodeKind::Missing.color())
        .replace("__TOOLTIP_COLOR__", TOOLTIP_COLOR);

    inject_json_payloads(&html, &nodes_json, &edges_json)
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
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if ext.eq_ignore_ascii_case("bin") || ext.eq_ignore_ascii_case("xml") {
        NodeStyle {
            color: "#3b82f6",
            shape: "box",
            border_dash: None,
            label_prefix: None,
        }
    } else if ext.eq_ignore_ascii_case("geopcdx") || ext.eq_ignore_ascii_case("xsec") {
        NodeStyle {
            color: "#10b981",
            shape: "ellipse",
            border_dash: None,
            label_prefix: None,
        }
    } else if ext.eq_ignore_ascii_case("tgpcdx") {
        NodeStyle {
            color: "#f59e0b",
            shape: "ellipse",
            border_dash: None,
            label_prefix: None,
        }
    } else {
        NodeStyle {
            color: "#8b5cf6",
            shape: "ellipse",
            border_dash: None,
            label_prefix: None,
        }
    }
}

fn full_graph_node_level(path: &Path, is_dead: bool) -> Option<usize> {
    if is_dead {
        return Some(3);
    }
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if ext.eq_ignore_ascii_case("bin") || ext.eq_ignore_ascii_case("xml") {
        Some(0)
    } else if ext.eq_ignore_ascii_case("geopcdx") || ext.eq_ignore_ascii_case("xsec") {
        Some(1)
    } else if ext.eq_ignore_ascii_case("tgpcdx") {
        Some(2)
    } else {
        None
    }
}

#[derive(Serialize)]
struct FullGraphNode {
    id: usize,
    label: String,
    title: String,
    color: &'static str,
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
    color: Option<&'static str>,
}

fn build_full_html(
    nodes_json: &str,
    edges_json: &str,
    total_files: usize,
    active_files: usize,
    dead_files: usize,
    missing_files: usize,
) -> String {
    let html = FULL_GRAPH_TEMPLATE
        .replace("__VIS_CDN__", VIS_NETWORK_CDN)
        .replace("__TOTAL__", &total_files.to_string())
        .replace("__ACTIVE__", &active_files.to_string())
        .replace("__DEAD__", &dead_files.to_string())
        .replace("__MISSING__", &missing_files.to_string());

    inject_json_payloads(&html, nodes_json, edges_json)
}

/// Generate a full directory asset dependency graph containing ALL scanned files
/// (healthy, orphan, and broken) and open it in the default browser.
pub fn generate_full_directory_graph(
    graph: &crate::auditor::FullAssetGraph,
    _target_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let node_cap = graph.files_on_disk.len() + graph.missing_files.len();
    let mut path_to_idx: HashMap<&Path, usize> = HashMap::with_capacity(node_cap);
    let mut nodes: Vec<FullGraphNode> = Vec::with_capacity(node_cap);

    for path in &graph.files_on_disk {
        let idx = nodes.len();
        path_to_idx.insert(path.as_path(), idx);
        let is_dead = graph.dead_files.contains(path);
        let is_missing = graph.missing_files.contains(path);
        let style = full_graph_node_style(path, is_dead, is_missing);

        nodes.push(FullGraphNode {
            id: idx,
            label: format_node_label(path, style.label_prefix),
            title: to_assets_relative_display(path),
            color: style.color,
            shape: style.shape,
            level: full_graph_node_level(path, is_dead),
            border_dash: style.border_dash,
        });
    }

    for path in &graph.missing_files {
        if path_to_idx.contains_key(path.as_path()) {
            continue;
        }
        let idx = nodes.len();
        path_to_idx.insert(path.as_path(), idx);
        let style = full_graph_node_style(path, false, true);

        nodes.push(FullGraphNode {
            id: idx,
            label: format_node_label(path, style.label_prefix),
            title: to_assets_relative_display(path),
            color: style.color,
            shape: style.shape,
            level: Some(2),
            border_dash: style.border_dash,
        });
    }

    let edge_cap: usize = graph.forward_edges.values().map(HashSet::len).sum();
    let mut edges: Vec<FullGraphEdge> = Vec::with_capacity(edge_cap);
    let mut seen_edges: HashSet<(usize, usize)> = HashSet::with_capacity(edge_cap);

    for (src, dsts) in &graph.forward_edges {
        let Some(&src_idx) = path_to_idx.get(src.as_path()) else {
            continue;
        };
        for dst in dsts {
            let Some(&dst_idx) = path_to_idx.get(dst.as_path()) else {
                continue;
            };
            if !seen_edges.insert((src_idx, dst_idx)) {
                continue;
            }
            let is_missing = graph.missing_files.contains(dst);
            edges.push(FullGraphEdge {
                from: src_idx,
                to: dst_idx,
                dashes: is_missing.then_some([4usize, 2usize]),
                color: Some(if is_missing { "#ef4444" } else { "#6b7280" }),
            });
        }
    }

    let total_files = graph.files_on_disk.len();
    let dead_count = graph.dead_files.len();
    let missing_count = graph.missing_files.len();
    let active_files = total_files.saturating_sub(dead_count);

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
#[path = "../tests/graph_view.rs"]
mod tests;
