//! Texture format handling and mip size calculations.

use ddsfile::{D3DFormat, Dds, DxgiFormat, FourCC};

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
    pub const fn as_dds_name(self) -> &'static str {
        match self {
            Self::Dxt1 => "DXT1",
            Self::Dxt3 => "DXT3",
            Self::Dxt5 => "DXT5",
            Self::Rgba8888 | Self::Bgra8888 => "RGBA8888",
            Self::L8 => "L8",
            Self::A8 => "A8",
            Self::Unknown => "unknown format",
        }
    }
}

pub(super) fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    let needle = needle.as_bytes();
    if needle.is_empty() {
        return true;
    }
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

/// Распознаёт формат сжатия по заголовку DDS_HEADER файла .dds.
pub fn dds_compression_of(dds: &Dds) -> DdsCompression {
    if let Some(dxgi) = dds.get_dxgi_format() {
        return match dxgi {
            DxgiFormat::BC3_Typeless | DxgiFormat::BC3_UNorm | DxgiFormat::BC3_UNorm_sRGB => {
                DdsCompression::Dxt5
            }
            DxgiFormat::BC2_Typeless | DxgiFormat::BC2_UNorm | DxgiFormat::BC2_UNorm_sRGB => {
                DdsCompression::Dxt3
            }
            DxgiFormat::BC1_Typeless | DxgiFormat::BC1_UNorm | DxgiFormat::BC1_UNorm_sRGB => {
                DdsCompression::Dxt1
            }
            DxgiFormat::B8G8R8A8_Typeless
            | DxgiFormat::B8G8R8A8_UNorm
            | DxgiFormat::B8G8R8A8_UNorm_sRGB
            | DxgiFormat::B8G8R8X8_Typeless
            | DxgiFormat::B8G8R8X8_UNorm
            | DxgiFormat::B8G8R8X8_UNorm_sRGB => DdsCompression::Bgra8888,
            DxgiFormat::R8G8B8A8_Typeless
            | DxgiFormat::R8G8B8A8_UNorm
            | DxgiFormat::R8G8B8A8_UNorm_sRGB
            | DxgiFormat::R8G8B8A8_UInt
            | DxgiFormat::R8G8B8A8_SNorm
            | DxgiFormat::R8G8B8A8_SInt => DdsCompression::Rgba8888,
            DxgiFormat::A8_UNorm => DdsCompression::A8,
            DxgiFormat::R8_Typeless
            | DxgiFormat::R8_UNorm
            | DxgiFormat::R8_UInt
            | DxgiFormat::R8_SNorm
            | DxgiFormat::R8_SInt => DdsCompression::L8,
            _ => DdsCompression::Unknown,
        };
    }

    if let Some(d3d) = dds.get_d3d_format() {
        return match d3d {
            D3DFormat::DXT5 | D3DFormat::DXT4 => DdsCompression::Dxt5,
            D3DFormat::DXT3 | D3DFormat::DXT2 => DdsCompression::Dxt3,
            D3DFormat::DXT1 => DdsCompression::Dxt1,
            D3DFormat::A8R8G8B8 | D3DFormat::X8R8G8B8 => DdsCompression::Bgra8888,
            D3DFormat::A8B8G8R8 | D3DFormat::X8B8G8R8 => DdsCompression::Rgba8888,
            D3DFormat::L8 => DdsCompression::L8,
            D3DFormat::A8 => DdsCompression::A8,
            _ => DdsCompression::Unknown,
        };
    }

    if let Some(fourcc) = &dds.header.spf.fourcc {
        return match fourcc.0 {
            FourCC::DXT5 | FourCC::DXT4 => DdsCompression::Dxt5,
            FourCC::DXT3 | FourCC::DXT2 => DdsCompression::Dxt3,
            FourCC::DXT1 => DdsCompression::Dxt1,
            _ => DdsCompression::Unknown,
        };
    }

    if dds.header.spf.rgb_bit_count == Some(32) {
        return DdsCompression::Rgba8888;
    }

    DdsCompression::Unknown
}

/// DdsCompression from TgPcDx format string (zero allocations).
pub fn tgpcdx_dds_compression(fmt: &str) -> DdsCompression {
    let has = |needle| contains_ignore_ascii_case(fmt, needle);
    if has("INTERP_ALPHA") || has("DXT5") || has("BC3") {
        DdsCompression::Dxt5
    } else if has("EXPL_ALPHA") || has("DXT3") || has("BC2") {
        DdsCompression::Dxt3
    } else if has("COMPRESSED") || has("DXT1") || has("BC1") {
        DdsCompression::Dxt1
    } else if has("BGRA") {
        DdsCompression::Bgra8888
    } else if has("8888") || has("RGBA") || has("ARGB") || has("UNCOMPRESSED") {
        DdsCompression::Rgba8888
    } else if has("L8") {
        DdsCompression::L8
    } else if has("A8") {
        DdsCompression::A8
    } else {
        DdsCompression::Unknown
    }
}

/// (человекочитаемая метка, эквивалент в DDS) для тега `Format` из .TgPcDx.
pub fn format_label_and_dds_name(fmt: &str) -> (String, Option<&'static str>) {
    match tgpcdx_dds_compression(fmt) {
        DdsCompression::Dxt5 => ("DXT5 (BC3, interpolated alpha)".into(), Some("DXT5")),
        DdsCompression::Dxt3 => ("DXT3 (BC2, explicit alpha)".into(), Some("DXT3")),
        DdsCompression::Dxt1 => ("DXT1 (BC1, no / 1-bit alpha)".into(), Some("DXT1")),
        DdsCompression::Rgba8888 | DdsCompression::Bgra8888 => {
            ("RGBA8888 (uncompressed)".into(), Some("RGBA8888"))
        }
        DdsCompression::L8 => ("L8 (8-bit grayscale)".into(), Some("L8")),
        DdsCompression::A8 => ("A8 (8-bit alpha)".into(), Some("A8")),
        DdsCompression::Unknown => (fmt.to_owned(), None),
    }
}

