//! High-level SERZ AST transforms: blueprint replacement and entity/ribbon removal.

use super::SerzNode;

/// Сравнивает имя SERZ-узла с эталоном через `-`, считая `::` эквивалентным `-` (без аллокаций).
fn name_matches_dash(name: &str, expected_dash: &str) -> bool {
    let mut name_parts = name.split("::").flat_map(|p| p.split('-'));
    let mut exp_parts = expected_dash.split('-');
    loop {
        match (name_parts.next(), exp_parts.next()) {
            (Some(a), Some(b)) if a == b => continue,
            (None, None) => return true,
            _ => return false,
        }
    }
}

/// Нормализует расширение блюпринта к `.xml`.
fn blueprint_ext_to_xml(path: &str) -> String {
    if let Some(stripped) = path.strip_suffix(".bin") {
        format!("{stripped}.xml")
    } else if !path.ends_with(".xml") {
        format!("{path}.xml")
    } else {
        path.to_owned()
    }
}

fn blueprint_matches(
    prov: Option<String>,
    prod: Option<String>,
    bp: Option<String>,
    provider: &str,
    product: &str,
    blueprint: &str,
) -> bool {
    let (Some(prov), Some(prod), Some(bp)) = (prov, prod, bp) else {
        return false;
    };
    if !prov.eq_ignore_ascii_case(provider) || !prod.eq_ignore_ascii_case(product) {
        return false;
    }
    let norm_bp = bp.replace('/', "\\").to_lowercase();
    let mut norm_target = blueprint.replace('/', "\\").to_lowercase();
    let prefix = format!("{}\\{}\\", provider.to_lowercase(), product.to_lowercase());
    if let Some(stripped) = norm_target.strip_prefix(&prefix) {
        norm_target = stripped.to_string();
    }
    blueprint_ext_to_xml(&norm_bp) == blueprint_ext_to_xml(&norm_target)
}

fn entity_matches_blueprint(
    node: &SerzNode,
    provider: &str,
    product: &str,
    blueprint: &str,
) -> bool {
    let (mut prov, mut prod, mut bp) = (None, None, None);
    collect_blueprint_ids(node, &mut prov, &mut prod, &mut bp);
    blueprint_matches(prov, prod, bp, provider, product, blueprint)
}

fn loft_matches_blueprint(node: &SerzNode, provider: &str, product: &str, blueprint: &str) -> bool {
    let (mut prov, mut prod, mut bp) = (None, None, None);
    collect_section_generic_props(node, &mut prov, &mut prod, &mut bp);
    blueprint_matches(prov, prod, bp, provider, product, blueprint)
}

fn collect_section_generic_props(
    node: &SerzNode,
    prov: &mut Option<String>,
    prod: &mut Option<String>,
    bp: &mut Option<String>,
) {
    if let SerzNode::Element { name, children, .. } = node {
        if name_matches_dash(name, "Network-cSectionGenericProperties") {
            extract_blueprint_from_section(children, prov, prod, bp);
            return;
        }
        for child in children {
            collect_section_generic_props(child, prov, prod, bp);
        }
    }
}

fn extract_blueprint_from_section(
    children: &[SerzNode],
    prov: &mut Option<String>,
    prod: &mut Option<String>,
    bp: &mut Option<String>,
) {
    for child in children {
        if let SerzNode::Element {
            name,
            children: sub_children,
            ..
        } = child
            && name == "BlueprintID"
        {
            for sub in sub_children {
                if let SerzNode::Element {
                    name: abs_name,
                    children: abs_children,
                    ..
                } = sub
                    && name_matches_dash(abs_name, "iBlueprintLibrary-cAbsoluteBlueprintID")
                {
                    collect_from_absolute_blueprint_id(abs_children, prov, prod, bp);
                }
            }
        }
    }
}

fn collect_from_absolute_blueprint_id(
    children: &[SerzNode],
    prov: &mut Option<String>,
    prod: &mut Option<String>,
    bp: &mut Option<String>,
) {
    for child in children {
        match child {
            SerzNode::Element {
                name: bs_name,
                children: bs_children,
                ..
            } if bs_name == "BlueprintSetID" => {
                for bs_sub in bs_children {
                    if let SerzNode::Element {
                        name: ibs_name,
                        children: ibs_children,
                        ..
                    } = bs_sub
                        && name_matches_dash(ibs_name, "iBlueprintLibrary-cBlueprintSetID")
                    {
                        for prop in ibs_children {
                            if let SerzNode::Property {
                                name: p_name,
                                value,
                                ..
                            } = prop
                            {
                                if prov.is_none() && p_name == "Provider" {
                                    *prov = Some(value.clone());
                                }
                                if prod.is_none() && p_name == "Product" {
                                    *prod = Some(value.clone());
                                }
                            }
                        }
                    }
                }
            }
            SerzNode::Property {
                name: p_name,
                value,
                ..
            } if p_name == "BlueprintID" && bp.is_none() => {
                *bp = Some(value.clone());
            }
            _ => {}
        }
    }
}

