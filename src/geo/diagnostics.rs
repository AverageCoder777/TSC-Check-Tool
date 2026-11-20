//! Diagnostic logging for geometry parsing.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

use super::parse::{IndexBuffer, MaterialBinding, RawSubmesh, VertexBuffer};
use super::types::GeoParseDiagnostics;

/// Best-effort warning channel for recoverable per-buffer/per-batch parse failures.
/// A single broken buffer must not abort the whole model, so the reason is logged and
/// only the offending buffer or batch is skipped.
pub(crate) fn warn_parse_skip(message: String) {
    eprintln!("[GeoPcDx] {message}");
    let log_path = crate::app::settings::log_file();
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        let _ = writeln!(
            file,
            "{} [GeoPcDx] {}",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
            message
        );
    }
}

/// Diagnostic log for every material binding resolved during parsing.
pub(crate) fn log_material_diagnostics(path: &Path, bindings: &HashMap<u32, MaterialBinding>) {
    use std::io::Write;

    let mut entries: Vec<_> = bindings.iter().collect();
    entries.sort_by_key(|(id, _)| *id);
    let mut lines = Vec::new();
    for (id, binding) in entries {
        lines.push(format!(
            "material id={} index={} name={:?} shader={:?} mode={:?} alpha_cutoff={} texture_index={} excluded={}",
            id,
            binding.material_index,
            binding.material_name,
            binding.shader_name,
            binding.render_mode,
            binding.alpha_ref,
            binding.texture_index,
            binding.excluded,
        ));
    }
    let message = format!(
        "[GeoPcDx] {}: materials=[{}]",
        path.display(),
        lines.join("; ")
    );
    eprintln!("{message}");
    let log_path = crate::app::settings::log_file();
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        let _ = writeln!(
            file,
            "{} {}",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
            message
        );
    }
}

/// Diagnostic log for every vertex and index buffer found during parsing.
pub(crate) fn log_vertex_buffer_diagnostics(
    path: &Path,
    vertex_data: &[VertexBuffer],
    index_data: &[IndexBuffer],
) {
    use std::io::Write;

    let mut vb_lines = Vec::new();
    for buffer in vertex_data {
        vb_lines.push(format!(
            "vb[{}]: num_vertices={} stride={} pos_offset={} pos_format={:?} norm_offset={:?} uv_offset={:?}",
            buffer.source_index,
            buffer.bytes.len().checked_div(buffer.stride).unwrap_or(0),
            buffer.stride,
            buffer.position_offset,
            buffer.position_format,
            buffer.normal_offset,
            buffer.uv_offset,
        ));
    }
    let mut ib_lines = Vec::new();
    for buffer in index_data {
        ib_lines.push(format!(
            "ib[{}]: indices={} type={}",
            buffer.source_index,
            buffer.indices.len(),
            buffer.element_type,
        ));
    }
    let message = format!(
        "[GeoPcDx] {}: vertex_buffers=[{}]; index_buffers=[{}]",
        path.display(),
        vb_lines.join("; "),
        ib_lines.join("; ")
    );
    eprintln!("{message}");
    let log_path = crate::app::settings::log_file();
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        let _ = writeln!(
            file,
            "{} {}",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
            message
        );
    }
}

/// Diagnostic log for every raw batch before submesh conversion.
pub(crate) fn log_batch_diagnostics(path: &Path, batches: &[RawSubmesh]) {
    use std::io::Write;

    let mut lines = Vec::new();
    for batch in batches {
        let det = batch.transform.determinant();
        lines.push(format!(
            "batch node={:?} det={:.4} prim={} num_prim={} index_start={} vertex_base={} vbuf_idx={} ibuf_idx={} mode={:?} material_name={:?}",
            batch.node_name,
            det,
            batch.topology,
            batch.index_count,
            batch.first_index,
            batch.base_vertex,
            batch.vertex_buffer_index,
            batch.index_buffer_index,
            batch.render_mode,
            batch.material_name,
        ));
    }
    let message = format!(
        "[GeoPcDx] {}: batches=[{}]",
        path.display(),
        lines.join("; ")
    );
    eprintln!("{message}");
    let log_path = crate::app::settings::log_file();
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        let _ = writeln!(
            file,
            "{} {}",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
            message
        );
    }
}

/// Diagnostic log for every model load.
pub(crate) fn log_parse_diagnostics(path: &Path, diagnostics: &GeoParseDiagnostics) {
    use std::io::Write;

    let ranges = if diagnostics.batch_ranges.is_empty() {
        "none".to_owned()
    } else {
        diagnostics.batch_ranges.join("; ")
    };
    let message = format!(
        "[GeoPcDx] {}: vertex_stride={} vertex_count={} declared_vertex_count={} fvf=0x{:X} submeshes={} local_transforms={} vertex_layout=[{}] index_type={} raw_index_count={} index_layout=[{}] primitive_type={} batches=[{}]",
        path.display(),
        diagnostics.vertex_stride,
        diagnostics.vertex_count,
        diagnostics.declared_vertex_count,
        diagnostics.fvf,
        diagnostics.submesh_count,
        diagnostics.local_transform_count,
        diagnostics.vertex_layout,
        diagnostics.index_element_type,
        diagnostics.raw_index_count,
        diagnostics.index_layout,
        diagnostics.primitive_type,
        ranges,
    );
    let log_path = crate::app::settings::log_file();
    let result = (|| {
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;
        writeln!(
            file,
            "{} {}",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
            message
        )
    })();
    if let Err(error) = result {
        eprintln!(
            "Failed to write GeoPcDx diagnostics to {}: {error}",
            log_path.display()
        );
    }
}
