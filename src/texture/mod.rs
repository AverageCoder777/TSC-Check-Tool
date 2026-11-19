//! TgPcDx texture decoding, DDS export/import, and metadata inspection.
//!
//! This module is split into several submodules:
//! - `format` — format handling and mip size calculations
//! - `decode` — decoding TgPcDx to RGBA and UI conversion
//! - `dds` — DDS import/export, mip chain validation, SERZ tree modification
//! - `inspect` — metadata extraction, inspection, and internal name editing
//! - `serz_parse` — common SERZ/XML parsing helpers

pub mod decode;
pub mod dds;
pub mod format;
pub mod inspect;
pub mod serz_parse;

// Re-export all public items for backward compatibility
pub use decode::{
    decode_tgpcdx_for_viewer, decode_tgpcdx_to_rgba, decode_tgpcdx_with_max_dimension,
    rgba_to_slint_image,
};

pub use dds::{
    convert_dds_to_tgpcdx, convert_dds_to_tgpcdx_report, export_tgpcdx_bytes_to_dds,
    export_tgpcdx_to_dds,
};

pub use format::{
    dds_compression_of, dds_format_to_tgpcdx, expected_mip_size, format_label_and_dds_name,
    mip_level_bytes, tgpcdx_dds_compression, tgpcdx_format_to_d3d, tgpcdx_format_to_dds_name,
    DdsCompression, ImportReport, MipChainInfo, validate_dds_mip_chain,
};

pub use inspect::{
    append_tgpcdx_log, inspect_tgpcdx, inspect_tgpcdx_file, inspect_tgpcdx_with_path,
    inspect_texture, inspect_texture_file, texture_info_text, update_internal_name_in_node,
    update_tgpcdx_internal_name, update_tgpcdx_internal_name_bytes, write_output_bytes,
    TextureInfo, TextureInspectReport, TgpcdxInfo,
};

pub use serz_parse::{
    all_image_blocks, collect_serz_image_mips, count_c_hc_textures, extract_blob,
    extract_blob_size, extract_format, extract_tag, extract_unnamed_u16, find_all_elements,
    find_element_mut, find_texture_dimensions, hex_decode, image_dimensions, image_mips_from_bytes,
    parse_bool_tag, serz_bytes_to_metadata_xml, serz_bytes_to_xml, set_blob_data,
    set_or_insert_property, set_property_value, ImageMip,
};

#[cfg(test)]
#[path = "../tests/texture.rs"]
mod tests;