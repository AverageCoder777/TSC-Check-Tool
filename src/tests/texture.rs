use super::*;
use crate::serz::SerzPreamble;
use crate::texture::DdsCompression;
use std::fs;
use std::io::Cursor;
use std::path::Path;
use tempfile;

#[test]
fn round_trip_tgpcdx_dds() {
    let dir = tempfile::tempdir().unwrap();
    let dds_path = dir.path().join("src.dds");
    let dds = ddsfile::Dds::new_d3d(ddsfile::NewD3dParams {
        height: 8,
        width: 8,
        depth: None,
        format: ddsfile::D3DFormat::DXT5,
        mipmap_levels: Some(3),
        caps2: None,
    })
    .unwrap();
    let mut f = fs::File::create(&dds_path).unwrap();
    dds.write(&mut f).unwrap();

    let tg_path = dir.path().join("src.TgPcDx");
    convert_dds_to_tgpcdx(&dds_path, &tg_path).unwrap();
    assert!(tg_path.is_file());
    let serialized = fs::read(&tg_path).unwrap();
    assert_eq!(&serialized[..10], b"SERZ\0\0\x01\0\xFF\x43");

    // Preserve a custom Name in the target before re-importing.
    let data = fs::read(&tg_path).unwrap();
    let mut reader = crate::serz::SerzReader::new(&data);
    let mut root = reader.parse().unwrap();
    let preamble = reader.preamble.clone();
    let tex = find_element_mut(&mut root, "cHcTexture").unwrap();
    assert!(set_property_value(
        tex,
        "Name",
        "TrainSimContent\\Textures\\Sample"
    ));
    let patched = crate::serz::encode_to_vec(&root, preamble.as_ref()).unwrap();
    fs::write(&tg_path, &patched).unwrap();

    let out_dds = dir.path().join("out.dds");
    export_tgpcdx_to_dds(&tg_path, &out_dds).unwrap();
    let data = fs::read(&out_dds).unwrap();
    assert_eq!(&data[0..4], b"DDS ");

    let dds2 = ddsfile::Dds::read(Cursor::new(&data)).unwrap();
    assert_eq!(dds2.get_width(), 8);
    assert_eq!(dds2.get_height(), 8);
    assert_eq!(dds2.get_num_mipmap_levels(), 3);

    // Import back into the same target file, preserving metadata.
    convert_dds_to_tgpcdx(&out_dds, &tg_path).unwrap();
    assert!(tg_path.is_file());

    let data = fs::read(&tg_path).unwrap();
    let xml = serz_bytes_to_xml(&data).unwrap();
    assert!(
        xml.contains(r#"<Name d:type="cDeltaString">TrainSimContent\Textures\Sample</Name>"#)
    );
    assert!(xml.contains("HC_IMAGE_FORMAT_COMPRESSED_INTERP_ALPHA"));

    let blocks = all_image_blocks(&xml);
    assert_eq!(blocks.len(), 3);
    let ws: Vec<u32> = blocks
        .iter()
        .map(|b| extract_tag(b, "Width", "sUInt16").unwrap().parse().unwrap())
        .collect();
    assert_eq!(ws, vec![8, 4, 2]);
}

#[test]
fn dds_button_import_rebuilds_binary_tgpcdx_and_preview() {
    let dir = tempfile::tempdir().unwrap();
    let target_path = dir.path().join("texture.TgPcDx");
    let original_dds_path = dir.path().join("original.dds");
    let mut original_dds = ddsfile::Dds::new_d3d(ddsfile::NewD3dParams {
        height: 8,
        width: 8,
        depth: None,
        format: ddsfile::D3DFormat::DXT1,
        mipmap_levels: Some(4),
        caps2: None,
    })
    .unwrap();
    for (index, byte) in original_dds.data.iter_mut().enumerate() {
        *byte = (index as u8).wrapping_mul(11).wrapping_add(3);
    }
    let mut original_file = fs::File::create(&original_dds_path).unwrap();
    original_dds.write(&mut original_file).unwrap();

    convert_dds_to_tgpcdx(&original_dds_path, &target_path).unwrap();

    let input_dds_path = dir.path().join("replacement.dds");
    let mut input_dds = ddsfile::Dds::new_d3d(ddsfile::NewD3dParams {
        height: 16,
        width: 16,
        depth: None,
        format: ddsfile::D3DFormat::DXT5,
        mipmap_levels: Some(3),
        caps2: None,
    })
    .unwrap();
    for (index, byte) in input_dds.data.iter_mut().enumerate() {
        *byte = (index as u8).wrapping_mul(29).wrapping_add(0xA7);
    }
    let expected_data = input_dds.data.clone();
    let mut input_file = fs::File::create(&input_dds_path).unwrap();
    input_dds.write(&mut input_file).unwrap();
    let dds_file_bytes = fs::read(&input_dds_path).unwrap();
    assert_eq!(&dds_file_bytes[..4], b"DDS ");
    assert_eq!(&dds_file_bytes[128..], expected_data);

    // This is the import function called by TextureController's DDS button.
    let report = convert_dds_to_tgpcdx_report(&input_dds_path, &target_path).unwrap();
    let imported = fs::read(&target_path).unwrap();
    assert_eq!(&imported[..10], b"SERZ\0\0\x01\0\xFF\x43");

    let imported_mips = image_mips_from_bytes(&imported).unwrap();
    assert_eq!(
        imported_mips
            .iter()
            .map(|mip| (mip.width, mip.height))
            .collect::<Vec<_>>(),
        vec![(16, 16), (8, 8), (4, 4)]
    );
    let imported_data: Vec<u8> = imported_mips
        .iter()
        .flat_map(|mip| mip.data.iter().copied())
        .collect();
    assert_eq!(imported_data, expected_data);
    assert_eq!(report.total_bytes, expected_data.len());
    assert_eq!(imported_data.len(), report.total_bytes);

    let (width, height, rgba) = decode_tgpcdx_to_rgba(&imported).unwrap();
    assert_eq!((width, height), (16, 16));
    assert_eq!(rgba.len(), 16 * 16 * 4);
}

#[test]
fn dds_import_expands_single_blob_to_declared_mip_nodes() {
    let dir = tempfile::tempdir().unwrap();
    let target_path = dir.path().join("texture.TgPcDx");
    let original_dds_path = dir.path().join("original.dds");
    let original_dds = ddsfile::Dds::new_d3d(ddsfile::NewD3dParams {
        height: 16,
        width: 16,
        depth: None,
        format: ddsfile::D3DFormat::DXT5,
        mipmap_levels: Some(1),
        caps2: None,
    })
    .unwrap();
    let mut original_file = fs::File::create(&original_dds_path).unwrap();
    original_dds.write(&mut original_file).unwrap();
    convert_dds_to_tgpcdx(&original_dds_path, &target_path).unwrap();
    assert_eq!(
        image_mips_from_bytes(&fs::read(&target_path).unwrap())
            .unwrap()
            .len(),
        1
    );

    let input_dds_path = dir.path().join("replacement.dds");
    let mut input_dds = ddsfile::Dds::new_d3d(ddsfile::NewD3dParams {
        height: 16,
        width: 16,
        depth: None,
        format: ddsfile::D3DFormat::DXT5,
        mipmap_levels: Some(3),
        caps2: None,
    })
    .unwrap();
    for (index, byte) in input_dds.data.iter_mut().enumerate() {
        *byte = (index as u8).wrapping_mul(17).wrapping_add(0x5B);
    }
    let declared_mip_count = input_dds.get_num_mipmap_levels() as usize;
    let mut input_file = fs::File::create(&input_dds_path).unwrap();
    input_dds.write(&mut input_file).unwrap();

    let report = convert_dds_to_tgpcdx_report(&input_dds_path, &target_path).unwrap();
    let imported = fs::read(&target_path).unwrap();
    let imported_mips = image_mips_from_bytes(&imported).unwrap();
    assert_eq!(report.mip_count as usize, declared_mip_count);
    assert_eq!(imported_mips.len(), declared_mip_count);

    let xml = serz_bytes_to_xml(&imported).unwrap();
    let image_blocks = all_image_blocks(&xml);
    assert_eq!(image_blocks.len(), declared_mip_count);
    let declared_blob_sizes = image_blocks
        .iter()
        .map(|block| extract_blob_size(block) as usize)
        .collect::<Vec<_>>();
    let actual_blob_sizes = imported_mips
        .iter()
        .map(|mip| mip.data.len())
        .collect::<Vec<_>>();
    assert_eq!(declared_blob_sizes, actual_blob_sizes);
}

#[test]
fn test_mip_chain_sizes_dxt1_dxt5_1024_to_1() {
    const DXT1_TOTAL: usize = 699_064;
    const DXT5_TOTAL: usize = 1_398_128;

    for (fmt, block_bytes, expected_total) in [
        (DdsCompression::Dxt1, 8usize, DXT1_TOTAL),
        (DdsCompression::Dxt5, 16usize, DXT5_TOTAL),
    ] {
        let mut total = 0usize;
        let mut levels = 0u32;
        let (mut w, mut h) = (1024u32, 1024u32);
        loop {
            let size = mip_level_bytes(w, h, fmt).unwrap();
            total += size;
            levels += 1;
            if w == 1 && h == 1 {
                break;
            }
            w = (w / 2).max(1);
            h = (h / 2).max(1);
        }
        assert_eq!(
            levels, 11,
            "{fmt:?} должен содержать 11 уровней для 1024x1024"
        );
        assert_eq!(
            total, expected_total,
            "{fmt:?} неверный суммарный размер цепочки"
        );
        assert_eq!(
            mip_level_bytes(1024, 1024, fmt),
            Some(256 * 256 * block_bytes)
        );
        assert_eq!(mip_level_bytes(2, 2, fmt), Some(block_bytes));
        assert_eq!(mip_level_bytes(1, 1, fmt), Some(block_bytes));

        // Валидация принимает точную цепочку мипмапов.
        let info = validate_dds_mip_chain(1024, 1024, fmt, 11, total, Some(11)).unwrap();
        assert_eq!(info.levels, 11);
        assert_eq!(info.total_bytes, total);
    }
}

#[test]
fn test_validate_dds_rejects_invalid_chains() {
    // Обрезанный буфер данных.
    assert!(
        validate_dds_mip_chain(1024, 1024, DdsCompression::Dxt1, 11, 100, Some(11)).is_err()
    );
    // Лишние данные после цепочки мипмапов.
    assert!(
        validate_dds_mip_chain(1024, 1024, DdsCompression::Dxt1, 11, 699_064 + 1, Some(11),)
            .is_err()
    );
    // Мипмапов меньше, чем в исходном .TgPcDx.
    assert!(
        validate_dds_mip_chain(1024, 1024, DdsCompression::Dxt1, 1, 699_064, Some(11)).is_err()
    );
    // Без ожидания от оригинала один уровень допустим.
    let one = mip_level_bytes(1024, 1024, DdsCompression::Dxt1).unwrap();
    assert!(validate_dds_mip_chain(1024, 1024, DdsCompression::Dxt1, 1, one, None).is_ok());
    // Неизвестный формат отвергается.
    assert!(validate_dds_mip_chain(1024, 1024, DdsCompression::Unknown, 1, 0, None).is_err());
}

#[test]
fn test_dds_format_mismatch_messages() {
    let original = TextureInfo {
        name: "tex".into(),
        is_swizzled: false,
        width: 1024,
        height: 1024,
        mip_count: 11,
        min_width: 1,
        min_height: 1,
        format: "HC_IMAGE_FORMAT_COMPRESSED".into(),
        raw_format: "HC_IMAGE_FORMAT_COMPRESSED".into(),
        format_label: "DXT1 (BC1, без альфы / 1-bit альфа)".into(),
        dds_format: Some("DXT1"),
        texture_count: 1,
        mip_storage_mode: "Единый упакованный блоб (1× cHcImageDx)".into(),
        flags_u16: 0,
        flags: 0,
        unnamed_u16: 0,
        image_flags: 0,
        pre_blob_u16: 0,
        file_size_bytes: 0,
        total_blob_bytes: 0,
        header_bytes: 0,
        name_matches_filename: false,
        has_bak: false,
        bak_size_bytes: 0,
        backup_size_bytes: 0,
    };
    let expected = tgpcdx_format_to_dds_name(&original.format).unwrap();
    let actual = DdsCompression::Dxt5.as_dds_name();
    assert_ne!(expected, actual);
    assert_eq!(
        format!("Ожидался формат {expected}, но выбранный DDS имеет формат {actual}"),
        "Ожидался формат DXT1, но выбранный DDS имеет формат DXT5"
    );

    // Нестепень двойки отвергается.
    assert!(!700u32.is_power_of_two());
    assert!(1024u32.is_power_of_two());
}

#[test]
fn test_texture_info_text_renders_chain() {
    let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<cHcTextureGroup xmlns:d="http://www.kuju.com/TnT/2003/Delta" d:version="1.0" d:id="1">
<Texture>
<cHcTexture d:id="2">
<Width d:type="sUInt32">1024</Width>
<Height d:type="sUInt32">1024</Height>
<Name d:type="cDeltaString">TrainSimContent\Textures\Sample</Name>
<Data>
<cHcTextureData d:id="3">
<Mip>
<cHcImageDx d:id="4">
<Format d:type="cDeltaString">HC_IMAGE_FORMAT_COMPRESSED_INTERP_ALPHA</Format>
<IsSwizzled d:type="bool">0</IsSwizzled>
<Width d:type="sUInt16">1024</Width>
<Height d:type="sUInt16">1024</Height>
<d:blob d:size="4">0000</d:blob>
</cHcImageDx>
</Mip>
</cHcTextureData>
</Data>
</cHcTexture>
</Texture>
</cHcTextureGroup>"#;
    let info = inspect_tgpcdx(xml.as_bytes()).unwrap();
    assert_eq!(info.name, r#"TrainSimContent\Textures\Sample"#);
    assert_eq!(info.width, 1024);
    assert_eq!(info.height, 1024);
    assert_eq!(info.mip_count, 1);
    assert_eq!(info.format_label, "DXT5 (BC3, плавная альфа)");
    assert_eq!(info.dds_format, Some("DXT5"));

    let text = texture_info_text(&info, Path::new("x\\y\\tex.TgPcDx"), false);
    assert!(text.contains("Внутреннее имя (Name): TrainSimContent\\Textures\\Sample"));
    assert!(text.contains("Разрешение: 1024×1024 px | Текстур в группе: 1 | Мипмапов: 1"));
    assert!(text.contains("Формат: DXT5 (BC3, плавная альфа) [HC_IMAGE_FORMAT_COMPRESSED_INTERP_ALPHA] | DDS: DXT5"));
    assert!(text.contains("IsSwizzled: 0"));
    assert!(text.contains("Flags(u16): 0"));
    assert!(text.contains(
        "Для импорта требуется: DDS в формате DXT5, размеры степень двойки, с мипмапами"
    ));
    assert!(text.contains("TGPCDX  x\\y\\tex.TgPcDx"));
}

#[test]
fn preserves_serz_prefix_and_tail_on_reimport() {
    use std::fs;
    let dir = tempfile::tempdir().unwrap();
    let dds_path = dir.path().join("src.dds");
    let mut dds = ddsfile::Dds::new_d3d(ddsfile::NewD3dParams {
        height: 16,
        width: 16,
        depth: None,
        format: ddsfile::D3DFormat::DXT1,
        mipmap_levels: Some(5),
        caps2: None,
    })
    .unwrap();
    for (i, b) in dds.data.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(37).wrapping_add(11);
    }
    let mut f = fs::File::create(&dds_path).unwrap();
    dds.write(&mut f).unwrap();

    let tg_path = dir.path().join("src.TgPcDx");
    convert_dds_to_tgpcdx(&dds_path, &tg_path).unwrap();
    let orig = fs::read(&tg_path).unwrap();
    let original_blob: Vec<u8> = image_mips_from_bytes(&orig)
        .unwrap()
        .into_iter()
        .flat_map(|mip| mip.data)
        .collect();
    assert_eq!(original_blob, dds.data);

    // Modify only the first mip in an exported DDS, then reimport.
    let out_dds = dir.path().join("out.dds");
    export_tgpcdx_to_dds(&tg_path, &out_dds).unwrap();
    let mut dds2 = fs::read(&out_dds).unwrap();
    for b in &mut dds2[128..256] {
        *b = b.wrapping_add(1);
    }
    fs::write(&out_dds, &dds2).unwrap();

    let tg2_path = dir.path().join("dst.TgPcDx");
    fs::copy(&tg_path, &tg2_path).unwrap();
    convert_dds_to_tgpcdx(&out_dds, &tg2_path).unwrap();
    let new = fs::read(&tg2_path).unwrap();
    let imported_blob: Vec<u8> = image_mips_from_bytes(&new)
        .unwrap()
        .into_iter()
        .flat_map(|mip| mip.data)
        .collect();
    let imported_dds = ddsfile::Dds::read(Cursor::new(&dds2)).unwrap();
    assert_eq!(imported_blob, imported_dds.get_data(0).unwrap());

    assert_eq!(new.len(), orig.len());

    let marker = [0xFFu8, 0x42];
    let pos = orig.windows(2).position(|w| w == marker).expect("marker");
    let elem_count = u32::from_le_bytes(orig[pos + 2..pos + 6].try_into().unwrap());
    let blob_start = pos;
    let blob_end = pos + 6 + (elem_count as usize) * 2;

    assert_eq!(&new[..blob_start], &orig[..blob_start]);
    assert_eq!(&new[blob_end..], &orig[blob_end..]);
    assert_ne!(&new[pos + 6..blob_end], &orig[pos + 6..blob_end]);
}

#[test]
fn update_tgpcdx_internal_name_preserves_metadata_and_blob() {
    use std::fs;
    let dir = tempfile::tempdir().unwrap();
    let dds_path = dir.path().join("src.dds");
    let mut dds = ddsfile::Dds::new_d3d(ddsfile::NewD3dParams {
        height: 16,
        width: 16,
        depth: None,
        format: ddsfile::D3DFormat::DXT1,
        mipmap_levels: Some(5),
        caps2: None,
    })
    .unwrap();
    for (i, b) in dds.data.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(37).wrapping_add(11);
    }
    let mut f = fs::File::create(&dds_path).unwrap();
    dds.write(&mut f).unwrap();

    let tg_path = dir.path().join("src.TgPcDx");
    convert_dds_to_tgpcdx(&dds_path, &tg_path).unwrap();
    let orig_bytes = fs::read(&tg_path).unwrap();
    let orig_info = inspect_tgpcdx_with_path(&orig_bytes, Some(&tg_path)).unwrap();

    fn first_blob(data: &[u8]) -> Vec<u8> {
        let xml = serz_bytes_to_xml(data).unwrap();
        let block = all_image_blocks(&xml)[0];
        hex_decode(&extract_blob(block).unwrap())
    }
    let orig_blob = first_blob(&orig_bytes);

    // Длиннее.
    update_tgpcdx_internal_name(&tg_path, "Провайдер\\Продукт\\Folder\\LongerTextureName")
        .unwrap();
    let long_bytes = fs::read(&tg_path).unwrap();
    let long_info = inspect_tgpcdx_with_path(&long_bytes, Some(&tg_path)).unwrap();
    assert_eq!(
        long_info.name,
        "Провайдер\\Продукт\\Folder\\LongerTextureName"
    );
    assert_eq!(long_info.width, orig_info.width);
    assert_eq!(long_info.height, orig_info.height);
    assert_eq!(long_info.format, orig_info.format);
    assert_eq!(long_info.is_swizzled, orig_info.is_swizzled);
    assert_eq!(long_info.total_blob_bytes, orig_info.total_blob_bytes);
    assert_eq!(first_blob(&long_bytes), orig_blob);

    // Короче.
    update_tgpcdx_internal_name(&tg_path, "X").unwrap();
    let short_bytes = fs::read(&tg_path).unwrap();
    let short_info = inspect_tgpcdx_with_path(&short_bytes, Some(&tg_path)).unwrap();
    assert_eq!(short_info.name, "X");
    assert_eq!(short_info.width, orig_info.width);
    assert_eq!(short_info.height, orig_info.height);
    assert_eq!(short_info.format, orig_info.format);
    assert_eq!(short_info.is_swizzled, orig_info.is_swizzled);
    assert_eq!(short_info.total_blob_bytes, orig_info.total_blob_bytes);
    assert_eq!(first_blob(&short_bytes), orig_blob);
}

