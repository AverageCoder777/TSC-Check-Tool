//! Material binding extraction from GeoPcDx SERZ trees.

use std::collections::{HashMap, HashSet};

use crate::auditor::normalize_texture_reference;
use crate::common::serz::SerzNode;
use crate::geo::helpers::find_text;
use crate::geo::texture::{classify_shader, is_hidden_shader_name, texture_priority};
use crate::geo::types::GeoRenderMode;

use super::{MaterialBinding, contains_ignore_ascii_case};

fn is_shader_field(name: &str) -> bool {
    name.eq_ignore_ascii_case("shadername") || name.eq_ignore_ascii_case("effectname")
}

fn is_active_alpha_test(alpha_test_type: &str) -> bool {
    [
        "HC_ALPHATEST_GREATER",
        "HC_ALPHATEST_GREATEREQUAL",
        "HC_ALPHATEST_LESS",
        "HC_ALPHATEST_LESSEQUAL",
        "HC_ALPHATEST_NOTEQUAL",
    ]
    .iter()
    .any(|mode| alpha_test_type.eq_ignore_ascii_case(mode))
}

pub(crate) fn find_material_bindings(
    node: &SerzNode,
    textures: &[String],
    ids: &HashMap<u32, &SerzNode>,
) -> HashMap<u32, MaterialBinding> {
    struct MaterialCollector<'a> {
        textures: &'a [String],
        ids: &'a HashMap<u32, &'a SerzNode>,
        next_index: u32,
        bindings: HashMap<u32, MaterialBinding>,
        visited_elements: HashSet<u32>,
        reference_stack: HashSet<u32>,
    }

    impl<'a> MaterialCollector<'a> {
        fn collect(&mut self, node: &SerzNode) {
            if let SerzNode::Reference { target_id, .. } = node {
                if self.reference_stack.insert(*target_id) {
                    if let Some(target) = self.ids.get(target_id) {
                        self.collect(target);
                    }
                    self.reference_stack.remove(target_id);
                }
                return;
            }
            let SerzNode::Element {
                name, id, children, ..
            } = node
            else {
                return;
            };
            if *id != 0 && !self.visited_elements.insert(*id) {
                return;
            }
            if name.eq_ignore_ascii_case("cHcEffectMaterialDx")
                || name.eq_ignore_ascii_case("cHcExplicitStateMaterial")
            {
                let binding = self.parse_material_element(children);
                self.bindings.insert(*id, binding);
            }
            for child in children {
                self.collect(child);
            }
        }

        fn parse_material_element(&mut self, children: &[SerzNode]) -> MaterialBinding {
            let mut texture_references =
                collect_text_values(children, &["texturebyname"], self.ids);
            let mut delta_strings = Vec::new();
            let mut stack = HashSet::new();
            for child in children {
                collect_delta_strings(child, self.ids, &mut stack, &mut delta_strings);
            }
            texture_references.extend(delta_strings.into_iter().filter(|value| {
                value.contains('\\')
                    || value.contains('/')
                    || contains_ignore_ascii_case(value, ".tgpcdx")
            }));

            let shader_name =
                find_text(children, &["shadername", "effectname"], self.ids).unwrap_or_default();
            let texture_is_shadow = texture_references
                .iter()
                .any(|texture| contains_ignore_ascii_case(texture, "shadow"));
            let frame_alpha_mode = find_text(children, &["framealphamode"], self.ids)
                .unwrap_or_default()
                .trim();
            let subtract_alpha = frame_alpha_mode.eq_ignore_ascii_case("ALPHA_MODE_SUBTRACT");
            let excluded =
                is_hidden_shader_name(shader_name) || texture_is_shadow || subtract_alpha;

            let material_index = self.next_index;
            self.next_index = self.next_index.saturating_add(1);

            let texture_index = texture_references
                .iter()
                .filter_map(|reference| {
                    let normalized = normalize_texture_reference(reference);
                    self.textures
                        .iter()
                        .position(|texture| texture.eq_ignore_ascii_case(&normalized))
                        .map(|index| (index, texture_priority(&normalized)))
                })
                .min_by_key(|&(_, priority)| priority)
                .map(|(index, _)| index)
                .unwrap_or(0) as u32;

            let material_name =
                find_text(children, &["name", "materialname"], self.ids).unwrap_or_default();
            let trimmed_name = material_name.trim();
            let display_material_name = if trimmed_name.is_empty() {
                "default".to_owned()
            } else {
                trimmed_name.to_owned()
            };

            let alpha_test_type = find_text(children, &["alphatesttype"], self.ids)
                .unwrap_or_default()
                .trim();
            let alpha_test_enabled = is_active_alpha_test(alpha_test_type);
            let alpha_ref = find_text(children, &["alpharef"], self.ids)
                .and_then(|value| value.trim().parse::<f32>().ok())
                .map(|value| (value / 255.0).clamp(0.0, 1.0))
                .unwrap_or(0.5);

            // Shader classification gives a baseline. Material settings can only
            // upgrade Opaque -> Cutout/Blended, never downgrade Cutout/Blended -> Opaque.
            let mut render_mode = classify_shader(shader_name);
            if render_mode == GeoRenderMode::Opaque {
                if frame_alpha_mode.eq_ignore_ascii_case("ALPHA_MODE_BLEND")
                    || frame_alpha_mode.eq_ignore_ascii_case("ALPHA_MODE_ADD")
                {
                    render_mode = GeoRenderMode::Blended;
                } else if frame_alpha_mode.eq_ignore_ascii_case("ALPHA_MODE_TEST")
                    || alpha_test_enabled
                {
                    render_mode = GeoRenderMode::Cutout;
                }
            }
            // sunblend is a semi-transparent cab sun visor material identified by name.
            if contains_ignore_ascii_case(&display_material_name, "sunblend") {
                render_mode = GeoRenderMode::Blended;
            }

            MaterialBinding {
                material_index,
                texture_index,
                render_mode,
                excluded,
                material_name: display_material_name,
                shader_name: shader_name.trim().to_owned(),
                alpha_ref,
            }
        }
    }

    let mut collector = MaterialCollector {
        textures,
        ids,
        next_index: 0,
        bindings: HashMap::new(),
        visited_elements: HashSet::new(),
        reference_stack: HashSet::new(),
    };
    collector.collect(node);
    collector.bindings
}