fn collect_blueprint_ids(
    node: &SerzNode,
    prov: &mut Option<String>,
    prod: &mut Option<String>,
    bp: &mut Option<String>,
) {
    match node {
        SerzNode::Element { name, children, .. } => {
            if name == "ReskinBlueprintID" {
                return;
            }
            for child in children {
                collect_blueprint_ids(child, prov, prod, bp);
            }
        }
        SerzNode::Property { name, value, .. } if !value.trim().is_empty() => {
            if name == "Provider" && prov.is_none() {
                *prov = Some(value.clone());
            } else if name == "Product" && prod.is_none() {
                *prod = Some(value.clone());
            } else if name == "BlueprintID" && bp.is_none() {
                *bp = Some(value.clone());
            }
        }
        _ => {}
    }
}

impl SerzNode {
    pub fn replace_blueprint_references(
        &mut self,
        old_provider: &str,
        old_product: &str,
        old_blueprint: &str,
        new_provider: &str,
        new_product: &str,
        new_blueprint: &str,
    ) -> usize {
        let mut count = 0;
        let SerzNode::Element { name, children, .. } = self else {
            return 0;
        };

        if name_matches_dash(name, "iBlueprintLibrary-cAbsoluteBlueprintID") {
            let mut prov_match = false;
            let mut prod_match = false;
            let mut blueprint_match = false;

            for child in children.iter() {
                match child {
                    SerzNode::Element {
                        name: c_name,
                        children: sub_children,
                        ..
                    } if c_name == "BlueprintSetID" => {
                        for sub in sub_children {
                            if let SerzNode::Element {
                                children: set_children,
                                ..
                            } = sub
                            {
                                for prop in set_children {
                                    if let SerzNode::Property {
                                        name: p_name,
                                        value,
                                        ..
                                    } = prop
                                    {
                                        if p_name == "Provider"
                                            && value.eq_ignore_ascii_case(old_provider)
                                        {
                                            prov_match = true;
                                        }
                                        if p_name == "Product"
                                            && value.eq_ignore_ascii_case(old_product)
                                        {
                                            prod_match = true;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    SerzNode::Property {
                        name: p_name,
                        value,
                        ..
                    } if p_name == "BlueprintID" => {
                        let norm_val = value.replace('/', "\\").to_lowercase();
                        let norm_old = old_blueprint.replace('/', "\\").to_lowercase();
                        let val_xml = norm_val
                            .strip_suffix(".bin")
                            .map(|s| format!("{s}.xml"))
                            .unwrap_or(norm_val);
                        let old_xml = norm_old
                            .strip_suffix(".bin")
                            .map(|s| format!("{s}.xml"))
                            .unwrap_or(norm_old);
                        if val_xml == old_xml {
                            blueprint_match = true;
                        }
                    }
                    _ => {}
                }
            }

            if prov_match && prod_match && (old_blueprint.is_empty() || blueprint_match) {
                for child in children.iter_mut() {
                    match child {
                        SerzNode::Element {
                            name: c_name,
                            children: sub_children,
                            ..
                        } if c_name == "BlueprintSetID" => {
                            for sub in sub_children.iter_mut() {
                                if let SerzNode::Element {
                                    children: set_children,
                                    ..
                                } = sub
                                {
                                    for prop in set_children.iter_mut() {
                                        if let SerzNode::Property {
                                            name: p_name,
                                            value,
                                            ..
                                        } = prop
                                        {
                                            if p_name == "Provider" {
                                                *value = new_provider.to_string();
                                            }
                                            if p_name == "Product" {
                                                *value = new_product.to_string();
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        SerzNode::Property {
                            name: p_name,
                            value,
                            ..
                        } if p_name == "BlueprintID" && !new_blueprint.is_empty() => {
                            *value = new_blueprint.replace('/', "\\");
                        }
                        _ => {}
                    }
                }
                count += 1;
                println!("[Replacement Debug] A match was found and replaced in the tile.");
            }
        }

        for child in children.iter_mut() {
            count += child.replace_blueprint_references(
                old_provider,
                old_product,
                old_blueprint,
                new_provider,
                new_product,
                new_blueprint,
            );
        }
        count
    }

    pub fn remove_c_dynamic_entities(
        &mut self,
        provider: &str,
        product: &str,
        blueprint: &str,
    ) -> usize {
        let mut removed = 0;
        if let SerzNode::Element { children, .. } = self {
            let mut kept = Vec::with_capacity(children.len());
            for mut child in children.drain(..) {
                if let SerzNode::Element {
                    name: child_name, ..
                } = &child
                    && child_name == "cDynamicEntity"
                    && entity_matches_blueprint(&child, provider, product, blueprint)
                {
                    removed += 1;
                    continue;
                }
                if matches!(&child, SerzNode::Element { .. }) {
                    removed += child.remove_c_dynamic_entities(provider, product, blueprint);
                }
                kept.push(child);
            }
            *children = kept;
        }
        removed
    }

    pub fn remove_loft_ribbons(&mut self, provider: &str, product: &str, blueprint: &str) -> usize {
        let mut removed = 0;
        if let SerzNode::Element { children, .. } = self {
            let mut kept = Vec::with_capacity(children.len());
            for mut child in children.drain(..) {
                let is_ribbon = matches!(
                    &child,
                    SerzNode::Element { name, .. }
                        if name.contains("LoftRibbon") || name.contains("RoadRibbon")
                );
                if is_ribbon && loft_matches_blueprint(&child, provider, product, blueprint) {
                    removed += 1;
                    continue;
                }
                if matches!(&child, SerzNode::Element { .. }) {
                    removed += child.remove_loft_ribbons(provider, product, blueprint);
                }
                kept.push(child);
            }
            *children = kept;
        }
        removed
    }
}
