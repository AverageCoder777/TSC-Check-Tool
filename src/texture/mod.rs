//! TgPcDx texture decoding, DDS export/import, and metadata inspection.
//!
//! This module is split into several submodules:
//! - `format` — format handling and mip size calculations
//! - `decode` — decoding TgPcDx to RGBA and UI conversion
//! - `dds` — DDS import/export, mip chain validation, SERZ tree modification
//! - `inspect` — metadata extraction, inspection, and internal name editing
//! - `serz_parse` — common SERZ/XML parsing helpers
//! - `texture_controller` — texture viewer controller

pub mod dds;
pub mod decode;
pub mod format;
pub mod inspect;
pub mod serz_parse;
pub mod texture_controller;

pub use decode::{decode_tgpcdx_to_rgba, rgba_to_slint_image};

pub use dds::{convert_dds_to_tgpcdx_report, export_tgpcdx_to_dds};

pub use inspect::{
    inspect_tgpcdx_file, inspect_tgpcdx_with_path, texture_info_text, update_tgpcdx_internal_name,
};

#[cfg(test)]
#[path = "../tests/texture.rs"]
mod tests;