fn collect_delta_strings(
    node: &SerzNode,
    ids: &HashMap<u32, &SerzNode>,
    reference_stack: &mut HashSet<u32>,
    output: &mut Vec<String>,
) {
    match node {
        SerzNode::Reference { target_id, .. } => {
            if reference_stack.insert(*target_id) {
                if let Some(target) = ids.get(target_id) {
                    collect_delta_strings(target, ids, reference_stack, output);
                }
                reference_stack.remove(target_id);
            }
        }
        SerzNode::Property {
            name,
            type_name,
            value,
            ..
        } if type_name.eq_ignore_ascii_case("cDeltaString") && !is_shader_field(name) => {
            output.push(value.trim().to_owned());
        }
        SerzNode::Array {
            name,
            type_name,
            values,
        } if type_name.eq_ignore_ascii_case("cDeltaString") && !is_shader_field(name) => {
            output.extend(values.iter().map(|value| value.trim().to_owned()));
        }
        SerzNode::Element { name, children, .. } if !is_shader_field(name) => {
            for child in children {
                collect_delta_strings(child, ids, reference_stack, output);
            }
        }
        _ => {}
    }
}

fn collect_text_values(
    children: &[SerzNode],
    expected: &[&str],
    ids: &HashMap<u32, &SerzNode>,
) -> Vec<String> {
    fn collect(
        node: &SerzNode,
        expected: &[&str],
        ids: &HashMap<u32, &SerzNode>,
        reference_stack: &mut HashSet<u32>,
        output: &mut Vec<String>,
    ) {
        let matches_expected = |name: &str| {
            expected
                .iter()
                .any(|candidate| name.eq_ignore_ascii_case(candidate))
        };
        match node {
            SerzNode::Property { name, value, .. } if matches_expected(name) => {
                output.push(value.trim().to_owned());
            }
            SerzNode::Array { name, values, .. } if matches_expected(name) => {
                output.extend(values.iter().map(|value| value.trim().to_owned()));
            }
            SerzNode::Reference { name, target_id } => {
                if reference_stack.insert(*target_id) {
                    if let Some(target) = ids.get(target_id) {
                        if matches_expected(name) {
                            collect_descendant_text(target, ids, reference_stack, output);
                        } else {
                            collect(target, expected, ids, reference_stack, output);
                        }
                    }
                    reference_stack.remove(target_id);
                }
            }
            SerzNode::Element { name, children, .. } if matches_expected(name) => {
                for child in children {
                    collect_descendant_text(child, ids, reference_stack, output);
                }
            }
            SerzNode::Element { children, .. } => {
                for child in children {
                    collect(child, expected, ids, reference_stack, output);
                }
            }
            _ => {}
        }
    }

    fn collect_descendant_text(
        node: &SerzNode,
        ids: &HashMap<u32, &SerzNode>,
        reference_stack: &mut HashSet<u32>,
        output: &mut Vec<String>,
    ) {
        match node {
            SerzNode::Property { value, .. } => output.push(value.trim().to_owned()),
            SerzNode::Array { values, .. } => {
                output.extend(values.iter().map(|value| value.trim().to_owned()));
            }
            SerzNode::Element { children, .. } => {
                for child in children {
                    collect_descendant_text(child, ids, reference_stack, output);
                }
            }
            SerzNode::Reference { target_id, .. } if reference_stack.insert(*target_id) => {
                if let Some(target) = ids.get(target_id) {
                    collect_descendant_text(target, ids, reference_stack, output);
                }
                reference_stack.remove(target_id);
            }
            _ => {}
        }
    }

    let mut values = Vec::new();
    let mut reference_stack = HashSet::new();
    for child in children {
        collect(child, expected, ids, &mut reference_stack, &mut values);
    }
    values
}