/// Ожидаемый размер одного уровня мипмапа в байтах.
pub fn mip_level_bytes(w: u32, h: u32, fmt: DdsCompression) -> Option<usize> {
    if w == 0 || h == 0 {
        return None;
    }
    let bw = w.div_ceil(4) as usize;
    let bh = h.div_ceil(4) as usize;
    match fmt {
        DdsCompression::Dxt1 => bw.checked_mul(bh)?.checked_mul(8),
        DdsCompression::Dxt3 | DdsCompression::Dxt5 => bw.checked_mul(bh)?.checked_mul(16),
        DdsCompression::Rgba8888 | DdsCompression::Bgra8888 => {
            (w as usize).checked_mul(h as usize)?.checked_mul(4)
        }
        DdsCompression::L8 | DdsCompression::A8 => (w as usize).checked_mul(h as usize),
        DdsCompression::Unknown => None,
    }
}

/// DDS format to TgPcDx format string.
pub fn dds_format_to_tgpcdx(dds: &Dds) -> String {
    match dds_compression_of(dds) {
        DdsCompression::Dxt5 => "HC_IMAGE_FORMAT_COMPRESSED_INTERP_ALPHA",
        DdsCompression::Dxt3 => "HC_IMAGE_FORMAT_COMPRESSED_EXPL_ALPHA",
        DdsCompression::Dxt1 | DdsCompression::Unknown => "HC_IMAGE_FORMAT_COMPRESSED",
        DdsCompression::Rgba8888 | DdsCompression::Bgra8888 => "HC_IMAGE_FORMAT_8888",
        DdsCompression::L8 => "HC_IMAGE_FORMAT_L8",
        DdsCompression::A8 => "HC_IMAGE_FORMAT_A8",
    }
    .into()
}

/// TgPcDx format string to D3D format for DDS export.
pub fn tgpcdx_format_to_d3d(fmt: &str) -> D3DFormat {
    match tgpcdx_dds_compression(fmt) {
        DdsCompression::Dxt5 => D3DFormat::DXT5,
        DdsCompression::Dxt3 => D3DFormat::DXT3,
        DdsCompression::Dxt1 | DdsCompression::Unknown => D3DFormat::DXT1,
        DdsCompression::Bgra8888 => D3DFormat::A8R8G8B8,
        DdsCompression::Rgba8888 => D3DFormat::A8B8G8R8,
        DdsCompression::L8 => D3DFormat::L8,
        DdsCompression::A8 => D3DFormat::A8,
    }
}

/// Expected mip size for validation during DDS export.
pub fn expected_mip_size(w: u32, h: u32, fmt: D3DFormat) -> usize {
    if w == 0 || h == 0 {
        return 0;
    }
    match fmt {
        D3DFormat::DXT1 | D3DFormat::DXT2 | D3DFormat::DXT3 | D3DFormat::DXT4 | D3DFormat::DXT5 => {
            let bw = w.div_ceil(4) as usize;
            let bh = h.div_ceil(4) as usize;
            let block = if fmt == D3DFormat::DXT1 { 8 } else { 16 };
            bw.saturating_mul(bh).saturating_mul(block)
        }
        D3DFormat::A8 | D3DFormat::L8 | D3DFormat::A4L4 => (w as usize).saturating_mul(h as usize),
        D3DFormat::R5G6B5
        | D3DFormat::A1R5G5B5
        | D3DFormat::X1R5G5B5
        | D3DFormat::A4R4G4B4
        | D3DFormat::X4R4G4B4
        | D3DFormat::A8L8
        | D3DFormat::L16 => (w as usize).saturating_mul(h as usize).saturating_mul(2),
        _ => (w as usize).saturating_mul(h as usize).saturating_mul(4),
    }
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
/// требуется заявленным уровням мипмапов.
pub fn validate_dds_mip_chain(
    width: u32,
    height: u32,
    fmt: DdsCompression,
    declared_levels: u32,
    data_len: usize,
    expected_levels: Option<u32>,
) -> Result<MipChainInfo, String> {
    if fmt == DdsCompression::Unknown {
        return Err("unknown compression format of DDS".into());
    }
    if width == 0 || height == 0 {
        return Err("incorrect DDS dimensions".into());
    }
    if declared_levels == 0 {
        return Err("no mipmap levels in DDS".into());
    }
    if let Some(expected) = expected_levels
        && declared_levels < expected
    {
        let level_word = if declared_levels == 1 { "level" } else { "levels" };
        return Err(format!(
            "DDS file has {declared_levels} mipmap {level_word}, expected {expected}. Enable mipmap generation when saving DDS."
        ));
    }

    let mut total = 0usize;
    let mut mw = width;
    let mut mh = height;
    for _ in 0..declared_levels {
        let size = mip_level_bytes(mw, mh, fmt).ok_or("failed to compute mipmap level size")?;
        total = total
            .checked_add(size)
            .ok_or("overflow when summing mipmap levels")?;
        mw = (mw / 2).max(1);
        mh = (mh / 2).max(1);
    }

    if total != data_len {
        return Err(format!(
            "DDS file is truncated or has extra data: expected {total} bytes of mipmaps, found {data_len}"
        ));
    }
    Ok(MipChainInfo {
        levels: declared_levels,
        total_bytes: total,
    })
}
