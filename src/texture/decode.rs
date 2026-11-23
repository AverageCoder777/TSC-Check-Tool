use std::borrow::Cow;
use std::cmp::Reverse;

use super::format::{
    DdsCompression, contains_ignore_ascii_case, mip_level_bytes, tgpcdx_dds_compression,
};
use super::serz_parse::{ImageMip, image_mips_from_bytes};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompressedFormat {
    Bc1,
    Bc2,
    Bc3,
}

impl CompressedFormat {
    pub const fn block_bytes(self) -> usize {
        match self {
            Self::Bc1 => 8,
            Self::Bc2 | Self::Bc3 => 16,
        }
    }
}

#[derive(Clone, Debug)]
pub struct TextureMip {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug)]
pub enum DecodedTextureData {
    Rgba8 {
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    },
    Compressed {
        format: CompressedFormat,
        mips: Vec<TextureMip>,
    },
}

/// Режим отображения каналов во вьюере текстур.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ChannelMode {
    #[default]
    Rgb,
    Rgba,
    Alpha,
    Red,
    Green,
    Blue,
}

impl ChannelMode {
    pub const COUNT: i32 = 6;

    pub const fn from_index(index: i32) -> Self {
        match index.rem_euclid(Self::COUNT) {
            0 => Self::Rgb,
            1 => Self::Rgba,
            2 => Self::Alpha,
            3 => Self::Red,
            4 => Self::Green,
            _ => Self::Blue,
        }
    }

    pub const fn as_index(self) -> i32 {
        match self {
            Self::Rgb => 0,
            Self::Rgba => 1,
            Self::Alpha => 2,
            Self::Red => 3,
            Self::Green => 4,
            Self::Blue => 5,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Rgb => "RGB (opaque)",
            Self::Rgba => "RGBA",
            Self::Alpha => "Alpha",
            Self::Red => "Red (R)",
            Self::Green => "Green (G)",
            Self::Blue => "Blue (B)",
        }
    }
}

/// Распаковывает упакованный блоб (`1× cHcImageDx` со всеми мипмапами подряд) в отдельные
/// `ImageMip` и сортирует уровни от большего к меньшему.
pub(super) fn expand_and_sort_mips(mut blocks: Vec<ImageMip>) -> Vec<ImageMip> {
    if blocks.len() == 1 {
        let fmt = tgpcdx_dds_compression(&blocks[0].format);
        let base_w = blocks[0].width;
        let base_h = blocks[0].height;
        if fmt != DdsCompression::Unknown
            && let Some(top_size) = mip_level_bytes(base_w, base_h, fmt)
            && blocks[0].data.len() > top_size
        {
            let base = blocks.pop().unwrap();
            let mut mips = Vec::new();
            let mut offset = 0usize;
            let mut w = base.width;
            let mut h = base.height;
            while offset < base.data.len() {
                let Some(sz) = mip_level_bytes(w, h, fmt) else {
                    break;
                };
                if sz == 0 || offset + sz > base.data.len() {
                    break;
                }
                mips.push(ImageMip {
                    format: base.format.clone(),
                    width: w,
                    height: h,
                    data: base.data[offset..offset + sz].to_vec(),
                });
                offset += sz;
                if w == 1 && h == 1 {
                    break;
                }
                w = (w / 2).max(1);
                h = (h / 2).max(1);
            }
            if !mips.is_empty() {
                return mips;
            }
            blocks.push(base);
        }
    }

    blocks.sort_unstable_by_key(|b| Reverse((b.width.max(b.height), u64::from(b.width) * u64::from(b.height))));
    blocks
}

