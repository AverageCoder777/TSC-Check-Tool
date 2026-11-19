use super::*;
use std::path::Path;
use tempfile;

fn write_triangle_geometry(path: &Path, texture: &str) {
    let mut vertex_bytes = Vec::new();
    for position in [[0.0_f32, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]] {
        for value in position
            .into_iter()
            .chain([0.0, 0.0, 1.0])
            .chain([0.0, 0.0])
        {
            vertex_bytes.extend_from_slice(&value.to_le_bytes());
        }
    }
    let vertex_hex = vertex_bytes
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<String>();
    let index_hex = [0_u16, 1, 2]
        .into_iter()
        .flat_map(u16::to_le_bytes)
        .map(|byte| format!("{byte:02X}"))
        .collect::<String>();
    let xml = format!(
        r#"<Root xmlns:d="http://www.kuju.com/TnT/2003/Delta">
            <cHcVertexBuffer>
                <VertexStride d:type="sUInt32">32</VertexStride>
                <VertexData><blob>{vertex_hex}</blob></VertexData>
            </cHcVertexBuffer>
            <cHcIndexBuffer>
                <IndexSize d:type="sUInt32">2</IndexSize>
                <IndexData><blob>{index_hex}</blob></IndexData>
            </cHcIndexBuffer>
            <TextureByName><e d:type="cDeltaString">{texture}</e></TextureByName>
        </Root>"#
    );
    std::fs::write(path, xml).unwrap();
}

#[test]
fn loads_only_the_selected_geopcdx_without_blueprint_children() {
    let directory = tempfile::tempdir().unwrap();
    let geometry_path = directory.path().join("Main.GeoPcDx");
    let child_geometry_path = directory.path().join("Bogie.GeoPcDx");
    write_triangle_geometry(&geometry_path, r"Textures\main.TgPcDx");
    write_triangle_geometry(&child_geometry_path, r"Textures\bogie.TgPcDx");
    std::fs::write(
        directory.path().join("Main.bin"),
        r#"<Root xmlns:d="http://www.kuju.com/TnT/2003/Delta">
            <cWagonBlueprint>
                <GeometryID d:type="cDeltaString">Main.GeoPcDx</GeometryID>
                <ContainerComponent><Children>
                    <cEntityContainerBlueprint-sChild>
                        <BlueprintID d:type="cDeltaString">Bogie.xml</BlueprintID>
                    </cEntityContainerBlueprint-sChild>
                </Children></ContainerComponent>
            </cWagonBlueprint>
        </Root>"#,
    )
    .unwrap();
    std::fs::write(
        directory.path().join("Bogie.bin"),
        r#"<Root><cEngineBlueprint>
            <GeometryID d:type="cDeltaString">Bogie.GeoPcDx</GeometryID>
        </cEngineBlueprint></Root>"#,
    )
    .unwrap();

    let loaded = LoadedModel::load(&geometry_path).unwrap();

    assert_eq!(loaded.model.vertices.len(), 3);
    assert_eq!(loaded.model.indices.len(), 3);
    assert_eq!(loaded.model.textures, [r"Textures\main.TgPcDx"]);
    assert_eq!(
        loaded.stats(),
        "3 vertices  •  1 / 1 triangles  •  1 / 1 draw batches  •  1 textures"
    );
}
