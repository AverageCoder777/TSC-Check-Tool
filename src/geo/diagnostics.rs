//! Diagnostic logging for geometry parsing.

use std::collections::HashMap;
use std::fmt::{self, Write as _};
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::Path;

use super::parse::{IndexBuffer, MaterialBinding, RawSubmesh, VertexBuffer};
use super::types::GeoParseDiagnostics;

/// Appends a single timestamped line to the application log file.
fn append_log_line(args: fmt::Arguments<'_>, report_error: bool) {
    let log_path = crate::app::settings::log_file();
    let result = (|| {
        if let Some(parent) = log_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;
        writeln!(
            file,
            "{} {}",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
            args
        )
    })();
    if report_error && let Err(error) = result {
        eprintln!(
            "Failed to write GeoPcDx diagnostics to {}: {error}",
            log_path.display()
        );
    }
}

/// Best-effort warning channel for recoverable per-buffer/per-batch parse failures.
/// A single broken buffer must not abort the whole model, so the reason is logged and
/// only the offending buffer or batch is skipped.
pub(crate) fn warn_parse_skip(message: impl fmt::Display) {
    eprintln!("[GeoPcDx] {message}");
    append_log_line(format_args!("[GeoPcDx] {message}"), false);
}

/// Diagnostic log for every material binding resolved during parsing.
pub(crate) fn log_material_diagnostics(path: &Path, bindings: &HashMap<u32, MaterialBinding>) {
    let mut entries: Vec<_> = bindings.iter().collect();
    entries.sort_unstable_by_key(|&(&id, _)| id);

    let mut message = String::with_capacity(64 + entries.len() * 128);
    let _ = write!(message, "[GeoPcDx] {}: materials=[", path.display());
    for (i, (id, binding)) in entries.into_iter().enumerate() {
        if i > 0 {
            message.push_str("; ");
        }
        let _ = write!(
            message,
            "material id={} index={} name={:?} shader={:?} mode={:?} alpha_cutoff={} texture_index={} excluded={}",
            id,
            binding.material_index,
            binding.material_name,
            binding.shader_name,
            binding.render_mode,
            binding.alpha_ref,
            binding.texture_index,
            binding.excluded,
        );
    }
    message.push(']');

    eprintln!("{message}");
    append_log_line(format_args!("{message}"), false);
}

/// Diagnostic log for every vertex and index buffer found during parsing.
pub(crate) fn log_vertex_buffer_diagnostics(
    path: &Path,
    vertex_data: &[VertexBuffer],
    index_data: &[IndexBuffer],
) {
    let mut message =
        String::with_capacity(64 + vertex_data.len() * 96 + index_data.len() * 48);
    let _ = write!(message, "[GeoPcDx] {}: vertex_buffers=[", path.display());
    for (i, buffer) in vertex_data.iter().enumerate() {
        if i > 0 {
            message.push_str("; ");
        }
        let _ = write!(
            message,
            "vb[{}]: num_vertices={} stride={} pos_offset={} pos_format={:?} norm_offset={:?} uv_offset={:?}",
            buffer.source_index,
            buffer.bytes.len().checked_div(buffer.stride).unwrap_or(0),
            buffer.stride,
            buffer.position_offset,
            buffer.position_format,
            buffer.normal_offset,
            buffer.uv_offset,
        );
    }
    message.push_str("]; index_buffers=[");
    for (i, buffer) in index_data.iter().enumerate() {
        if i > 0 {
            message.push_str("; ");
        }
        let _ = write!(
            message,
            "ib[{}]: indices={} type={}",
            buffer.source_index,
            buffer.indices.len(),
            buffer.element_type,
        );
    }
    message.push(']');

    eprintln!("{message}");
    append_log_line(format_args!("{message}"), false);
}

/// Diagnostic log for every raw batch before submesh conversion.
pub(crate) fn log_batch_diagnostics(path: &Path, batches: &[RawSubmesh]) {
    let mut message = String::with_capacity(64 + batches.len() * 144);
    let _ = write!(message, "[GeoPcDx] {}: batches=[", path.display());
    for (i, batch) in batches.iter().enumerate() {
        if i > 0 {
            message.push_str("; ");
        }
        let _ = write!(
            message,
            "batch node={:?} det={:.4} prim={} num_prim={} index_start={} vertex_base={} vbuf_idx={} ibuf_idx={} mode={:?} material_name={:?}",
            batch.node_name,
            batch.transform.determinant(),
            batch.topology,
            batch.index_count,
            batch.first_index,
            batch.base_vertex,
            batch.vertex_buffer_index,
            batch.index_buffer_index,
            batch.render_mode,
            batch.material_name,
        );
    }
    message.push(']');

    eprintln!("{message}");
    append_log_line(format_args!("{message}"), false);
}

/// Diagnostic log for every model load.
pub(crate) fn log_parse_diagnostics(path: &Path, diagnostics: &GeoParseDiagnostics) {
    let mut message = String::with_capacity(256 + diagnostics.batch_ranges.len() * 32);
    let _ = write!(
        message,
        "[GeoPcDx] {}: vertex_stride={} vertex_count={} declared_vertex_count={} fvf=0x{:X} submeshes={} local_transforms={} vertex_layout=[{}] index_type={} raw_index_count={} index_layout=[{}] primitive_type={} batches=[",
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
    );
    if diagnostics.batch_ranges.is_empty() {
        message.push_str("none");
    } else {
        for (i, range) in diagnostics.batch_ranges.iter().enumerate() {
            if i > 0 {
                message.push_str("; ");
            }
            message.push_str(range);
        }
    }
    message.push(']');

    append_log_line(format_args!("{message}"), true);
}
