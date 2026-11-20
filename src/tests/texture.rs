use super::*;
use crate::common::serz::*;
use std::fs;
use tempfile;

#[test]
fn save_button_preserves_binary_blob_and_serz_markers() {
    fn collect_blobs(node: &SerzNode, blobs: &mut Vec<Vec<u8>>) {
        match node {
            SerzNode::Element { children, .. } => {
                for child in children {
                    collect_blobs(child, blobs);
                }
            }
            SerzNode::Blob { data } => blobs.push(data.clone()),
            _ => {}
        }
    }

    fn markers(bytes: &[u8]) -> Vec<[u8; 2]> {
        bytes
            .windows(2)
            .filter(|pair| pair[0] == 0xFF && matches!(pair[1], 0x42 | 0x43 | 0x50 | 0x56 | 0x70))
            .map(|pair| [pair[0], pair[1]])
            .collect()
    }

    let blob = vec![0x00, 0xFF, 0xAA, 0x80, 0x12, 0xFF, 0xAB, 0x42];
    let root = SerzNode::Element {
        name: "cHcTextureGroup".into(),
        id: 1,
        version: 1,
        children: vec![SerzNode::Element {
            name: "cHcTexture".into(),
            id: 2,
            version: 1,
            children: vec![
                SerzNode::Property {
                    name: "Name".into(),
                    type_name: "cDeltaString".into(),
                    value: "Provider\\Product\\Original".into(),
                    alt_encoding: None,
                },
                SerzNode::Element {
                    name: "cHcImageDx".into(),
                    id: 3,
                    version: 1,
                    children: vec![
                        SerzNode::Property {
                            name: "Format".into(),
                            type_name: "cDeltaString".into(),
                            value: "HC_IMAGE_FORMAT_COMPRESSED".into(),
                            alt_encoding: None,
                        },
                        SerzNode::Property {
                            name: "Width".into(),
                            type_name: "sUInt16".into(),
                            value: "4".into(),
                            alt_encoding: None,
                        },
                        SerzNode::Property {
                            name: "Height".into(),
                            type_name: "sUInt16".into(),
                            value: "4".into(),
                            alt_encoding: None,
                        },
                        SerzNode::Blob { data: blob.clone() },
                    ],
                },
            ],
        }],
    };
    let preamble = SerzPreamble { flag: 0, val: 0 };
    let original = encode_tgpcdx_to_vec(&root, Some(&preamble)).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("texture.TgPcDx");
    fs::write(&path, &original).unwrap();

    // This is the same save function invoked by TextureController's button callback.
    update_tgpcdx_internal_name(&path, "Provider\\Product\\Renamed").unwrap();

    let saved = fs::read(&path).unwrap();
    assert_eq!(&saved[..10], b"SERZ\0\0\x01\0\xFF\x43");
    assert_eq!(markers(&saved), markers(&original));

    let mut reader = SerzReader::new(&saved);
    let saved_tree = reader.parse().unwrap();
    let mut saved_blobs = Vec::new();
    collect_blobs(&saved_tree, &mut saved_blobs);
    assert_eq!(saved_blobs, vec![blob]);
}

#[test]
fn update_tgpcdx_internal_name_rejects_empty() {
    let dir = tempfile::tempdir().unwrap();
    let tg_path = dir.path().join("x.TgPcDx");
    std::fs::write(&tg_path, b"").unwrap();
    assert!(update_tgpcdx_internal_name(&tg_path, "   ").is_err());
}

#[test]
fn update_tgpcdx_internal_name_rejects_xml_input() {
    let dir = tempfile::tempdir().unwrap();
    let tg_path = dir.path().join("x.TgPcDx");
    let xml = b"<cHcTexture><Name d:type=\"cDeltaString\">Old</Name></cHcTexture>";
    std::fs::write(&tg_path, xml).unwrap();

    assert!(update_tgpcdx_internal_name(&tg_path, "New").is_err());
    assert_eq!(std::fs::read(&tg_path).unwrap(), xml);
}
