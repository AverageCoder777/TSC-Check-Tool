#[cfg(test)]
mod tests {
    use crate::geo::assembly::{
        collect_blueprint_attachments, find_geometry_blueprint, matching_geometry_blueprint,
        resolve_asset_path,
    };
    use crate::geo::helpers::{
        build_id_map, collect_float_values, collect_i32_values, compute_world_transforms,
        find_named_values, find_number, find_transform_id, matrix_from_values, parse_serz_document,
        select_vertex_stride,
    };
    use crate::geo::parse::{
        NodeContext, RawSubmesh, convert_to_triangle_list, deduplicate_submeshes,
        find_material_bindings, find_submeshes,
    };
    use crate::geo::texture::{classify_shader, is_hidden_shader_name, resolve_texture_data};
    use crate::geo::types::PrimitiveTopology;
    use crate::geo::{GeoAssembly, GeoAssemblyPart, GeoModel, GeoRenderMode, GeoSubmesh};
    use crate::serz::SerzNode;
    use glam::{Mat4, Vec3};
    use std::collections::{HashMap, HashSet};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    fn triangle_vertices(count: usize) -> Vec<u8> {
        let mut vertices = Vec::new();
        for index in 0..count {
            let position = [index as f32, (index % 2) as f32, 0.0];
            for value in position
                .into_iter()
                .chain([0.0, 0.0, 1.0])
                .chain([0.0, 0.0])
            {
                vertices.extend_from_slice(&value.to_le_bytes());
            }
        }
        vertices
    }

    fn xml(vertices: &[u8], indices: &[u8], stride: usize) -> String {
        let vertex_hex = vertices
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect::<String>();
        let index_hex = indices
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect::<String>();
        format!(
            r#"<Root xmlns:d="http://www.kuju.com/TnT/2003/Delta">
                <cHcVertexBuffer><VertexStride d:type="sUInt32">{stride}</VertexStride>
                    <VertexData><blob>{vertex_hex}</blob></VertexData>
                </cHcVertexBuffer>
                <cHcIndexBuffer><IndexSize d:type="sUInt32">2</IndexSize>
                    <IndexData><blob>{index_hex}</blob></IndexData>
                </cHcIndexBuffer>
                <TextureByName><e d:type="cDeltaString">Textures\sample.TgPcDx</e></TextureByName>
            </Root>"#
        )
    }

    #[test]
    fn reads_blueprint_geometry_and_attachment_transforms() {
        let xml = r#"
            <Root xmlns:d="http://www.kuju.com/TnT/2003/Delta">
                <cWagonBlueprint d:id="1">
                    <GeometryID d:type="cDeltaString">Models\Main.GeoPcDx</GeometryID>
                    <Bogey>
                        <BogeyBlueprint d:type="cDeltaString">Bogies\front.xml</BogeyBlueprint>
                        <BogeyPivotX d:type="sFloat32">3</BogeyPivotX>
                        <BogeyPivotY d:type="sFloat32">-1</BogeyPivotY>
                        <BogiePivotZ d:type="sFloat32">7</BogiePivotZ>
                    </Bogey>
                    <Bogey d:type="ref">2</Bogey>
                    <ContainerComponent>
                        <Children>
                            <cEntityContainerBlueprint-sChild>
                                <BlueprintID d:type="cDeltaString">Parts\pantograph.xml</BlueprintID>
                                <Matrix d:type="sFloat32">1 0 0 0 0 1 0 0 0 0 1 0 5 6 7 1</Matrix>
                            </cEntityContainerBlueprint-sChild>
                        </Children>
                    </ContainerComponent>
                </cWagonBlueprint>
                <BogeyEntry d:id="2">
                    <BogeyBlueprint d:type="cDeltaString">Bogies\rear.xml</BogeyBlueprint>
                    <BogeyPivotX d:type="sFloat32">-3</BogeyPivotX>
                </BogeyEntry>
            </Root>
        "#;
        let root = SerzNode::from_xml_str(xml).unwrap().0;
        let ids = build_id_map(&root);

        let blueprint = matching_geometry_blueprint(&root, "main", &ids).unwrap();
        let mut attachments = Vec::new();
        collect_blueprint_attachments(&blueprint, &ids, false, false, &mut attachments);
        assert_eq!(
            attachments
                .iter()
                .map(|attachment| attachment.blueprint_reference.as_str())
                .collect::<Vec<_>>(),
            [
                r"Bogies\front.xml",
                r"Bogies\rear.xml",
                r"Parts\pantograph.xml"
            ]
        );
        assert_eq!(attachments[0].kind, "bogey");
        assert_eq!(attachments[0].blueprint_reference, r"Bogies\front.xml");
        assert_eq!(
            attachments[0].transform.w_axis.truncate(),
            Vec3::new(3.0, -1.0, 7.0)
        );
        assert_eq!(attachments[1].kind, "bogey");
        assert_eq!(
            attachments[1].transform.w_axis.truncate(),
            Vec3::new(-3.0, 0.0, 0.0)
        );
        assert_eq!(attachments[2].kind, "child");
        assert_eq!(
            attachments[2].transform.w_axis.truncate(),
            Vec3::new(5.0, 6.0, 7.0)
        );
    }

    #[test]
    fn discovers_sibling_blueprint_and_resolves_asset_relative_links() {
        let directory = tempfile::tempdir().unwrap();
        let assets = directory.path().join("Assets");
        let model_dir = assets.join("Provider").join("Product").join("Models");
        let bogey_dir = assets.join("Provider").join("Product").join("Bogies");
        let geometry_path = model_dir.join("Main.GeoPcDx");
        let blueprint_path = model_dir.join("Main.bin");
        let bogey_path = bogey_dir.join("Front.bin");
        std::fs::create_dir_all(&model_dir).unwrap();
        std::fs::create_dir_all(&bogey_dir).unwrap();
        std::fs::write(&geometry_path, b"").unwrap();
        std::fs::write(
            &blueprint_path,
            r#"<Root xmlns:d="http://www.kuju.com/TnT/2003/Delta">
                <cWagonBlueprint>
                    <GeometryID d:type="cDeltaString">Provider\Product\Models\[00]Main.GeoPcDx</GeometryID>
                </cWagonBlueprint>
            </Root>"#,
        )
        .unwrap();
        std::fs::write(&bogey_path, b"").unwrap();

        let (found_path, _, _) = find_geometry_blueprint(&geometry_path, "main").unwrap();
        let resolved =
            resolve_asset_path(&found_path, r"Provider\Product\Bogies\[00]Front.xml", "bin")
                .unwrap();
        let resolved_geometry = resolve_asset_path(
            &found_path,
            r"Provider\Product\Models\[00]Main.GeoPcDx",
            "GeoPcDx",
        )
        .unwrap();

        assert_eq!(found_path, blueprint_path);
        assert_eq!(resolved, bogey_path);
        assert_eq!(resolved_geometry, geometry_path);
    }

    #[test]
    fn composes_attached_parts_and_side_by_side_layouts() {
        let source = xml(&triangle_vertices(3), &[0, 0, 1, 0, 2, 0], 32);
        let model = Arc::new(GeoModel::parse(Path::new(""), source.as_bytes()).unwrap());
        let assembly = GeoAssembly {
            parts: vec![
                GeoAssemblyPart {
                    path: PathBuf::from("Main.GeoPcDx"),
                    model: Arc::clone(&model),
                    transform: Mat4::IDENTITY,
                    blueprint_attached: false,
                },
                GeoAssemblyPart {
                    path: PathBuf::from("Bogey.GeoPcDx"),
                    model,
                    transform: Mat4::from_translation(Vec3::new(10.0, -1.0, 2.0)),
                    blueprint_attached: true,
                },
            ],
            has_blueprint_attachments: true,
        };

        let (assembled, sources) = assembly.compose(false).unwrap();
        let bogey_position = assembled.vertices[assembled.indices[3] as usize].position;
        assert_eq!(bogey_position, [10.0, -1.0, 2.0]);
        assert_eq!(sources.len(), assembled.textures.len());

        let (side_by_side, _) = assembly.compose(true).unwrap();
        let bogey_position = side_by_side.vertices[side_by_side.indices[3] as usize].position;
        assert_eq!(bogey_position, [3.5, 0.0, 0.0]);
    }

    #[test]
    fn parses_mesh_and_texture_reference() {
        let vertices = triangle_vertices(3);
        let model = GeoModel::parse(
            Path::new(""),
            &xml(&vertices, &[0, 0, 1, 0, 2, 0], 32).into_bytes(),
        )
        .unwrap();
        assert_eq!(model.vertices.len(), 3);
        assert_eq!(model.indices, [0, 1, 2]);
        assert_eq!(model.textures, [r"Textures\sample.TgPcDx"]);
    }

