use super::*;
use crate::serz::{SerzNode, SerzPreamble};
use std::fs;
use tempfile;

fn texture_node(blob: Vec<u8>) -> SerzNode {
    SerzNode::Element {
        name: "cHcTexture".into(),
        id: 1,
        version: 1,
        children: vec![
            SerzNode::Property {
                name: "Name".into(),
                type_name: "cDeltaString".into(),
                value: "OldTexture".into(),
                alt_encoding: None,
            },
            SerzNode::Element {
                name: "cHcTextureData".into(),
                id: 2,
                version: 1,
                children: vec![SerzNode::Element {
                    name: "Mip".into(),
                    id: 3,
                    version: 1,
                    children: vec![SerzNode::Element {
                        name: "cHcImageDx".into(),
                        id: 4,
                        version: 1,
                        children: vec![SerzNode::Blob { data: blob }],
                    }],
                }],
            },
        ],
    }
}

fn find_property<'a>(node: &'a SerzNode, property_name: &str) -> Option<&'a str> {
    match node {
        SerzNode::Element { children, .. } => children
            .iter()
            .find_map(|child| find_property(child, property_name)),
        SerzNode::Property { name, value, .. } if name == property_name => Some(value),
        _ => None,
    }
}

fn collect_blobs<'a>(node: &'a SerzNode, blobs: &mut Vec<&'a [u8]>) {
    match node {
        SerzNode::Element { children, .. } => {
            for child in children {
                collect_blobs(child, blobs);
            }
        }
        SerzNode::Blob { data } => blobs.push(data),
        _ => {}
    }
}

#[test]
fn texture_editor_xml_round_trip_preserves_binary_blob() {
    let blob = vec![0xFF, 0xAA, 0x80, 0x42, 0x00, 0xFE, 0xFF, 0x01];
    let preamble = SerzPreamble {
        flag: 0x01,
        val: 0x1234_5678,
    };
    let original =
        crate::serz::encode_tgpcdx_to_vec(&texture_node(blob.clone()), Some(&preamble)).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("texture.TgPcDx");
    let temp_xml = generate_temp_xml_path(&target);
    fs::write(&target, &original).unwrap();

    let xml = resolve_temp_xml(&target, &temp_xml).unwrap();
    let edited_xml = xml.replace("OldTexture", "RenamedTexture");
    assert_ne!(xml, edited_xml);
    let saved = compile_xml_for_target(&target, &edited_xml).unwrap();
    let invalid_blob_size = edited_xml.replace(
        &format!("d:size=\"{}\"", blob.len()),
        &format!("d:size=\"{}\"", blob.len() + 1),
    );
    assert!(compile_xml_for_target(&target, &invalid_blob_size).is_err());

    assert_eq!(&saved[..10], b"SERZ\0\0\x01\0\xFF\x43");
    let root = crate::serz::validate_tgpcdx_binary(&saved, Some(&[blob.len()])).unwrap();
    assert_eq!(find_property(&root, "Name"), Some("RenamedTexture"));
    let mut saved_blobs = Vec::new();
    collect_blobs(&root, &mut saved_blobs);
    assert_eq!(saved_blobs, vec![blob.as_slice()]);
    let parsed_preamble = {
        let mut reader = crate::serz::SerzReader::new(&saved);
        reader.parse().unwrap();
        reader.preamble.unwrap()
    };
    assert_eq!(parsed_preamble.flag, 0x01);
    assert_eq!(parsed_preamble.val, 0x1234_5678);
}

#[test]
fn geometry_editor_xml_round_trip_preserves_binary_blob() {
    let blob = vec![0xFF, 0xAA, 0x80, 0x42, 0x00, 0xFE, 0xFF, 0x01];
    let preamble = SerzPreamble {
        flag: 0x01,
        val: 0x1234_5678,
    };
    let original =
        crate::serz::encode_to_vec(&texture_node(blob.clone()), Some(&preamble)).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("model.GeoPcDx");
    let temp_xml = generate_temp_xml_path(&target);
    fs::write(&target, &original).unwrap();

    let xml = resolve_temp_xml(&target, &temp_xml).unwrap();
    let edited_xml = xml.replace("OldTexture", "RenamedTexture");
    let saved = compile_xml_for_target(&target, &edited_xml).unwrap();

    let mut reader = crate::serz::SerzReader::new(&saved);
    let root = reader.parse().unwrap();
    assert_eq!(find_property(&root, "Name"), Some("RenamedTexture"));
    let mut saved_blobs = Vec::new();
    collect_blobs(&root, &mut saved_blobs);
    assert_eq!(saved_blobs, vec![blob.as_slice()]);
    assert_eq!(reader.preamble.as_ref(), Some(&preamble));
}
