//! TgPcDx texture decoding to RGBA and UI conversion.

use super::serz_parse::image_mips_from_bytes;

/// Основная функция декодирования TgPcDx в RGBA с опциональным ограничением размера.
pub fn decode_tgpcdx_with_max_dimension(
    data: &[u8],
    max_dimension: Option<u32>,
) -> Result<(u32, u32, Vec<u8>), String> {
    let image_blocks = image_mips_from_bytes(data)?;
    let image_block = max_dimension
        .and_then(|max_dim| {
            image_blocks
                .iter()
                .filter(|block| block.width <= max_dim && block.height <= max_dim)
                .max_by_key(|block| block.width.max(block.height))
        })
        .or_else(|| image_blocks.first())
        .ok_or("no image block")?;

    let (width, height) = (image_block.width, image_block.height);
    if width == 0 || height == 0 {
        return Err("invalid dimensions".into());
    }
    let bytes = &image_block.data;

    let pixels = (width as usize)
        .checked_mul(height as usize)
        .ok_or("texture dimensions overflow")?;
    let rgba_bytes = pixels.checked_mul(4).ok_or("texture dimensions overflow")?;

    let format_upper = image_block.format.to_ascii_uppercase();
    let is_bc3 = format_upper.contains("INTERP_ALPHA")
        || format_upper.contains("BC3")
        || format_upper.contains("DXT5");
    let is_bc2 = format_upper.contains("EXPL_ALPHA")
        || format_upper.contains("BC2")
        || format_upper.contains("DXT3");
    let is_bc1 = (format_upper.contains("COMPRESSED")
        || format_upper.contains("BC1")
        || format_upper.contains("DXT1"))
        && !is_bc3
        && !is_bc2;
    let is_uncompressed = format_upper.contains("RGBA")
        || format_upper.contains("BGRA")
        || format_upper.contains("ARGB")
        || format_upper.contains("UNCOMPRESSED");

    let is_known_compressed = is_bc1 || is_bc2 || is_bc3;

    if is_uncompressed || (!is_known_compressed && bytes.len() >= rgba_bytes) {
        if bytes.len() < rgba_bytes {
            return Err("uncompressed texture data too small".into());
        }
        let bgra = format_upper.contains("BGRA");
        let argb = format_upper.contains("ARGB");

        if !bgra && !argb {
            return Ok((width, height, bytes[..rgba_bytes].to_vec()));
        }

        let mut rgba = vec![0u8; rgba_bytes];
        let src_chunks = bytes[..rgba_bytes].as_chunks::<4>().0;
        let dst_chunks = rgba.as_chunks_mut::<4>().0;
        if bgra {
            for (dst, &[b, g, r, a]) in dst_chunks.iter_mut().zip(src_chunks) {
                *dst = [r, g, b, a];
            }
        } else {
            for (dst, &[a, r, g, b]) in dst_chunks.iter_mut().zip(src_chunks) {
                *dst = [r, g, b, a];
            }
        }
        return Ok((width, height, rgba));
    }

    let blocks_w = width.div_ceil(4) as usize;
    let blocks_h = height.div_ceil(4) as usize;
    let bc3_min_bytes = blocks_w.saturating_mul(blocks_h).saturating_mul(16);

    let mut image_u32 = vec![0u32; pixels];
    if is_bc2 {
        texture2ddecoder::decode_bc2(bytes, width as usize, height as usize, &mut image_u32)
            .map_err(|e| e.to_string())?;
    } else if is_bc3 || (!is_bc1 && bytes.len() >= bc3_min_bytes) {
        texture2ddecoder::decode_bc3(bytes, width as usize, height as usize, &mut image_u32)
            .map_err(|e| e.to_string())?;
    } else {
        texture2ddecoder::decode_bc1(bytes, width as usize, height as usize, &mut image_u32)
            .map_err(|e| e.to_string())?;
    }

    let mut rgba = vec![0u8; rgba_bytes];
    for (dst, px) in rgba.as_chunks_mut::<4>().0.iter_mut().zip(image_u32) {
        *dst = [
            (px >> 16) as u8,
            (px >> 8) as u8,
            px as u8,
            (px >> 24) as u8,
        ];
    }

    Ok((width, height, rgba))
}

/// Decode TgPcDx to RGBA (full resolution).
pub fn decode_tgpcdx_to_rgba(data: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    decode_tgpcdx_with_max_dimension(data, None)
}

/// Convert RGBA bytes to slint Image for UI display.
pub fn rgba_to_slint_image(width: u32, height: u32, rgba: &[u8]) -> slint::Image {
    let pixel_buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
        bytemuck::cast_slice(rgba),
        width,
        height,
    );
    slint::Image::from_rgba8(pixel_buffer)
}