/// Декодирует один `ImageMip` в RGBA8.
pub(super) fn decode_mip_to_rgba(mip: &ImageMip) -> Result<Vec<u8>, String> {
    let (width, height) = (mip.width, mip.height);
    if width == 0 || height == 0 {
        return Err("invalid dimensions".into());
    }
    let bytes = &mip.data;
    let pixels = (width as usize)
        .checked_mul(height as usize)
        .ok_or("texture dimensions overflow")?;
    let rgba_bytes = pixels.checked_mul(4).ok_or("texture dimensions overflow")?;

    match tgpcdx_dds_compression(&mip.format) {
        DdsCompression::Dxt1 => decode_bc_to_rgba(CompressedFormat::Bc1, width, height, bytes),
        DdsCompression::Dxt3 => decode_bc_to_rgba(CompressedFormat::Bc2, width, height, bytes),
        DdsCompression::Dxt5 => decode_bc_to_rgba(CompressedFormat::Bc3, width, height, bytes),
        DdsCompression::L8 => {
            if bytes.len() < pixels {
                return Err("L8 texture data too small".into());
            }
            let mut rgba = vec![0u8; rgba_bytes];
            for (dst, &v) in rgba.as_chunks_mut::<4>().0.iter_mut().zip(&bytes[..pixels]) {
                *dst = [v, v, v, 255];
            }
            Ok(rgba)
        }
        DdsCompression::A8 => {
            if bytes.len() < pixels {
                return Err("A8 texture data too small".into());
            }
            let mut rgba = vec![0u8; rgba_bytes];
            for (dst, &a) in rgba.as_chunks_mut::<4>().0.iter_mut().zip(&bytes[..pixels]) {
                *dst = [255, 255, 255, a];
            }
            Ok(rgba)
        }
        DdsCompression::Rgba8888 | DdsCompression::Bgra8888 => {
            decode_uncompressed_rgba(&mip.format, bytes, rgba_bytes)
        }
        DdsCompression::Unknown => {
            if bytes.len() >= rgba_bytes {
                return decode_uncompressed_rgba(&mip.format, bytes, rgba_bytes);
            }
            let bc3_min = (width.div_ceil(4) as usize)
                .saturating_mul(height.div_ceil(4) as usize)
                .saturating_mul(16);
            let format = if bytes.len() >= bc3_min {
                CompressedFormat::Bc3
            } else {
                CompressedFormat::Bc1
            };
            decode_bc_to_rgba(format, width, height, bytes)
        }
    }
}

fn decode_uncompressed_rgba(format: &str, bytes: &[u8], rgba_bytes: usize) -> Result<Vec<u8>, String> {
    if bytes.len() < rgba_bytes {
        return Err("uncompressed texture data too small".into());
    }
    let is_bgra = contains_ignore_ascii_case(format, "BGRA");
    let is_argb = contains_ignore_ascii_case(format, "ARGB");

    if !is_bgra && !is_argb {
        return Ok(bytes[..rgba_bytes].to_vec());
    }

    let mut rgba = vec![0u8; rgba_bytes];
    let src_chunks = bytes[..rgba_bytes].as_chunks::<4>().0;
    let dst_chunks = rgba.as_chunks_mut::<4>().0;
    if is_bgra {
        for (dst, &[b, g, r, a]) in dst_chunks.iter_mut().zip(src_chunks) {
            *dst = [r, g, b, a];
        }
    } else {
        for (dst, &[a, r, g, b]) in dst_chunks.iter_mut().zip(src_chunks) {
            *dst = [r, g, b, a];
        }
    }
    Ok(rgba)
}

pub fn decode_tgpcdx_with_max_dimension(
    data: &[u8],
    max_dimension: Option<u32>,
) -> Result<(u32, u32, Vec<u8>), String> {
    let image_blocks = expand_and_sort_mips(image_mips_from_bytes(data)?);
    let image_block = max_dimension
        .and_then(|max_dim| {
            image_blocks
                .iter()
                .find(|block| block.width <= max_dim && block.height <= max_dim)
        })
        .or_else(|| image_blocks.first())
        .ok_or("no image block")?;

    let rgba = decode_mip_to_rgba(image_block)?;
    Ok((image_block.width, image_block.height, rgba))
}

pub fn decode_tgpcdx_to_rgba(data: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    decode_tgpcdx_with_max_dimension(data, None)
}

pub fn rgba_to_slint_image(width: u32, height: u32, rgba: &[u8]) -> slint::Image {
    let pixel_buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
        bytemuck::cast_slice(rgba),
        width,
        height,
    );
    slint::Image::from_rgba8(pixel_buffer)
}

/// Строит буфер RGBA8 для выбранного режима каналов без лишних аллокаций (для `Rgba` — 0 копий).
pub(crate) fn apply_channel_mode(rgba: &[u8], mode: ChannelMode) -> Cow<'_, [u8]> {
    if mode == ChannelMode::Rgba {
        return Cow::Borrowed(rgba);
    }
    let mut out = vec![0u8; rgba.len()];
    let src = rgba.as_chunks::<4>().0;
    let dst = out.as_chunks_mut::<4>().0;
    match mode {
        ChannelMode::Rgb => {
            for (d, &[r, g, b, _]) in dst.iter_mut().zip(src) {
                *d = [r, g, b, 255];
            }
        }
        ChannelMode::Alpha => {
            for (d, &[_, _, _, a]) in dst.iter_mut().zip(src) {
                *d = [a, a, a, 255];
            }
        }
        ChannelMode::Red => {
            for (d, &[r, _, _, _]) in dst.iter_mut().zip(src) {
                *d = [r, r, r, 255];
            }
        }
        ChannelMode::Green => {
            for (d, &[_, g, _, _]) in dst.iter_mut().zip(src) {
                *d = [g, g, g, 255];
            }
        }
        ChannelMode::Blue => {
            for (d, &[_, _, b, _]) in dst.iter_mut().zip(src) {
                *d = [b, b, b, 255];
            }
        }
        ChannelMode::Rgba => unreachable!(),
    }
    Cow::Owned(out)
}