#[test]
fn save_button_preserves_binary_blob_and_serz_markers() {
    fn collect_blobs(node: &crate::serz::SerzNode, blobs: &mut Vec<Vec<u8>>) {
        match node {
            crate::serz::SerzNode::Element { children, .. } => {
                for child in children {
                    collect_blobs(child, blobs);
                }
            }
            crate::serz::SerzNode::Blob { data } => blobs.push(data.clone()),
            _ => {}
        }
    }

    fn markers(bytes: &[u8]) -> Vec<[u8; 2]> {
        bytes
            .windows(2)
            .filter(|pair| {
                pair[0] == 0xFF && matches!(pair[1], 0x42 | 0x43 | 0x50 | 0x56 | 0x70)
            })
            .map(|pair| [pair[0], pair[1]])
            .collect()
    }

    let blob = vec![0x00, 0xFF, 0xAA, 0x80, 0x12, 0xFF, 0xAB, 0x42];
    let root = crate::serz::SerzNode::Element {
        name: "cHcTextureGroup".into(),
        id: 1,
        version: 1,
        children: vec![crate::serz::SerzNode::Element {
            name: "cHcTexture".into(),
            id: 2,
            version: 1,
            children: vec![
                crate::serz::SerzNode::Property {
                    name: "Name".into(),
                    type_name: "cDeltaString".into(),
                    value: "Provider\\Product\\Original".into(),
                    alt_encoding: None,
                },
                crate::serz::SerzNode::Element {
                    name: "cHcImageDx".into(),
                    id: 3,
                    version: 1,
                    children: vec![
                        crate::serz::SerzNode::Property {
                            name: "Format".into(),
                            type_name: "cDeltaString".into(),
                            value: "HC_IMAGE_FORMAT_COMPRESSED".into(),
                            alt_encoding: None,
                        },
                        crate::serz::SerzNode::Property {
                            name: "Width".into(),
                            type_name: "sUInt16".into(),
                            value: "4".into(),
                            alt_encoding: None,
                        },
                        crate::serz::SerzNode::Property {
                            name: "Height".into(),
                            type_name: "sUInt16".into(),
                            value: "4".into(),
                            alt_encoding: None,
                        },
                        crate::serz::SerzNode::Blob { data: blob.clone() },
                    ],
                },
            ],
        }],
    };
    let preamble = crate::serz::SerzPreamble { flag: 0, val: 0 };
    let original = crate::serz::encode_tgpcdx_to_vec(&root, Some(&preamble)).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("texture.TgPcDx");
    fs::write(&path, &original).unwrap();

    // This is the same save function invoked by TextureController's button callback.
    update_tgpcdx_internal_name(&path, "Provider\\Product\\Renamed").unwrap();

    let saved = fs::read(&path).unwrap();
    assert_eq!(&saved[..10], b"SERZ\0\0\x01\0\xFF\x43");
    assert_eq!(markers(&saved), markers(&original));

    let mut reader = crate::serz::SerzReader::new(&saved);
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