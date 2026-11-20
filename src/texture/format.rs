//! Texture format handling and mip size calculations.

/// Сжатие DDS, которое движок TS Classic ожидает от импортируемого файла.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DdsCompression {
    Dxt1,
    Dxt3,
    Dxt5,
    Rgba8888,
    Bgra8888,
    L8,
    A8,
    Unknown,
}

impl DdsCompression {
    pub fn as_dds_name(self) -> &'static str {
        match self {
            DdsCompression::Dxt1 => "DXT1",
            DdsCompression::Dxt3 => "DXT3",
            DdsCompression::Dxt5 => "DXT5",
            DdsCompression::Rgba8888 | DdsCompression::Bgra8888 => "RGBA8888",
            DdsCompression::L8 => "L8",
            DdsCompression::A8 => "A8",
            DdsCompression::Unknown => "неизвестный формат",
        }
    }
}

/// Распознаёт формат сжатия по заголовку DDS_HEADER файла .dds.
pub fn dds_compression_of(dds: &ddsfile::Dds) -> DdsCompression {
    if let Some(dxgi) = dds.get_dxgi_format() {
        let s = format!("{dxgi:?}");
        if s.contains("BC3") {
            return DdsCompression::Dxt5;
        }
        if s.contains("BC2") {
            return DdsCompression::Dxt3;
        }
        if s.contains("BC1") {
            return DdsCompression::Dxt1;
        }
        if s.contains("B8G8R8A8") || s.contains("B8G8R8X8") {
            return DdsCompression::Bgra8888;
        }
        if s.contains("R8G8B8A8") || s.contains("R8G8B8X8") {
            return DdsCompression::Rgba8888;
        }
        if s.contains("A8_UNORM") {
            return DdsCompression::A8;
        }
        if s.contains("R8_UNORM") || s.contains("L8_UNORM") {
            return DdsCompression::L8;
        }
        return DdsCompression::Unknown;
    }
    if let Some(d3d) = dds.get_d3d_format() {
        return match d3d {
            ddsfile::D3DFormat::DXT5 | ddsfile::D3DFormat::DXT4 => DdsCompression::Dxt5,
            ddsfile::D3DFormat::DXT3 | ddsfile::D3DFormat::DXT2 => DdsCompression::Dxt3,
            ddsfile::D3DFormat::DXT1 => DdsCompression::Dxt1,
            ddsfile::D3DFormat::A8R8G8B8
            | ddsfile::D3DFormat::X8R8G8B8
            | ddsfile::D3DFormat::A8B8G8R8
            | ddsfile::D3DFormat::X8B8G8R8 => DdsCompression::Bgra8888,
            ddsfile::D3DFormat::L8 => DdsCompression::L8,
            ddsfile::D3DFormat::A8 => DdsCompression::A8,
            _ => DdsCompression::Unknown,
        };
    }
    if let Some(ref fourcc) = dds.header.spf.fourcc {
        let s = format!("{fourcc:?}");
        if s.contains("DXT5") {
            return DdsCompression::Dxt5;
        }
        if s.contains("DXT3") {
            return DdsCompression::Dxt3;
        }
        if s.contains("DXT1") {
            return DdsCompression::Dxt1;
        }
    }
    if dds.header.spf.fourcc.is_none()
        && let Some(bpp) = dds.header.spf.rgb_bit_count
        && bpp == 32
    {
        return DdsCompression::Rgba8888;
    }
    DdsCompression::Unknown
}

/// (человекочитаемая метка, эквивалент в DDS) для тега `Format` из .TgPcDx.
pub fn format_label_and_dds_name(fmt: &str) -> (String, Option<&'static str>) {
    let u = fmt.to_uppercase();
    if u.contains("INTERP_ALPHA") || u.contains("DXT5") || u.contains("BC3") {
        return ("DXT5 (BC3, плавная альфа)".into(), Some("DXT5"));
    }
    if u.contains("EXPL_ALPHA") || u.contains("DXT3") || u.contains("BC2") {
        return ("DXT3 (BC2, явная альфа)".into(), Some("DXT3"));
    }
    if u.contains("DXT1") || u.contains("BC1") || u.contains("COMPRESSED") {
        return ("DXT1 (BC1, без альфы / 1-bit альфа)".into(), Some("DXT1"));
    }
    if u.contains("8888")
        || u.contains("ARGB")
        || u.contains("RGBA")
        || u.contains("BGRA")
        || u.contains("UNCOMPRESSED")
    {
        return ("RGBA8888 (без сжатия)".into(), Some("RGBA8888"));
    }
    if u.contains("L8") || u.contains("A8") {
        return ("L8 / A8 (8-bit градации серого)".into(), {
            if u.contains("L8") {
                Some("L8")
            } else {
                Some("A8")
            }
        });
    }
    (fmt.to_owned(), None)
}

