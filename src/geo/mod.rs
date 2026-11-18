//! Geometry parsing for GeoPcDx files.

pub mod diagnostics;
pub mod helpers;
pub mod parse;
pub mod texture;
pub mod types;
pub mod vertex;

#[cfg(test)]
pub mod assembly;

#[cfg(test)]
mod tests;

pub use types::{GeoModel, GeoRenderMode, GeoSubmesh, GeoVertex};

#[cfg(test)]
pub use types::{GeoAssembly, GeoAssemblyPart};
