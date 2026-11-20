//! Geometry parsing for GeoPcDx files.

pub mod diagnostics;
pub mod gpu_renderer;
pub mod helpers;
pub mod parse;
pub mod texture;
pub mod types;
pub mod vertex;
pub mod viewer3d;

#[cfg(test)]
pub mod assembly;

#[cfg(test)]
mod tests;

pub use types::{GeoModel, GeoRenderMode, GeoSubmesh, GeoVertex};

#[cfg(test)]
pub use types::{GeoAssembly, GeoAssemblyPart};

pub use viewer3d::{show_geo_viewer, GeoViewerWindow};