/// Ожидаемый размер одного уровня мипмапа в байтах.
pub fn mip_level_bytes(w: u32, h: u32, fmt: DdsCompression) -> Option<usize> {
    let bw = w.div_ceil(4).max(1) as usize;
    let bh = h.div_ceil(4).max(1) as usize;
    match fmt {
        DdsCompression::Dxt1 => Some(bw * bh * 8),
        DdsCompression::Dxt3 | DdsCompression::Dxt5 => Some(bw * bh * 16),
        DdsCompression::Rgba8888 | DdsCompression::Bgra8888 => {
            Some((w as usize) * (h as usize) * 4)
        }
        DdsCompression::L8 | DdsCompression::A8 => Some((w as usize) * (h as usize)),
        DdsCompression::Unknown => None,
    }
}

/// DDS format to TgPcDx format string.
pub fn dds_format_to_tgpcdx(dds: &ddsfile::Dds) -> String {
    if let Some(ref fourcc) = dds.header.spf.fourcc {
        let s = format!("{:?}", fourcc);
        if s.contains("DXT5") {
            return "HC_IMAGE_FORMAT_COMPRESSED_INTERP_ALPHA".into();
        }
        if s.contains("DXT3") {
            return "HC_IMAGE_FORMAT_COMPRESSED_EXPL_ALPHA".into();
        }
        if s.contains("DXT1") {
            return "HC_IMAGE_FORMAT_COMPRESSED".into();
        }
    }
    if let Some(d3d) = dds.get_d3d_format() {
        match d3d {
            ddsfile::D3DFormat::DXT5 => return "HC_IMAGE_FORMAT_COMPRESSED_INTERP_ALPHA".into(),
            ddsfile::D3DFormat::DXT3 => return "HC_IMAGE_FORMAT_COMPRESSED_EXPL_ALPHA".into(),
            ddsfile::D3DFormat::DXT1 => return "HC_IMAGE_FORMAT_COMPRESSED".into(),
            ddsfile::D3DFormat::A8R8G8B8
            | ddsfile::D3DFormat::A8B8G8R8
            | ddsfile::D3DFormat::X8R8G8B8
            | ddsfile::D3DFormat::X8B8G8R8 => return "HC_IMAGE_FORMAT_8888".into(),
            ddsfile::D3DFormat::L8 => return "HC_IMAGE_FORMAT_L8".into(),
            ddsfile::D3DFormat::A8 => return "HC_IMAGE_FORMAT_A8".into(),
            _ => {}
        }
    }
    if let Some(dxgi) = dds.get_dxgi_format() {
        let s = format!("{:?}", dxgi);
        if s.contains("BC3") {
            return "HC_IMAGE_FORMAT_COMPRESSED_INTERP_ALPHA".into();
        }
        if s.contains("BC2") {
            return "HC_IMAGE_FORMAT_COMPRESSED_EXPL_ALPHA".into();
        }
        if s.contains("BC1") {
            return "HC_IMAGE_FORMAT_COMPRESSED".into();
        }
        if s.contains("R8G8B8A8") || s.contains("B8G8R8A8") || s.contains("R8G8B8X8") || s.contains("B8G8R8X8") {
            return "HC_IMAGE_FORMAT_8888".into();
        }
        if s.contains("A8_UNORM") {
            return "HC_IMAGE_FORMAT_A8".into();
        }
        if s.contains("R8_UNORM") || s.contains("L8_UNORM") {
            return "HC_IMAGE_FORMAT_L8".into();
        }
    }
    // Uncompressed heuristic: if no fourcc/DXT and pitch is 4 bytes per pixel.
    if dds.header.spf.fourcc.is_none() {
        let bpp = dds.header.spf.rgb_bit_count;
        if bpp == Some(32) {
            return "HC_IMAGE_FORMAT_8888".into();
        }
    }
    "HC_IMAGE_FORMAT_COMPRESSED".into()
}

/// TgPcDx format string to D3D format for DDS export.
pub fn tgpcdx_format_to_d3d(fmt: &str) -> ddsfile::D3DFormat {
    let u = fmt.to_uppercase();
    if u.contains("INTERP_ALPHA") || u.contains("DXT5") || u.contains("BC3") {
        return ddsfile::D3DFormat::DXT5;
    }
    if u.contains("EXPL_ALPHA") || u.contains("DXT3") || u.contains("BC2") {
        return ddsfile::D3DFormat::DXT3;
    }
    if u.contains("DXT1") || u.contains("BC1") || u.contains("COMPRESSED") {
        return ddsfile::D3DFormat::DXT1;
    }
    if u.contains("BGRA") {
        return ddsfile::D3DFormat::A8R8G8B8;
    }
    if u.contains("RGBA") || u.contains("ARGB") {
        return ddsfile::D3DFormat::A8B8G8R8;
    }
    if u.contains("L8") {
        return ddsfile::D3DFormat::L8;
    }
    if u.contains("A8") {
        return ddsfile::D3DFormat::A8;
    }
    if u.contains("8888") {
        return ddsfile::D3DFormat::A8B8G8R8;
    }
    ddsfile::D3DFormat::DXT1
}