    #[test]
    fn keeps_vertex_data_and_count_scoped_to_each_vertex_buffer() {
        let narrow_vertices = [[0.0_f32, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]]
            .into_iter()
            .flatten()
            .flat_map(f32::to_le_bytes)
            .collect::<Vec<_>>();
        let narrow_blob = narrow_vertices
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect::<String>();
        let unrelated_blob = "0000000000000000";
        let second_buffer = format!(
            r#"
                <cHcVertexBuffer>
                    <OtherData>
                        <NumVertices d:type="sUInt32">5</NumVertices>
                        <IndexData><blob>{unrelated_blob}</blob></IndexData>
                    </OtherData>
                    <NumVertices d:type="sUInt32">3</NumVertices>
                    <blob>{narrow_blob}</blob>
                </cHcVertexBuffer>
            "#
        );
        let source = xml(&triangle_vertices(3), &[0, 0, 1, 0, 2, 0], 32)
            .replace("</Root>", &format!("{second_buffer}</Root>"));

        let model = GeoModel::parse(Path::new(""), source.as_bytes()).unwrap();

        assert_eq!(model.vertices.len(), 6);
        assert_eq!(model.indices, [0, 1, 2]);
    }

    #[test]
    fn rejects_indices_outside_vertex_buffer() {
        let mut vertices = Vec::new();
        for _ in 0..3 {
            vertices.extend_from_slice(&[0u8; 32]);
        }
        assert!(
            GeoModel::parse(
                Path::new(""),
                &xml(&vertices, &[0, 0, 1, 0, 3, 0], 32).into_bytes()
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_truncated_vertex_records() {
        assert!(
            GeoModel::parse(
                Path::new(""),
                &xml(&[0; 31], &[0, 0, 1, 0, 2, 0], 32).into_bytes()
            )
            .is_err()
        );
    }

    #[test]
    fn truncates_incomplete_triangle_list_indices_and_odd_byte_padding() {
        let vertices = triangle_vertices(4);
        let raw_indices = [0, 0, 1, 0, 2, 0, 3, 0, 0xff];
        let model = GeoModel::parse(
            Path::new(""),
            &xml(&vertices, &raw_indices, 32).into_bytes(),
        )
        .unwrap();
        assert_eq!(model.diagnostics.index_element_type, "u16");
        assert_eq!(model.diagnostics.raw_index_count, 4);
        assert_eq!(model.indices, [0, 1, 2]);
    }

    #[test]
    fn converts_triangle_strips_with_alternating_winding_and_batch_ranges() {
        let vertices = triangle_vertices(6);
        let batch = r#"
            <cHcPrimitiveList>
                <StartIndex d:type="sUInt32">1</StartIndex>
                <IndexCount d:type="sUInt32">4</IndexCount>
                <PrimitiveType d:type="cDeltaString">eTriangleStrip</PrimitiveType>
                <MaterialIndex d:type="sUInt32">2</MaterialIndex>
            </cHcPrimitiveList>
        "#;
        let source = xml(&vertices, &[5, 0, 0, 0, 1, 0, 2, 0, 3, 0, 4, 0], 32)
            .replace("</Root>", &format!("{batch}</Root>"));
        let model = GeoModel::parse(Path::new(""), source.as_bytes()).unwrap();
        assert_eq!(model.indices, [0, 1, 2, 2, 1, 3]);
        assert_eq!(
            model.submeshes,
            [GeoSubmesh {
                first_index: 0,
                index_count: 6,
                material_index: 2,
                texture_index: 0,
                render_mode: GeoRenderMode::Opaque,
                alpha_cutoff: 0.5,
                vertex_buffer_index: 0,
                base_vertex: 0,
                vertex_buffer_offset: 0,
                visible: true,
                node_name: model.submeshes[0].node_name.clone(),
                material_name: "default".into(),
                shader_name: String::new(),
                lod_level: 0,
                triangle_count: 2,
                mirrored: false,
            }]
        );
        assert_eq!(
            model.diagnostics.primitive_type,
            PrimitiveTopology::TriangleStrip
        );
        assert!(model.diagnostics.batch_ranges[0].contains("start=1 count=4"));
    }

    #[test]
    fn preserves_primitive_set_materials_and_base_vertex_offsets() {
        let batches_and_materials = r#"
            <cHcSGNodeMaterial>
                <Material d:type="ref">7</Material>
                <cHcPrimitiveSetDescDx>
                    <NumPrimitives d:type="sUInt32">3</NumPrimitives>
                    <PrimitiveType d:type="cDeltaString">eTriangleList</PrimitiveType>
                    <VertexIndexBase d:type="sUInt32">0</VertexIndexBase>
                    <IndexArrayStart d:type="sUInt32">0</IndexArrayStart>
                </cHcPrimitiveSetDescDx>
            </cHcSGNodeMaterial>
            <cHcSGNodeMaterial>
                <Material d:type="ref">8</Material>
                <cHcPrimitiveSetDescDx>
                    <NumPrimitives d:type="sUInt32">3</NumPrimitives>
                    <PrimitiveType d:type="cDeltaString">eTriangleList</PrimitiveType>
                    <VertexIndexBase d:type="sUInt32">3</VertexIndexBase>
                    <IndexArrayStart d:type="sUInt32">3</IndexArrayStart>
                </cHcPrimitiveSetDescDx>
            </cHcSGNodeMaterial>
            <cHcEffectMaterialDx d:id="7">
                <TextureByName><e d:type="cDeltaString">Textures\first.TgPcDx</e></TextureByName>
            </cHcEffectMaterialDx>
            <cHcEffectMaterialDx d:id="8">
                <TextureByName><e d:type="cDeltaString">Textures\second.TgPcDx</e></TextureByName>
            </cHcEffectMaterialDx>
        "#;
        let source = xml(
            &triangle_vertices(9),
            &[0, 0, 1, 0, 2, 0, 3, 0, 4, 0, 5, 0],
            32,
        )
        .replace("</Root>", &format!("{batches_and_materials}</Root>"));
        let model = GeoModel::parse(Path::new(""), source.as_bytes()).unwrap();

        assert_eq!(model.submeshes.len(), 2);
        assert_eq!(model.indices, [0, 1, 2, 6, 7, 8]);
        let first_texture = model
            .textures
            .iter()
            .position(|texture| texture.eq_ignore_ascii_case(r"Textures\first.TgPcDx"))
            .unwrap() as u32;
        let second_texture = model
            .textures
            .iter()
            .position(|texture| texture.eq_ignore_ascii_case(r"Textures\second.TgPcDx"))
            .unwrap() as u32;
        assert_eq!(model.submeshes[0].texture_index, first_texture);
        assert_eq!(model.submeshes[1].texture_index, second_texture);
        assert_eq!(model.submeshes[1].base_vertex, 3);
    }

    #[test]
    fn primitive_set_counts_follow_triangle_topology() {
        let batches = r#"
            <cHcPrimitiveSetDescDx>
                <NumPrimitives d:type="sUInt32">3</NumPrimitives>
                <PrimitiveType d:type="cDeltaString">eTriangleList</PrimitiveType>
                <IndexArrayStart d:type="sUInt32">0</IndexArrayStart>
            </cHcPrimitiveSetDescDx>
            <cHcPrimitiveSetDescDx>
                <NumPrimitives d:type="sUInt32">4</NumPrimitives>
                <PrimitiveType d:type="cDeltaString">eTriangleStrip</PrimitiveType>
                <IndexArrayStart d:type="sUInt32">3</IndexArrayStart>
            </cHcPrimitiveSetDescDx>
        "#;
        let source = xml(
            &triangle_vertices(4),
            &[0, 0, 1, 0, 2, 0, 0, 0, 1, 0, 2, 0, 3, 0],
            32,
        )
        .replace("</Root>", &format!("{batches}</Root>"));
        let model = GeoModel::parse(Path::new(""), source.as_bytes()).unwrap();

        assert_eq!(model.indices, [0, 1, 2, 0, 1, 2, 2, 1, 3]);
        assert!(model.diagnostics.batch_ranges[0].contains("start=0 count=3"));
        assert!(model.diagnostics.batch_ranges[1].contains("start=3 count=4"));
    }

    #[test]
    fn resolves_referenced_primitive_types_for_triangle_strips() {
        let batches = r#"
            <cHcPrimitiveSetDescDx>
                <NumPrimitives d:id="43">
                    <Value d:type="sUInt32">4</Value>
                </NumPrimitives>
                <PrimitiveType d:id="42">
                    <Value d:type="cDeltaString">PRIMTYPE_TRIANGLESTRIP</Value>
                </PrimitiveType>
                <IndexArrayStart d:type="sUInt32">0</IndexArrayStart>
                <VertexIndexBase d:id="44">
                    <Value d:type="sUInt32">0</Value>
                </VertexIndexBase>
                <VertexIndexMin d:id="45">
                    <Value d:type="sUInt32">0</Value>
                </VertexIndexMin>
                <VertexIndexRange d:id="46">
                    <Value d:type="sUInt32">4</Value>
                </VertexIndexRange>
            </cHcPrimitiveSetDescDx>
            <cHcPrimitiveSetDescDx>
                <NumPrimitives d:type="ref">43</NumPrimitives>
                <PrimitiveType d:type="ref">42</PrimitiveType>
                <IndexArrayStart d:type="sUInt32">4</IndexArrayStart>
                <VertexIndexBase d:type="ref">44</VertexIndexBase>
                <VertexIndexMin d:type="ref">45</VertexIndexMin>
                <VertexIndexRange d:type="ref">46</VertexIndexRange>
            </cHcPrimitiveSetDescDx>
        "#;
        let source = xml(
            &triangle_vertices(4),
            &[0, 0, 1, 0, 2, 0, 3, 0, 0, 0, 1, 0, 2, 0, 3, 0],
            32,
        )
        .replace("</Root>", &format!("{batches}</Root>"));
        let parsed_root = parse_serz_document(source.as_bytes()).unwrap();
        let ids = build_id_map(&parsed_root);
        let SerzNode::Element { children, .. } = &parsed_root else {
            panic!("expected root element");
        };
        assert_eq!(find_number(children, &["numprimitives"], &ids), Some(4));
        assert_eq!(find_number(children, &["vertexindexbase"], &ids), Some(0));
        assert_eq!(find_number(children, &["vertexindexmin"], &ids), Some(0));
        assert_eq!(find_number(children, &["vertexindexrange"], &ids), Some(4));
        let model = GeoModel::parse(Path::new(""), source.as_bytes()).unwrap();

        assert_eq!(
            model.diagnostics.primitive_type,
            PrimitiveTopology::TriangleStrip
        );
        assert_eq!(model.diagnostics.batch_ranges.len(), 2);
        assert!(
            model
                .diagnostics
                .batch_ranges
                .iter()
                .all(|range| range.contains("topology=TriangleStrip"))
        );
        assert_eq!(model.indices, [0, 1, 2, 2, 1, 3, 0, 1, 2, 2, 1, 3]);
    }

    #[test]
    fn reads_float_matrices_and_integer_dependencies_from_blobs() {
        let matrix_values = [
            1.0_f32, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 3.0, 4.0, 0.0, 1.0,
        ];
        let matrix_bytes = matrix_values
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect();
        let dependency_bytes = [-1_i32, 0].into_iter().flat_map(i32::to_le_bytes).collect();
        let root = SerzNode::Element {
            name: "Root".into(),
            id: 0,
            version: 0,
            children: vec![
                SerzNode::Element {
                    name: "SourceLToPTransform".into(),
                    id: 0,
                    version: 0,
                    children: vec![SerzNode::Blob { data: matrix_bytes }],
                },
                SerzNode::Element {
                    name: "TransformDependency".into(),
                    id: 0,
                    version: 0,
                    children: vec![SerzNode::Blob {
                        data: dependency_bytes,
                    }],
                },
            ],
        };
        let transforms = find_named_values(&root, "SourceLToPTransform")
            .unwrap()
            .into_iter()
            .flat_map(collect_float_values)
            .collect::<Vec<_>>();
        let dependencies = find_named_values(&root, "TransformDependency")
            .unwrap()
            .into_iter()
            .flat_map(collect_i32_values)
            .collect::<Vec<_>>();

        assert_eq!(transforms.len(), 16);
        assert_eq!(
            matrix_from_values(&transforms).unwrap().w_axis.truncate(),
            Vec3::new(3.0, 4.0, 0.0)
        );
        assert_eq!(dependencies, [-1, 0]);
    }

    #[test]
    fn applies_root_transform_dependency_hierarchy_to_scenegraph_batches() {
        let transforms = r#"
            <SourceLToPTransform d:type="sFloat32">
                1 0 0 0 0 1 0 0 0 0 1 0 3 0 0 1
                1 0 0 0 0 1 0 0 0 0 1 0 0 4 0 1
            </SourceLToPTransform>
            <TransformDependency d:type="sInt32">-1 0</TransformDependency>
            <cHcTransformNode>
                <Transform d:type="sFloat32">1 0 0 0 0 1 0 0 0 0 1 0 100 0 0 1</Transform>
                <cHcGeometryNodeSameState>
                    <PrimaryTransformID d:type="sInt32">1</PrimaryTransformID>
                    <cHcPrimitiveList>
                        <StartIndex d:type="sUInt32">0</StartIndex>
                        <IndexCount d:type="sUInt32">3</IndexCount>
                    </cHcPrimitiveList>
                </cHcGeometryNodeSameState>
            </cHcTransformNode>
        "#;
        let source = xml(&triangle_vertices(3), &[0, 0, 1, 0, 2, 0], 32)
            .replace("</Root>", &format!("{transforms}</Root>"));
        let model = GeoModel::parse(Path::new(""), source.as_bytes()).unwrap();

        assert_eq!(model.diagnostics.local_transform_count, 2);
        assert_eq!(
            model.vertices[model.indices[0] as usize].position,
            [3.0, 4.0, 0.0]
        );
    }

    #[test]
    fn invalid_transform_dependencies_fall_back_to_identity() {
        let local = [
            Mat4::from_translation(Vec3::X),
            Mat4::from_translation(Vec3::Y),
            Mat4::from_translation(Vec3::Z),
        ];
        let cyclic = compute_world_transforms(&local[..2], &[1, 0]);
        assert_eq!(cyclic, [Mat4::IDENTITY, Mat4::IDENTITY]);

        let invalid_parent = compute_world_transforms(&local[2..], &[9]);
        assert_eq!(invalid_parent, [Mat4::IDENTITY]);
    }

    #[test]
    fn keeps_only_lod_zero_and_excludes_helper_batches() {
        let batches = r#"
            <LOD0><cHcPrimitiveList>
                <StartIndex d:type="sUInt32">0</StartIndex>
                <IndexCount d:type="sUInt32">3</IndexCount>
            </cHcPrimitiveList></LOD0>
            <LOD1><cHcPrimitiveList>
                <StartIndex d:type="sUInt32">0</StartIndex>
                <IndexCount d:type="sUInt32">3</IndexCount>
            </cHcPrimitiveList></LOD1>
            <softshadow><cHcPrimitiveList>
                <StartIndex d:type="sUInt32">0</StartIndex>
                <IndexCount d:type="sUInt32">3</IndexCount>
            </cHcPrimitiveList></softshadow>
            <StencilShadow><cHcPrimitiveList>
                <StartIndex d:type="sUInt32">0</StartIndex>
                <IndexCount d:type="sUInt32">3</IndexCount>
            </cHcPrimitiveList></StencilShadow>
            <DummyNode><cHcPrimitiveList>
                <StartIndex d:type="sUInt32">0</StartIndex>
                <IndexCount d:type="sUInt32">3</IndexCount>
            </cHcPrimitiveList></DummyNode>
        "#;
        let source = xml(&triangle_vertices(3), &[0, 0, 1, 0, 2, 0], 32)
            .replace("</Root>", &format!("{batches}</Root>"));
        let model = GeoModel::parse(Path::new(""), source.as_bytes()).unwrap();

        assert_eq!(model.submeshes.len(), 1);
        assert_eq!(model.indices, [0, 1, 2]);
        assert_eq!(model.diagnostics.batch_ranges.len(), 1);
    }

    #[test]
    fn traverses_only_the_first_lod_selector_child() {
        let selector = r#"
            <cHcSGNodeLODSelector>
                <Child>
                    <LOD0><cHcPrimitiveList>
                        <StartIndex d:type="sUInt32">0</StartIndex>
                        <IndexCount d:type="sUInt32">3</IndexCount>
                    </cHcPrimitiveList></LOD0>
                </Child>
                <Child>
                    <LOD1><cHcPrimitiveList>
                        <StartIndex d:type="sUInt32">0</StartIndex>
                        <IndexCount d:type="sUInt32">3</IndexCount>
                    </cHcPrimitiveList></LOD1>
                </Child>
            </cHcSGNodeLODSelector>
        "#;
        let source = xml(&triangle_vertices(3), &[0, 0, 1, 0, 2, 0], 32)
            .replace("</Root>", &format!("{selector}</Root>"));
        let model = GeoModel::parse(Path::new(""), source.as_bytes()).unwrap();

        assert_eq!(model.submeshes.len(), 1);
        assert_eq!(model.indices, [0, 1, 2]);
    }

    #[test]
    fn applies_parent_transform_to_batch_vertices() {
        let transform = r#"
            <cHcTransformNode>
                <Transform d:type="sFloat32">1 0 0 0 0 1 0 0 0 0 1 0 3 -2 4 1</Transform>
                <cHcPrimitiveList>
                    <StartIndex d:type="sUInt32">0</StartIndex>
                    <IndexCount d:type="sUInt32">3</IndexCount>
                </cHcPrimitiveList>
            </cHcTransformNode>
        "#;
        let source = xml(&triangle_vertices(3), &[0, 0, 1, 0, 2, 0], 32)
            .replace("</Root>", &format!("{transform}</Root>"));
        let model = GeoModel::parse(Path::new(""), source.as_bytes()).unwrap();
        let transformed_position = model.vertices[model.indices[0] as usize].position;

        assert_eq!(transformed_position, [3.0, -2.0, 4.0]);

        let skeleton = transform.replace("cHcTransformNode", "cHcSkeletonNode");
        let source = xml(&triangle_vertices(3), &[0, 0, 1, 0, 2, 0], 32)
            .replace("</Root>", &format!("{skeleton}</Root>"));
        let model = GeoModel::parse(Path::new(""), source.as_bytes()).unwrap();
        assert_eq!(
            model.vertices[model.indices[0] as usize].position,
            [3.0, -2.0, 4.0]
        );

        let matrix = matrix_from_values(&[
            1.0, 0.0, 0.0, 0.25, 0.0, 1.0, 0.0, 0.5, 0.0, 0.0, 1.0, 0.75, 3.0, -2.0, 4.0, 1.0,
        ])
        .unwrap();
        assert_eq!(matrix, Mat4::IDENTITY);

        assert_eq!(
            matrix_from_values(&[
                1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 13.2, 0.0, 0.0, 0.0, 0.0, 1.0,
            ])
            .unwrap()
            .z_axis
            .truncate()
            .length(),
            13.2
        );
        assert_eq!(
            matrix_from_values(&[
                1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0,
            ]),
            Some(Mat4::IDENTITY)
        );
        assert_eq!(
            matrix_from_values(&[
                1.1, 0.0, 0.0, 0.0, 0.0, 0.9, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 3.0, -2.0, 4.0, 1.0,
            ])
            .unwrap()
            .x_axis
            .truncate()
            .length(),
            1.1
        );
        assert_eq!(
            matrix_from_values(&[
                1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
            ]),
            Some(Mat4::IDENTITY)
        );
    }

    #[test]
    fn classifies_shader_alpha_and_helper_batches() {
        assert_eq!(classify_shader("TrainGlass"), GeoRenderMode::Blended);
        assert_eq!(classify_shader("RailVehicleFlora"), GeoRenderMode::Cutout);
        assert_eq!(classify_shader("BlendATexDiff"), GeoRenderMode::Cutout);
        assert_eq!(classify_shader("TrainDecal"), GeoRenderMode::Cutout);
        assert_eq!(classify_shader("TrainStandard"), GeoRenderMode::Opaque);
        assert!(is_hidden_shader_name("TrainShadowOnly"));
        assert!(is_hidden_shader_name("StencilShadow"));
        assert!(is_hidden_shader_name("Invisible"));
        assert!(!is_hidden_shader_name("TrainDecal"));
        assert!(!is_hidden_shader_name("number_plate"));

        let batches = r#"
            <cHcSGNodeMaterial>
                <Material d:type="ref">7</Material>
                <cHcPrimitiveList>
                    <StartIndex d:type="sUInt32">0</StartIndex>
                    <IndexCount d:type="sUInt32">3</IndexCount>
                </cHcPrimitiveList>
            </cHcSGNodeMaterial>
            <cHcPrimitiveList>
                <StartIndex d:type="sUInt32">0</StartIndex>
                <IndexCount d:type="sUInt32">3</IndexCount>
            </cHcPrimitiveList>
            <cHcEffectMaterialDx d:id="7">
                <ShaderName d:type="cDeltaString">TrainShadowOnly</ShaderName>
                <TextureByName d:type="cDeltaString">Textures\sample.TgPcDx</TextureByName>
            </cHcEffectMaterialDx>
        "#;
        let source = xml(&triangle_vertices(3), &[0, 0, 1, 0, 2, 0], 32)
            .replace("</Root>", &format!("{batches}</Root>"));
        let model = GeoModel::parse(Path::new(""), source.as_bytes()).unwrap();
        assert_eq!(model.submeshes.len(), 1);
        assert_eq!(model.indices, [0, 1, 2]);
    }

    #[test]
    fn excludes_subtractive_and_shadow_materials_and_prefers_diffuse_textures() {
        let xml = r#"
            <Root xmlns:d="http://www.kuju.com/TnT/2003/Delta">
                <cHcExplicitStateMaterial d:id="7">
                    <TextureByName d:type="cDeltaString">Textures\body_nm.TgPcDx</TextureByName>
                    <TextureByName d:type="cDeltaString">Textures\body_diffuse.TgPcDx</TextureByName>
                </cHcExplicitStateMaterial>
                <cHcExplicitStateMaterial d:id="8">
                    <FrameAlphaMode d:type="cDeltaString">ALPHA_MODE_SUBTRACT</FrameAlphaMode>
                    <ShaderName d:type="cDeltaString">Lit3DirSingle</ShaderName>
                    <TextureByName d:type="cDeltaString">Textures\shadow_mask.TgPcDx</TextureByName>
                </cHcExplicitStateMaterial>
                <cHcExplicitStateMaterial d:id="9">
                    <Name d:type="cDeltaString">ShadowMaterial</Name>
                    <TextureByName d:type="cDeltaString">Textures\shadow.TgPcDx</TextureByName>
                </cHcExplicitStateMaterial>
            </Root>
        "#;
        let root = SerzNode::from_xml_str(xml).unwrap().0;
        let textures = [
            r"Textures\body_nm.TgPcDx".to_owned(),
            r"Textures\body_diffuse.TgPcDx".to_owned(),
            r"Textures\shadow.TgPcDx".to_owned(),
        ];
        let bindings = find_material_bindings(&root, &textures, &build_id_map(&root));

        assert_eq!(bindings[&7].texture_index, 1);
        assert!(!bindings[&7].excluded);
        assert!(bindings[&8].excluded);
        assert!(bindings[&9].excluded);
    }

    #[test]
    fn promotes_alpha_tested_materials_to_cutout_mode() {
        let xml = r#"
            <Root xmlns:d="http://www.kuju.com/TnT/2003/Delta">
                <cHcEffectMaterialDx d:id="7">
                    <ShaderName d:type="cDeltaString">TrainBasicObjectDiffuse.fx</ShaderName>
                    <AlphaTestType d:type="ref">8</AlphaTestType>
                </cHcEffectMaterialDx>
                <AlphaTestValue d:id="8">
                    <Value d:type="cDeltaString">HC_ALPHATEST_GREATER</Value>
                </AlphaTestValue>
            </Root>
        "#;
        let root = SerzNode::from_xml_str(xml).unwrap().0;
        let bindings = find_material_bindings(&root, &[], &build_id_map(&root));

        assert_eq!(bindings[&7].render_mode, GeoRenderMode::Cutout);
    }

    #[test]
    fn classifies_frame_alpha_modes_from_references() {
        let xml = r#"
            <Root xmlns:d="http://www.kuju.com/TnT/2003/Delta">
                <cHcEffectMaterialDx d:id="1">
                    <FrameAlphaMode d:type="ref">11</FrameAlphaMode>
                </cHcEffectMaterialDx>
                <cHcEffectMaterialDx d:id="2">
                    <FrameAlphaMode d:type="ref">12</FrameAlphaMode>
                </cHcEffectMaterialDx>
                <cHcEffectMaterialDx d:id="3">
                    <FrameAlphaMode d:type="ref">13</FrameAlphaMode>
                </cHcEffectMaterialDx>
                <cHcEffectMaterialDx d:id="4">
                    <FrameAlphaMode d:type="ref">14</FrameAlphaMode>
                </cHcEffectMaterialDx>
                <FrameAlphaTest d:id="11">
                    <Value d:type="cDeltaString">ALPHA_MODE_TEST</Value>
                </FrameAlphaTest>
                <FrameAlphaBlend d:id="12">
                    <Value d:type="cDeltaString">ALPHA_MODE_BLEND</Value>
                </FrameAlphaBlend>
                <FrameAlphaAdd d:id="13">
                    <Value d:type="cDeltaString">ALPHA_MODE_ADD</Value>
                </FrameAlphaAdd>
                <FrameAlphaSubtract d:id="14">
                    <Value d:type="cDeltaString">ALPHA_MODE_SUBTRACT</Value>
                </FrameAlphaSubtract>
            </Root>
        "#;
        let root = SerzNode::from_xml_str(xml).unwrap().0;
        let bindings = find_material_bindings(&root, &[], &build_id_map(&root));

        assert_eq!(bindings[&1].render_mode, GeoRenderMode::Cutout);
        assert_eq!(bindings[&2].render_mode, GeoRenderMode::Blended);
        assert_eq!(bindings[&3].render_mode, GeoRenderMode::Blended);
        assert!(bindings[&4].excluded);
    }

    #[test]
    fn renders_instanced_submesh_references_under_distinct_transforms() {
        let root_xml = r#"
            <Root xmlns:d="http://www.kuju.com/TnT/2003/Delta">
                <cHcSGNodeZSorter>
                    <TransformID d:type="sUInt32">0</TransformID>
                    <SingleChildTransform d:type="bool">1</SingleChildTransform>
                    <Child d:type="ref">7</Child>
                </cHcSGNodeZSorter>
                <cHcSGNodeZSorter>
                    <TransformID d:type="sUInt32">1</TransformID>
                    <SingleChildTransform d:type="bool">1</SingleChildTransform>
                    <Child d:type="ref">7</Child>
                </cHcSGNodeZSorter>
                <cHcSGNodeZSorter>
                    <TransformID d:type="sUInt32">0</TransformID>
                    <SingleChildTransform d:type="bool">1</SingleChildTransform>
                    <Child d:type="ref">7</Child>
                </cHcSGNodeZSorter>
            </Root>
        "#;
        let shared_xml = r#"
            <cHcGeometryNodeSameState xmlns:d="http://www.kuju.com/TnT/2003/Delta" d:id="7">
                <PrimaryTransformID d:type="sUInt32">0</PrimaryTransformID>
                <cHcPrimitiveList>
                    <StartIndex d:type="sUInt32">0</StartIndex>
                    <IndexCount d:type="sUInt32">3</IndexCount>
                </cHcPrimitiveList>
            </cHcGeometryNodeSameState>
        "#;
        let root = SerzNode::from_xml_str(root_xml).unwrap().0;
        let shared = SerzNode::from_xml_str(shared_xml).unwrap().0;
        let ids = HashMap::from([(7, &shared)]);
        let transforms = [
            Mat4::from_translation(Vec3::X),
            Mat4::from_translation(Vec3::Y),
        ];
        let mut batches = Vec::new();
        let no_bindings = HashMap::new();
        let empty_vb_map = HashMap::new();
        let context = NodeContext {
            material_bindings: &no_bindings,
            ids: &ids,
            world_transforms: &transforms,
            transform_names: &[],
            vb_id_to_index: &empty_vb_map,
            prim_id_to_vbuf: &empty_vb_map,
        };
        find_submeshes(
            &root,
            None,
            &context,
            Mat4::IDENTITY,
            None,
            None,
            false,
            None,
            None,
            &mut HashSet::new(),
            &mut batches,
        );

        assert_eq!(batches.len(), 3);
        deduplicate_submeshes(&mut batches);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].transform, transforms[0]);
        assert_eq!(batches[1].transform, transforms[1]);
    }

    #[test]
    fn z_sorter_transform_requires_single_child_transform() {
        let root_xml = r#"
            <Root xmlns:d="http://www.kuju.com/TnT/2003/Delta">
                <cHcSGNodeZSorter>
                    <TransformID d:type="sUInt32">1</TransformID>
                    <SingleChildTransform d:type="bool">0</SingleChildTransform>
                    <cHcPrimitiveList>
                        <StartIndex d:type="sUInt32">0</StartIndex>
                        <IndexCount d:type="sUInt32">3</IndexCount>
                    </cHcPrimitiveList>
                </cHcSGNodeZSorter>
                <cHcSGNodeZSorter>
                    <TransformID d:type="sUInt32">1</TransformID>
                    <SingleChildTransform d:type="bool">1</SingleChildTransform>
                    <cHcPrimitiveList>
                        <StartIndex d:type="sUInt32">0</StartIndex>
                        <IndexCount d:type="sUInt32">3</IndexCount>
                    </cHcPrimitiveList>
                </cHcSGNodeZSorter>
            </Root>
        "#;
        let root = SerzNode::from_xml_str(root_xml).unwrap().0;
        let transforms = [Mat4::IDENTITY, Mat4::from_translation(Vec3::Y)];
        let mut batches = Vec::new();
        let (no_bindings, no_ids) = (HashMap::new(), HashMap::new());
        let empty_vb_map = HashMap::new();
        let context = NodeContext {
            material_bindings: &no_bindings,
            ids: &no_ids,
            world_transforms: &transforms,
            transform_names: &[],
            vb_id_to_index: &empty_vb_map,
            prim_id_to_vbuf: &empty_vb_map,
        };
        find_submeshes(
            &root,
            None,
            &context,
            Mat4::IDENTITY,
            None,
            None,
            false,
            None,
            None,
            &mut HashSet::new(),
            &mut batches,
        );

        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].transform, transforms[1]);
        assert_eq!(batches[1].transform, transforms[1]);
    }

    #[test]
    fn excludes_lower_lod_names_and_recognizes_lod_separators() {
        assert_eq!(crate::geo::texture::lod_from_name("bogie_lod1"), Some(1));
        assert_eq!(crate::geo::texture::lod_from_name("bogie_lod_2"), Some(2));
        assert!(crate::geo::texture::lod_name_is_nonzero("bogie_lod1"));
        assert!(crate::geo::texture::lod_name_is_nonzero("bogie_lod_4"));
        assert!(!crate::geo::texture::lod_name_is_nonzero("bogie_lod0"));
    }

    #[test]
    fn skips_degenerate_triangles_in_triangle_strips() {
        assert_eq!(
            convert_to_triangle_list(&[0, 1, 2, 2, 3, 4], PrimitiveTopology::TriangleStrip, "u16"),
            [0, 1, 2, 3, 2, 4]
        );
    }

    #[test]
    fn clamps_triangle_strip_batches_to_declared_primitive_count() {
        let batch = r#"
            <cHcPrimitiveList>
                <StartIndex d:type="sUInt32">0</StartIndex>
                <IndexCount d:type="sUInt32">8</IndexCount>
                <PrimCount d:type="sUInt32">4</PrimCount>
                <PrimitiveType d:type="cDeltaString">eTriangleStrip</PrimitiveType>
            </cHcPrimitiveList>
        "#;
        let source = xml(
            &triangle_vertices(6),
            &[0, 0, 1, 0, 2, 0, 3, 0, 4, 0, 5, 0],
            32,
        )
        .replace("</Root>", &format!("{batch}</Root>"));
        let model = GeoModel::parse(Path::new(""), source.as_bytes()).unwrap();

        assert_eq!(model.indices, [0, 1, 2, 2, 1, 3, 2, 3, 4, 4, 3, 5]);
        assert!(model.diagnostics.batch_ranges[0].contains("count=6"));
    }

    #[test]
    fn selects_a_vertex_stride_that_divides_the_buffer() {
        assert_eq!(select_vertex_stride(3 * 36, Some(52), Some(36), 0, 0), 36);
        assert_eq!(select_vertex_stride(3 * 52, Some(52), Some(52), 0, 0), 52);
        assert_eq!(select_vertex_stride(96, None, None, 0, 0), 32);
    }

    #[test]
    fn resolves_batch_node_names_from_transform_name_table() {
        let batch = r#"
            <TransformName d:type="cDeltaString">Body</TransformName>
            <TransformName d:type="cDeltaString">Cabin</TransformName>
            <cHcGeometryNode>
                <TransformID d:type="sUInt32">1</TransformID>
                <cHcPrimitiveList>
                    <StartIndex d:type="sUInt32">0</StartIndex>
                    <IndexCount d:type="sUInt32">3</IndexCount>
                </cHcPrimitiveList>
            </cHcGeometryNode>
        "#;
        let source = xml(&triangle_vertices(3), &[0, 0, 1, 0, 2, 0], 32)
            .replace("</Root>", &format!("{batch}</Root>"));
        let model = GeoModel::parse(Path::new(""), source.as_bytes()).unwrap();

        assert_eq!(model.submeshes[0].node_name, "Cabin");
    }

    #[test]
    fn decodes_u32_index_blobs_and_primitive_count_strip_ranges() {
        let index_data = [0u32, 1, 2, 3]
            .into_iter()
            .flat_map(u32::to_le_bytes)
            .collect::<Vec<_>>();
        let mut source = xml(&triangle_vertices(4), &index_data, 32).replace(
            "<IndexSize d:type=\"sUInt32\">2</IndexSize>",
            "<IndexSize d:type=\"sUInt32\">32</IndexSize>",
        );
        let batch = r#"
            <cHcPrimitiveList>
                <StartIndex d:type="sUInt32">0</StartIndex>
                <PrimCount d:type="sUInt32">2</PrimCount>
                <PrimitiveType d:type="cDeltaString">eTriangleStrip</PrimitiveType>
            </cHcPrimitiveList>
        "#;
        source = source.replace("</Root>", &format!("{batch}</Root>"));
        let model = GeoModel::parse(Path::new(""), source.as_bytes()).unwrap();
        assert_eq!(model.diagnostics.index_element_type, "u32");
        assert_eq!(model.diagnostics.raw_index_count, 4);
        assert_eq!(model.indices, [0, 1, 2, 2, 1, 3]);
    }

    #[test]
    fn decodes_implicit_u32_index_blob_when_vertices_exceed_u16_max() {
        let index_data = [u32::from(u16::MAX), u32::from(u16::MAX) - 1, 1, 0]
            .into_iter()
            .flat_map(u32::to_le_bytes)
            .collect::<Vec<_>>();
        let source = xml(
            &triangle_vertices(usize::from(u16::MAX) + 1),
            &index_data,
            32,
        )
        .replace("<IndexSize d:type=\"sUInt32\">2</IndexSize>", "")
        .replace(
            "</Root>",
            r#"
                    <cHcPrimitiveList>
                        <StartIndex d:type="sUInt32">0</StartIndex>
                        <IndexCount d:type="sUInt32">3</IndexCount>
                    </cHcPrimitiveList>
                </Root>"#,
        );
        let model = GeoModel::parse(Path::new(""), source.as_bytes()).unwrap();

        assert_eq!(model.diagnostics.index_element_type, "u16");
        assert_eq!(
            model.indices,
            [u32::from(u16::MAX), 0, u32::from(u16::MAX) - 1]
        );
    }

    #[test]
    fn reads_index_data_arrays_from_serz_ast() {
        let mut source = xml(&triangle_vertices(3), &[0, 0, 1, 0, 2, 0], 32);
        source = source.replace(
            "<IndexData><blob>000001000200</blob></IndexData>",
            "<IndexData d:numElements=\"3\" d:elementType=\"sUInt16\">0 1 2</IndexData>",
        );
        let model = GeoModel::parse(Path::new(""), source.as_bytes()).unwrap();
        assert_eq!(model.indices, [0, 1, 2]);
        assert_eq!(model.diagnostics.index_element_type, "u16");
    }

    #[test]
    fn resolves_texture_from_neighboring_ap_archive() {
        use std::io::Write;

        let directory = tempfile::tempdir().unwrap();
        let product = directory
            .path()
            .join("Assets")
            .join("Provider")
            .join("Product");
        let geo_dir = product.join("Models");
        std::fs::create_dir_all(&geo_dir).unwrap();
        let geo_path = geo_dir.join("model.GeoPcDx");
        std::fs::write(&geo_path, b"").unwrap();

        let archive_path = product.join("MainContent.ap");
        let mut archive = zip::ZipWriter::new(std::fs::File::create(archive_path).unwrap());
        archive
            .start_file(
                "Provider/Product/Textures/body.TgPcDx",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        archive.write_all(b"texture data").unwrap();
        archive.finish().unwrap();

        let mut archive_toc_cache = HashMap::new();
        let (resolved_path, _bytes) = resolve_texture_data(
            &geo_path,
            r"Textures\[00]body.TgPcDx",
            &mut archive_toc_cache,
        )
        .unwrap();
        assert!(resolved_path.contains("!Provider\\Product\\Textures\\body.TgPcDx"));
    }

    #[test]
    fn parses_vertex_index_bases_plural_with_u16_on_large_buffer() {
        let batches = r#"
            <cHcPrimitiveSetDescDx>
                <NumPrimitives d:type="sUInt32">3</NumPrimitives>
                <PrimitiveType d:type="cDeltaString">eTriangleList</PrimitiveType>
                <VertexIndexBases d:type="sUInt32">5</VertexIndexBases>
                <IndexArrayStart d:type="sUInt32">0</IndexArrayStart>
                <VertexIndexMin d:type="sUInt32">0</VertexIndexMin>
                <VertexIndexRange d:type="sUInt32">3</VertexIndexRange>
            </cHcPrimitiveSetDescDx>
        "#;
        let vertices = triangle_vertices(usize::from(u16::MAX) + 1);
        let source = xml(&vertices, &[0, 0, 1, 0, 2, 0], 32)
            .replace("</Root>", &format!("{batches}</Root>"));
        let model = GeoModel::parse(Path::new(""), source.as_bytes()).unwrap();

        assert_eq!(model.diagnostics.index_element_type, "u16");
        assert_eq!(model.submeshes[0].base_vertex, 5);
    }

    #[test]
    fn parses_transform_ids_plural_and_preserves_mirrored_matrices() {
        let mirrored = matrix_from_values(&[
            -1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
        ])
        .unwrap();
        assert!(mirrored.x_axis.x < 0.0);
        assert_eq!(mirrored.x_axis.truncate().length(), 1.0);

        let root_xml = r#"
            <Root xmlns:d="http://www.kuju.com/TnT/2003/Delta">
                <cHcGeometryNodeSameState>
                    <PrimaryTransformIDs d:type="sInt32">0</PrimaryTransformIDs>
                    <cHcPrimitiveList>
                        <StartIndex d:type="sUInt32">0</StartIndex>
                        <IndexCount d:type="sUInt32">3</IndexCount>
                    </cHcPrimitiveList>
                </cHcGeometryNodeSameState>
                <cHcSGNodeZSorter>
                    <TransformIDs d:type="sUInt32">1</TransformIDs>
                    <SingleChildTransform d:type="bool">0</SingleChildTransform>
                    <cHcPrimitiveList>
                        <StartIndex d:type="sUInt32">0</StartIndex>
                        <IndexCount d:type="sUInt32">3</IndexCount>
                    </cHcPrimitiveList>
                </cHcSGNodeZSorter>
            </Root>
        "#;
        let root = SerzNode::from_xml_str(root_xml).unwrap().0;
        let transforms = [Mat4::IDENTITY, Mat4::from_scale(Vec3::new(-1.0, 1.0, 1.0))];
        let mut batches = Vec::new();
        let (no_bindings, no_ids) = (HashMap::new(), HashMap::new());
        let empty_vb_map = HashMap::new();
        let context = NodeContext {
            material_bindings: &no_bindings,
            ids: &no_ids,
            world_transforms: &transforms,
            transform_names: &[],
            vb_id_to_index: &empty_vb_map,
            prim_id_to_vbuf: &empty_vb_map,
        };
        find_submeshes(
            &root,
            None,
            &context,
            Mat4::IDENTITY,
            None,
            None,
            false,
            None,
            None,
            &mut HashSet::new(),
            &mut batches,
        );
        assert_eq!(batches.len(), 2);
    }

    #[test]
    fn parses_d3d9_fvf_vertex_stride_xyz_normal_tex1() {
        let vertices = triangle_vertices(3);
        let mut source = xml(&vertices, &[0, 0, 1, 0, 2, 0], 32);
        source = source.replace("<VertexStride d:type=\"sUInt32\">32</VertexStride>", "");
        source = source.replace(
            "</cHcVertexBuffer>",
            "<NumVertices d:type=\"sUInt32\">3</NumVertices><cHcVertexTypeFVF><FVF d:type=\"sUInt32\">274</FVF></cHcVertexTypeFVF></cHcVertexBuffer>",
        );
        let model = match GeoModel::parse(Path::new(""), source.as_bytes()) {
            Ok(m) => m,
            Err(e) => panic!("parse error: {e}"),
        };
        assert_eq!(model.diagnostics.vertex_stride, 32);
        assert_eq!(model.diagnostics.declared_vertex_count, 3);
        assert_eq!(model.diagnostics.fvf, 0x112);
    }

    #[test]
    fn parses_d3d9_fvf_vertex_stride_xyz_normal_tex2() {
        let vertices = vec![0u8; 120];
        let mut source = xml(&vertices, &[0, 0, 1, 0, 2, 0], 32);
        source = source.replace("<VertexStride d:type=\"sUInt32\">32</VertexStride>", "");
        source = source.replace(
            "</cHcVertexBuffer>",
            "<NumVertices d:type=\"sUInt32\">3</NumVertices><cHcVertexTypeFVF><FVF d:type=\"sUInt32\">530</FVF></cHcVertexTypeFVF></cHcVertexBuffer>",
        );
        let model = GeoModel::parse(Path::new(""), source.as_bytes()).unwrap();
        assert_eq!(model.diagnostics.vertex_stride, 40);
        assert_eq!(model.diagnostics.fvf, 0x212);
    }

    #[test]
    fn parses_unsigned_minus_one_transform_ids_as_none() {
        let xml = r#"<Root xmlns:d="http://www.kuju.com/TnT/2003/Delta">
            <cHcSGNodeZSorter>
                <TransformIDs d:type="sUInt32">4294967295</TransformIDs>
                <SingleChildTransform d:type="bool">0</SingleChildTransform>
                <cHcPrimitiveList>
                    <StartIndex d:type="sUInt32">0</StartIndex>
                    <IndexCount d:type="sUInt32">3</IndexCount>
                </cHcPrimitiveList>
            </cHcSGNodeZSorter>
        </Root>"#;
        let root = SerzNode::from_xml_str(xml).unwrap().0;
        let ids = build_id_map(&root);
        let SerzNode::Element { children, .. } = &root else {
            panic!("expected root");
        };
        let zsorter = children
            .iter()
            .find(|child| {
                matches!(child, SerzNode::Element { name, .. } if name.to_ascii_lowercase().contains("zsorter"))
            })
            .unwrap();
        if let SerzNode::Element { children, .. } = zsorter {
            assert_eq!(find_transform_id(children, &ids), None);
        }
    }

    #[test]
    fn keeps_numbering_decal_and_suffix_named_geometry() {
        // These are real surfaces on rolling stock: dynamic number plates, decal overlays,
        // the second bogie/cab (`_b`/`_c`) and the second body shell. They must survive.
        let batches = r#"
            <body_b><Transform d:type="sFloat32">1 0 0 0 0 1 0 0 0 0 1 0 1 0 0 1</Transform>
                <cHcPrimitiveList>
                    <StartIndex d:type="sUInt32">0</StartIndex>
                    <IndexCount d:type="sUInt32">3</IndexCount>
                </cHcPrimitiveList></body_b>
            <cab_c><Transform d:type="sFloat32">1 0 0 0 0 1 0 0 0 0 1 0 2 0 0 1</Transform>
                <cHcPrimitiveList>
                    <StartIndex d:type="sUInt32">0</StartIndex>
                    <IndexCount d:type="sUInt32">3</IndexCount>
                </cHcPrimitiveList></cab_c>
            <number_plate><Transform d:type="sFloat32">1 0 0 0 0 1 0 0 0 0 1 0 3 0 0 1</Transform>
                <cHcPrimitiveList>
                    <StartIndex d:type="sUInt32">0</StartIndex>
                    <IndexCount d:type="sUInt32">3</IndexCount>
                </cHcPrimitiveList></number_plate>
            <TrainDecalNode><Transform d:type="sFloat32">1 0 0 0 0 1 0 0 0 0 1 0 4 0 0 1</Transform>
                <cHcPrimitiveList>
                    <StartIndex d:type="sUInt32">0</StartIndex>
                    <IndexCount d:type="sUInt32">3</IndexCount>
                </cHcPrimitiveList></TrainDecalNode>
            <hidden_panel><Transform d:type="sFloat32">1 0 0 0 0 1 0 0 0 0 1 0 5 0 0 1</Transform>
                <Visible d:type="sUInt32">0</Visible>
                <cHcPrimitiveList>
                    <StartIndex d:type="sUInt32">0</StartIndex>
                    <IndexCount d:type="sUInt32">3</IndexCount>
                </cHcPrimitiveList></hidden_panel>
        "#;
        let source = xml(&triangle_vertices(3), &[0, 0, 1, 0, 2, 0], 32)
            .replace("</Root>", &format!("{batches}</Root>"));
        let model = GeoModel::parse(Path::new(""), source.as_bytes()).unwrap();

        assert_eq!(model.submeshes.len(), 4);
        assert_eq!(model.indices.len(), 12);
    }

    #[test]
    fn deduplicates_only_exact_batches_not_material_layers() {
        let base = RawSubmesh {
            first_index: 0,
            index_count: 3,
            material_reference: Some(7),
            material_index: Some(0),
            texture_index: Some(0),
            vertex_buffer_index: 0,
            index_buffer_index: 0,
            base_vertex: 0,
            max_local_vertex_ref: Some(3),
            topology: PrimitiveTopology::TriangleList,
            transform: Mat4::IDENTITY,
            excluded: false,
            node_name: "body".into(),
            material_name: "body".into(),
            shader_name: "TrainStandard".into(),
            render_mode: GeoRenderMode::Opaque,
            lod_level: 0,
        };
        let dirt_layer = RawSubmesh {
            material_name: "body_dirt".into(),
            ..base.clone()
        };
        let blended_overlay = RawSubmesh {
            render_mode: GeoRenderMode::Blended,
            ..base.clone()
        };
        let mut batches = vec![base, dirt_layer.clone(), blended_overlay, dirt_layer];
        deduplicate_submeshes(&mut batches);

        assert_eq!(batches.len(), 3);
    }

    #[test]
    fn skips_only_the_broken_vertex_buffer_and_its_batch() {
        // Buffer #0 has a NaN position; buffer #1 is valid. The model must still load and
        // render the batch that addresses buffer #1.
        let mut broken = Vec::new();
        for vertex in 0..3 {
            for value in [
                if vertex == 0 { f32::NAN } else { vertex as f32 },
                0.0,
                0.0,
                0.0,
                0.0,
                1.0,
                0.0,
                0.0,
            ] {
                broken.extend_from_slice(&value.to_le_bytes());
            }
        }
        assert_eq!(broken.len(), 96);
        let broken_hex = broken
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<String>();
        let valid = triangle_vertices(3);
        let valid_hex = valid.iter().map(|b| format!("{b:02X}")).collect::<String>();
        let index_hex = [0u16, 1, 2]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .map(|b| format!("{b:02X}"))
            .collect::<String>();
        let source = format!(
            r#"<Root xmlns:d="http://www.kuju.com/TnT/2003/Delta">
                <cHcVertexBuffer>
                    <VertexStride d:type="sUInt32">32</VertexStride>
                    <VertexData><blob>{broken_hex}</blob></VertexData>
                </cHcVertexBuffer>
                <cHcVertexBuffer>
                    <VertexStride d:type="sUInt32">32</VertexStride>
                    <VertexData><blob>{valid_hex}</blob></VertexData>
                </cHcVertexBuffer>
                <cHcIndexBuffer>
                    <IndexSize d:type="sUInt32">2</IndexSize>
                    <IndexData><blob>{index_hex}</blob></IndexData>
                </cHcIndexBuffer>
                <cHcPrimitiveList>
                    <StartIndex d:type="sUInt32">0</StartIndex>
                    <IndexCount d:type="sUInt32">3</IndexCount>
                    <VertexBufferIndex d:type="sUInt32">0</VertexBufferIndex>
                    <VertexIndexBase d:type="sUInt32">0</VertexIndexBase>
                </cHcPrimitiveList>
                <cHcPrimitiveList>
                    <StartIndex d:type="sUInt32">0</StartIndex>
                    <IndexCount d:type="sUInt32">3</IndexCount>
                    <VertexBufferIndex d:type="sUInt32">1</VertexBufferIndex>
                    <VertexIndexBase d:type="sUInt32">0</VertexIndexBase>
                    <Transform d:type="sFloat32">1 0 0 0 0 1 0 0 0 0 1 0 0 0 0 1</Transform>
                </cHcPrimitiveList>
                <TextureByName><e d:type="cDeltaString">Textures\sample.TgPcDx</e></TextureByName>
            </Root>"#
        );
        let model = GeoModel::parse(Path::new(""), source.as_bytes()).unwrap();

        assert_eq!(model.submeshes.len(), 1);
        assert_eq!(model.indices.len(), 3);
        assert!(model.vertices.iter().all(|vertex| {
            vertex.position.iter().all(|value| value.is_finite())
                && vertex.normal.iter().all(|value| value.is_finite())
                && vertex.uv.iter().all(|value| value.is_finite())
        }));
    }

    #[test]
    fn multi_level_parent_child_matrix_multiplication() {
        let local = [
            Mat4::from_translation(Vec3::new(1.0, 0.0, 0.0)),
            Mat4::from_translation(Vec3::new(0.0, 2.0, 0.0)),
            Mat4::from_translation(Vec3::new(0.0, 0.0, 3.0)),
        ];
        let parents = [-1, 0, 1];
        let world = compute_world_transforms(&local, &parents);
        assert_eq!(world[0].w_axis.truncate(), Vec3::new(1.0, 0.0, 0.0));
        assert_eq!(world[1].w_axis.truncate(), Vec3::new(1.0, 2.0, 0.0));
        assert_eq!(world[2].w_axis.truncate(), Vec3::new(1.0, 2.0, 3.0));
    }
    #[test]
    fn declared_vertex_count_resolves_stride_when_fvf_differs() {
        let vertices = vec![0u8; 52 * 3];
        let mut source = xml(&vertices, &[0, 0, 1, 0, 2, 0], 32);
        source = source.replace("<VertexStride d:type=\"sUInt32\">32</VertexStride>", "");
        source = source.replace(
            "</cHcVertexBuffer>",
            r#"<NumVertices d:type="sUInt32">3</NumVertices><cHcVertexTypeFVF><FVF d:type="sUInt32">274</FVF></cHcVertexTypeFVF></cHcVertexBuffer>"#,
        );
        let model = GeoModel::parse(Path::new(""), source.as_bytes()).unwrap();

        assert_eq!(model.diagnostics.vertex_stride, 52);
        assert_eq!(model.diagnostics.vertex_count, 3);
    }

    #[test]
    fn decodes_float16_4_position_from_vertex_type_declaration() {
        let half = |v: f32| -> u16 {
            match v {
                1.0 => 0x3C00,
                2.0 => 0x4000,
                3.0 => 0x4200,
                4.0 => 0x4400,
                5.0 => 0x4500,
                6.0 => 0x4600,
                7.0 => 0x4700,
                8.0 => 0x4800,
                0.0 => 0x0000,
                _ => 0,
            }
        };
        let mut bytes = Vec::new();
        for [x, y, z] in [[1.0f32, 2.0, 3.0], [4.0, 5.0, 6.0], [7.0, 8.0, 0.0]] {
            bytes.extend_from_slice(&half(x).to_le_bytes());
            bytes.extend_from_slice(&half(y).to_le_bytes());
            bytes.extend_from_slice(&half(z).to_le_bytes());
            bytes.extend_from_slice(&0u16.to_le_bytes());
        }
        let blob = bytes.iter().map(|b| format!("{b:02X}")).collect::<String>();
        let index_hex = [0u16, 1, 2]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .map(|b| format!("{b:02X}"))
            .collect::<String>();
        let xml = format!(
            r#"<Root xmlns:d="http://www.kuju.com/TnT/2003/Delta">
                <cHcVertexBuffer>
                    <cHcVertexTypeDeclarationDx>
                        <ElementDecl>
                            <Usage d:type="cDeltaString">POSITION</Usage>
                            <Type d:type="cDeltaString">FLOAT16_4</Type>
                            <StreamIndex d:type="sUInt32">0</StreamIndex>
                            <Offset d:type="sUInt32">0</Offset>
                        </ElementDecl>
                    </cHcVertexTypeDeclarationDx>
                    <VertexData><blob>{blob}</blob></VertexData>
                </cHcVertexBuffer>
                <cHcIndexBuffer>
                    <IndexSize d:type="sUInt32">2</IndexSize>
                    <IndexData><blob>{index_hex}</blob></IndexData>
                </cHcIndexBuffer>
                <TextureByName><e d:type="cDeltaString">Textures\sample.TgPcDx</e></TextureByName>
            </Root>"#
        );
        let model = GeoModel::parse(Path::new(""), xml.as_bytes()).unwrap();
        assert_eq!(model.vertices.len(), 3);
        assert_eq!(model.vertices[0].position, [1.0, 2.0, 3.0]);
        assert_eq!(model.vertices[1].position, [4.0, 5.0, 6.0]);
        assert_eq!(model.vertices[2].position, [7.0, 8.0, 0.0]);
    }

    #[test]
    fn decodes_short4n_position_from_vertex_type_declaration() {
        // short4n: value/32767, (-32768 -> -1.0)
        let mut bytes = Vec::new();
        for [x, y, z] in [
            [32767i16, 0, -32768],
            [0, 32767, -32767],
            [-32768, 32767, 0],
        ] {
            bytes.extend_from_slice(&x.to_le_bytes());
            bytes.extend_from_slice(&y.to_le_bytes());
            bytes.extend_from_slice(&z.to_le_bytes());
            bytes.extend_from_slice(&0i16.to_le_bytes());
        }
        let blob = bytes.iter().map(|b| format!("{b:02X}")).collect::<String>();
        let index_hex = [0u16, 1, 2]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .map(|b| format!("{b:02X}"))
            .collect::<String>();
        let xml = format!(
            r#"<Root xmlns:d="http://www.kuju.com/TnT/2003/Delta">
                <cHcVertexBuffer>
                    <cHcVertexTypeDeclarationDx>
                        <ElementDecl>
                            <Usage d:type="cDeltaString">POSITION</Usage>
                            <Type d:type="cDeltaString">SHORT4N</Type>
                            <StreamIndex d:type="sUInt32">0</StreamIndex>
                            <Offset d:type="sUInt32">0</Offset>
                        </ElementDecl>
                    </cHcVertexTypeDeclarationDx>
                    <VertexData><blob>{blob}</blob></VertexData>
                </cHcVertexBuffer>
                <cHcIndexBuffer>
                    <IndexSize d:type="sUInt32">2</IndexSize>
                    <IndexData><blob>{index_hex}</blob></IndexData>
                </cHcIndexBuffer>
                <TextureByName><e d:type="cDeltaString">Textures\sample.TgPcDx</e></TextureByName>
            </Root>"#
        );
        let model = GeoModel::parse(Path::new(""), xml.as_bytes()).unwrap();
        assert_eq!(model.vertices.len(), 3);
        assert_eq!(model.vertices[0].position, [1.0f32, 0.0, -1.0]);
        assert!((model.vertices[1].position[0] - 0.0f32).abs() < 1e-5);
        assert!((model.vertices[1].position[1] - 1.0f32).abs() < 1e-5);
        assert!((model.vertices[1].position[2] + 1.0f32).abs() < 1e-5);
        assert!((model.vertices[2].position[0] + 1.0f32).abs() < 1e-5);
        assert!((model.vertices[2].position[1] - 1.0f32).abs() < 1e-5);
        assert!((model.vertices[2].position[2] - 0.0f32).abs() < 1e-5);
    }

    #[test]
    fn alpha_test_always_does_not_enable_cutout() {
        let xml = r#"
            <Root xmlns:d="http://www.kuju.com/TnT/2003/Delta">
                <cHcEffectMaterialDx d:id="7">
                    <ShaderName d:type="cDeltaString">TrainBumpSpec.fx</ShaderName>
                    <AlphaTestType d:type="cDeltaString">HC_ALPHATEST_ALWAYS</AlphaTestType>
                    <FrameAlphaMode d:type="cDeltaString">ALPHA_MODE_NONE</FrameAlphaMode>
                    <TextureByName d:type="cDeltaString">Textures\body.TgPcDx</TextureByName>
                </cHcEffectMaterialDx>
            </Root>"#;
        let root = SerzNode::from_xml_str(xml).unwrap().0;
        let bindings = find_material_bindings(
            &root,
            &[r"Textures\body.TgPcDx".to_owned()],
            &build_id_map(&root),
        );
        assert_eq!(bindings[&7].render_mode, GeoRenderMode::Opaque);
        // Diffuse alpha is a specular/environment mask: no cutout threshold applied.
        assert_eq!(bindings[&7].alpha_ref, 0.5);
    }

    #[test]
    fn alpha_test_greater_enables_cutout_with_alpha_ref() {
        let xml = r#"
            <Root xmlns:d="http://www.kuju.com/TnT/2003/Delta">
                <cHcEffectMaterialDx d:id="7">
                    <ShaderName d:type="cDeltaString">TrainBasic.fx</ShaderName>
                    <AlphaTestType d:type="cDeltaString">HC_ALPHATEST_GREATER</AlphaTestType>
                    <AlphaRef d:type="sUInt32">128</AlphaRef>
                    <TextureByName d:type="cDeltaString">Textures\body.TgPcDx</TextureByName>
                </cHcEffectMaterialDx>
            </Root>"#;
        let root = SerzNode::from_xml_str(xml).unwrap().0;
        let bindings = find_material_bindings(
            &root,
            &[r"Textures\body.TgPcDx".to_owned()],
            &build_id_map(&root),
        );
        assert_eq!(bindings[&7].render_mode, GeoRenderMode::Cutout);
        assert!((bindings[&7].alpha_ref - 0.5).abs() < 0.01);
    }

    #[test]
    fn base_vertex_applies_to_the_batch_vertex_buffer_not_buffer_zero() {
        let mut buf0 = Vec::new();
        for pos in [[0.0f32, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]] {
            for c in pos {
                buf0.extend_from_slice(&c.to_le_bytes());
            }
            buf0.extend_from_slice(&[0u8; 20]);
        }
        assert_eq!(buf0.len(), 96);
        let mut buf1 = Vec::new();
        for pos in [[10.0f32, 0.0, 0.0], [0.0, 10.0, 0.0], [0.0, 0.0, 10.0]] {
            for c in pos {
                buf1.extend_from_slice(&c.to_le_bytes());
            }
            buf1.extend_from_slice(&[0u8; 20]);
        }
        assert_eq!(buf1.len(), 96);
        let blob0 = buf0.iter().map(|b| format!("{b:02X}")).collect::<String>();
        let blob1 = buf1.iter().map(|b| format!("{b:02X}")).collect::<String>();
        let index_hex = [0u16, 1, 2]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .map(|b| format!("{b:02X}"))
            .collect::<String>();
        let xml = format!(
            r#"<Root xmlns:d="http://www.kuju.com/TnT/2003/Delta">
                <cHcVertexBuffer>
                    <VertexStride d:type="sUInt32">32</VertexStride>
                    <NumVertices d:type="sUInt32">3</NumVertices>
                    <VertexData><blob>{blob0}</blob></VertexData>
                </cHcVertexBuffer>
                <cHcVertexBuffer>
                    <VertexStride d:type="sUInt32">32</VertexStride>
                    <NumVertices d:type="sUInt32">2</NumVertices>
                    <VertexData><blob>{blob1}</blob></VertexData>
                </cHcVertexBuffer>
                <cHcIndexBuffer>
                    <IndexSize d:type="sUInt32">2</IndexSize>
                    <IndexData><blob>{index_hex}</blob></IndexData>
                </cHcIndexBuffer>
                <cHcPrimitiveList>
                    <StartIndex d:type="sUInt32">0</StartIndex>
                    <IndexCount d:type="sUInt32">3</IndexCount>
                    <VertexBufferIndex d:type="sUInt32">1</VertexBufferIndex>
                    <VertexIndexBase d:type="sUInt32">0</VertexIndexBase>
                </cHcPrimitiveList>
                <TextureByName><e d:type="cDeltaString">Textures\sample.TgPcDx</e></TextureByName>
            </Root>"#
        );
        let model = GeoModel::parse(Path::new(""), xml.as_bytes()).unwrap();
        assert_eq!(model.vertices.len(), 6);
        assert_eq!(model.indices, [3, 4, 5]);
        assert_eq!(model.vertices[3].position, [10.0, 0.0, 0.0]);
        assert_eq!(model.vertices[4].position, [0.0, 10.0, 0.0]);
        assert_eq!(model.vertices[5].position, [0.0, 0.0, 10.0]);
    }

    #[test]
    fn batch_binds_declared_vertex_buffer_slot_when_a_buffer_is_skipped() {
        fn blob(positions: [[f32; 3]; 3]) -> String {
            positions
                .iter()
                .flat_map(|pos| {
                    let mut record = pos.iter().flat_map(|c| c.to_le_bytes()).collect::<Vec<_>>();
                    record.extend_from_slice(&[0u8; 20]);
                    record
                })
                .map(|b| format!("{b:02X}"))
                .collect()
        }
        let slot_zero = blob([[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]]);
        let slot_two = blob([[10.0, 0.0, 0.0], [0.0, 10.0, 0.0], [0.0, 0.0, 10.0]]);
        let index_hex = [0u16, 1, 2]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .map(|b| format!("{b:02X}"))
            .collect::<String>();
        let xml = format!(
            r#"<Root xmlns:d="http://www.kuju.com/TnT/2003/Delta">
                <cHcVertexBuffer>
                    <VertexStride d:type="sUInt32">32</VertexStride>
                    <NumVertices d:type="sUInt32">3</NumVertices>
                    <VertexData><blob>{slot_zero}</blob></VertexData>
                </cHcVertexBuffer>
                <cHcVertexBuffer>
                    <VertexStride d:type="sUInt32">32</VertexStride>
                </cHcVertexBuffer>
                <cHcVertexBuffer>
                    <VertexStride d:type="sUInt32">32</VertexStride>
                    <NumVertices d:type="sUInt32">3</NumVertices>
                    <VertexData><blob>{slot_two}</blob></VertexData>
                </cHcVertexBuffer>
                <cHcIndexBuffer>
                    <IndexSize d:type="sUInt32">2</IndexSize>
                    <IndexData><blob>{index_hex}</blob></IndexData>
                </cHcIndexBuffer>
                <cHcPrimitiveList>
                    <StartIndex d:type="sUInt32">0</StartIndex>
                    <IndexCount d:type="sUInt32">3</IndexCount>
                    <VertexBufferIndex d:type="sUInt32">2</VertexBufferIndex>
                    <VertexIndexBase d:type="sUInt32">0</VertexIndexBase>
                </cHcPrimitiveList>
                <TextureByName><e d:type="cDeltaString">Textures\sample.TgPcDx</e></TextureByName>
            </Root>"#
        );
        let model = GeoModel::parse(Path::new(""), xml.as_bytes()).unwrap();
        assert_eq!(model.vertices.len(), 6);
        assert_eq!(model.indices, [3, 4, 5]);
        assert_eq!(model.vertices[3].position, [10.0, 0.0, 0.0]);
    }
}