pub fn decode_bc_to_rgba(
    format: CompressedFormat,
    width: u32,
    height: u32,
    bytes: &[u8],
) -> Result<Vec<u8>, String> {
    let pixels = (width as usize)
        .checked_mul(height as usize)
        .ok_or("texture dimensions overflow")?;
    let rgba_bytes = pixels.checked_mul(4).ok_or("texture dimensions overflow")?;

    let mut image_u32 = vec![0u32; pixels];
    match format {
        CompressedFormat::Bc1 => {
            texture2ddecoder::decode_bc1(bytes, width as usize, height as usize, &mut image_u32)
        }
        CompressedFormat::Bc2 => {
            texture2ddecoder::decode_bc2(bytes, width as usize, height as usize, &mut image_u32)
        }
        CompressedFormat::Bc3 => {
            texture2ddecoder::decode_bc3(bytes, width as usize, height as usize, &mut image_u32)
        }
    }
    .map_err(|e| e.to_string())?;

    let mut rgba = vec![0u8; rgba_bytes];
    for (dst, px) in rgba.as_chunks_mut::<4>().0.iter_mut().zip(image_u32) {
        *dst = [
            (px >> 16) as u8,
            (px >> 8) as u8,
            px as u8,
            (px >> 24) as u8,
        ];
    }
    Ok(rgba)
}

pub fn decode_tgpcdx_native(data: &[u8]) -> Result<DecodedTextureData, String> {
    let image_blocks = expand_and_sort_mips(image_mips_from_bytes(data)?);
    let Some(base) = image_blocks.first() else {
        return Err("no image block".into());
    };
    if base.width == 0 || base.height == 0 {
        return Err("invalid dimensions".into());
    }

    let rgba_bytes = (base.width as usize)
        .checked_mul(base.height as usize)
        .and_then(|p| p.checked_mul(4))
        .ok_or("texture dimensions overflow")?;

    let bc_format = match tgpcdx_dds_compression(&base.format) {
        DdsCompression::Dxt1 => Some(CompressedFormat::Bc1),
        DdsCompression::Dxt3 => Some(CompressedFormat::Bc2),
        DdsCompression::Dxt5 => Some(CompressedFormat::Bc3),
        DdsCompression::Unknown if base.data.len() < rgba_bytes => {
            let bc3_min = (base.width.div_ceil(4) as usize)
                .saturating_mul(base.height.div_ceil(4) as usize)
                .saturating_mul(16);
            Some(if base.data.len() >= bc3_min {
                CompressedFormat::Bc3
            } else {
                CompressedFormat::Bc1
            })
        }
        _ => None,
    };

    if let Some(format) = bc_format
        && base.width.is_multiple_of(4)
        && base.height.is_multiple_of(4)
    {
        let block_bytes = format.block_bytes();
        let mut mips = Vec::with_capacity(image_blocks.len());
        let mut expected_w = base.width;
        let mut expected_h = base.height;

        for block in image_blocks {
            if block.width != expected_w || block.height != expected_h {
                if mips.is_empty() {
                    break;
                }
                continue;
            }
            let min_bytes = (block.width.div_ceil(4) as usize)
                .saturating_mul(block.height.div_ceil(4) as usize)
                .saturating_mul(block_bytes);
            if block.data.len() < min_bytes {
                break;
            }
            mips.push(TextureMip {
                width: block.width,
                height: block.height,
                data: block.data,
            });
            if expected_w == 1 && expected_h == 1 {
                break;
            }
            expected_w = (expected_w / 2).max(1);
            expected_h = (expected_h / 2).max(1);
        }

        if !mips.is_empty() {
            return Ok(DecodedTextureData::Compressed { format, mips });
        }
    }

    let (width, height, pixels) = decode_tgpcdx_to_rgba(data)?;
    Ok(DecodedTextureData::Rgba8 {
        width,
        height,
        pixels,
    })
}