/// Expected mip size for validation during DDS export.
pub fn expected_mip_size(w: u32, h: u32, fmt: ddsfile::D3DFormat) -> usize {
    match fmt {
        ddsfile::D3DFormat::DXT1
        | ddsfile::D3DFormat::DXT2
        | ddsfile::D3DFormat::DXT3
        | ddsfile::D3DFormat::DXT4
        | ddsfile::D3DFormat::DXT5 => {
            let bw = (w as usize).div_ceil(4);
            let bh = (h as usize).div_ceil(4);
            let block = if matches!(fmt, ddsfile::D3DFormat::DXT1) {
                8
            } else {
                16
            };
            bw.max(1) * bh.max(1) * block
        }
        ddsfile::D3DFormat::A8 | ddsfile::D3DFormat::L8 | ddsfile::D3DFormat::A4L4 => {
            (w as usize) * (h as usize)
        }
        ddsfile::D3DFormat::R5G6B5
        | ddsfile::D3DFormat::A1R5G5B5
        | ddsfile::D3DFormat::X1R5G5B5
        | ddsfile::D3DFormat::A4R4G4B4
        | ddsfile::D3DFormat::X4R4G4B4
        | ddsfile::D3DFormat::A8L8
        | ddsfile::D3DFormat::L16 => (w as usize) * (h as usize) * 2,
        _ => (w as usize) * (h as usize) * 4,
    }
}

/// DdsCompression from TgPcDx format string.
pub fn tgpcdx_dds_compression(fmt: &str) -> DdsCompression {
    let u = fmt.to_uppercase();
    if u.contains("INTERP_ALPHA") || u.contains("DXT5") || u.contains("BC3") {
        return DdsCompression::Dxt5;
    }
    if u.contains("EXPL_ALPHA") || u.contains("DXT3") || u.contains("BC2") {
        return DdsCompression::Dxt3;
    }
    if u.contains("COMPRESSED") || u.contains("DXT1") || u.contains("BC1") {
        return DdsCompression::Dxt1;
    }
    if u.contains("8888") || u.contains("RGBA") || u.contains("BGRA") || u.contains("ARGB") {
        return DdsCompression::Rgba8888;
    }
    if u.contains("L8") {
        return DdsCompression::L8;
    }
    if u.contains("A8") {
        return DdsCompression::A8;
    }
    DdsCompression::Unknown
}

/// Результат проверки цепочки мипмапов .dds.
#[derive(Debug, Clone)]
pub struct MipChainInfo {
    pub levels: u32,
    pub total_bytes: usize,
}

/// Итоговый отчёт об успешном импорте DDS -> .TgPcDx.
#[derive(Debug, Clone)]
pub struct ImportReport {
    pub mip_count: u32,
    pub total_bytes: usize,
    pub dds_format: String,
}

/// Проверяет, что буфер данных .dds содержит ровно столько байт, сколько
/// требуется заявленным уровням мипмапов (сумма размеров уровней строго
/// равна длине буфера — без обрезанных хвостов и лишнего мусора).
pub fn validate_dds_mip_chain(
    width: u32,
    height: u32,
    fmt: DdsCompression,
    declared_levels: u32,
    data_len: usize,
    expected_levels: Option<u32>,
) -> Result<MipChainInfo, String> {
    if fmt == DdsCompression::Unknown {
        return Err("неизвестный формат сжатия DDS".into());
    }
    if declared_levels == 0 {
        return Err("в DDS-файле не объявлено ни одного уровня мипмапов".into());
    }
    if let Some(expected) = expected_levels
        && declared_levels < expected
    {
        let level_word = match declared_levels % 10 {
            1 if declared_levels % 100 != 11 => "уровень",
            2..=4 if !(12..=14).contains(&(declared_levels % 100)) => "уровня",
            _ => "уровней",
        };
        return Err(format!(
            "В DDS-файле {} {} мипмапов, ожидается {}. Включите генерацию MipMaps при сохранении DDS",
            declared_levels, level_word, expected
        ));
    }

    let mut total = 0usize;
    let mut mw = width;
    let mut mh = height;
    for _ in 0..declared_levels {
        let size =
            mip_level_bytes(mw, mh, fmt).ok_or("невозможно вычислить размер уровня мипмапа")?;
        total = total
            .checked_add(size)
            .ok_or("переполнение при суммировании уровней мипмапов")?;
        mw = (mw / 2).max(1);
        mh = (mh / 2).max(1);
    }

    if total != data_len {
        return Err(format!(
            "DDS-файл обрезан или содержит лишние данные: ожидается {total} байт мипмапов, найдено {data_len}"
        ));
    }
    Ok(MipChainInfo {
        levels: declared_levels,
        total_bytes: total,
    })
}