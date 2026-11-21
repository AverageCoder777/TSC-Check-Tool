use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use glam::Vec3;
use rayon::prelude::*;

use super::Texture;
use crate::geo::GeoModel;
use crate::geo::texture::resolve_texture_data;
use crate::texture::decode_tgpcdx_to_rgba;

pub(super) struct LoadedModel {
    pub(super) model: GeoModel,
    pub(super) texture: Arc<Texture>,
    pub(super) textures: Vec<Arc<Texture>>,
    pub(super) texture_status: String,
    pub(super) center: Vec3,
    pub(super) radius: f32,
}

impl LoadedModel {
    pub(super) fn load(path: &Path) -> Result<Self, String> {
        let model = GeoModel::load(path)?;
        let mut archive_toc_cache = HashMap::new();
        let mut unique_resolved = HashMap::<PathBuf, (String, Vec<u8>)>::new();

        let resolved_keys: Vec<Option<PathBuf>> = model
            .textures
            .iter()
            .map(|name| {
                resolve_texture_data(path, name, &mut archive_toc_cache).map(
                    |(resolved_path, bytes)| {
                        let key = PathBuf::from(&resolved_path);
                        unique_resolved
                            .entry(key.clone())
                            .or_insert((resolved_path, bytes));
                        key
                    },
                )
            })
            .collect();

        let decoded: HashMap<PathBuf, (String, Result<Arc<Texture>, String>)> = unique_resolved
            .into_par_iter()
            .map(|(key, (resolved_path, bytes))| {
                let result = decode_tgpcdx_to_rgba(&bytes).map(|(width, height, pixels)| {
                    Arc::new(Texture {
                        width,
                        height,
                        pixels,
                    })
                });
                (key, (resolved_path, result))
            })
            .collect();

        let fallback = Arc::new(Texture::fallback());
        let mut textures = Vec::with_capacity(model.textures.len().max(1));
        let mut statuses = Vec::with_capacity(model.textures.len().max(1));

        for (name, resolved_key) in model.textures.iter().zip(resolved_keys) {
            let resolved_texture =
                if let Some((resolved_path, result)) = resolved_key.and_then(|k| decoded.get(&k)) {
                    match result {
                        Ok(texture) => {
                            statuses.push(format!("Loaded: {name} ({resolved_path})"));
                            Arc::clone(texture)
                        }
                        Err(error) => {
                            statuses.push(format!("Missing / invalid: {name} ({error})"));
                            Arc::clone(&fallback)
                        }
                    }
                } else {
                    statuses.push(format!("Missing: {name}"));
                    Arc::clone(&fallback)
                };
            textures.push(resolved_texture);
        }

        if statuses.is_empty() {
            statuses.push("No texture references found".into());
            textures.push(Arc::clone(&fallback));
        }

        let texture = textures.first().cloned().unwrap_or(fallback);
        let (center, radius) = compute_bounds(&model);

        Ok(Self {
            model,
            texture,
            textures,
            texture_status: statuses.join("    •    "),
            center,
            radius,
        })
    }

    pub(super) fn stats(&self) -> String {
        let mut visible_batches = 0usize;
        let mut visible_triangles = 0u32;
        for submesh in &self.model.submeshes {
            if submesh.visible {
                visible_batches += 1;
                visible_triangles += submesh.triangle_count;
            }
        }
        format!(
            "{} vertices  •  {} / {} triangles  •  {} / {} draw batches  •  {} textures",
            self.model.vertices.len(),
            visible_triangles,
            self.model.indices.len() / 3,
            visible_batches,
            self.model.submeshes.len(),
            self.model.textures.len()
        )
    }
}

fn compute_bounds(model: &GeoModel) -> (Vec3, f32) {
    let mut min = Vec3::splat(f32::INFINITY);
    let mut max = Vec3::splat(f32::NEG_INFINITY);
    for &index in &model.indices {
        if let Some(vertex) = model.vertices.get(index as usize) {
            let position = Vec3::from_array(vertex.position);
            min = min.min(position);
            max = max.max(position);
        }
    }
    if min.is_finite() && max.is_finite() {
        let center = (min + max) * 0.5;
        let radius = ((max - min) * 0.5).length().max(0.01);
        (center, radius)
    } else {
        (Vec3::ZERO, 0.01)
    }
}