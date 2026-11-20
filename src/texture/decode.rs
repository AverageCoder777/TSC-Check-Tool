//! TgPcDx texture decoding to RGBA and UI conversion.
use super::serz_parse::image_mips_from_bytes;

/// Основная функция декодирования TgPcDx в RGBA с опциональным ограничением размера.
pub fn decode_tgpcdx_with_max_dimension(
    data: &[u8],
    max_dimension: Option<u32>,
) -> Result<(u32, u32, Vec<u8>), String> {
    let image_blocks = image_mips_from_bytes(data)?;
    let image_block = max_dimension
        .and_then(|max_dimension| {
            image_blocks
                .iter()
                .filter(|block| block.width <= max_dimension && block.height <= max_dimension)
                .max_by_key(|block| block.width.max(block.height))
        })
        .or_else(|| image_blocks.first())
        .ok_or("no image block")?;
    let format = &image_block.format;
    let (width, height) = (image_block.width, image_block.height);
    if width == 0 || height == 0 {
        return Err("invalid dimensions".into());
    }
    let bytes = &image_block.data;

    let pixels = width
        .checked_mul(height)
        .ok_or("texture dimensions overflow")? as usize;
    let rgba_bytes = pixels.checked_mul(4).ok_or("texture dimensions overflow")?;
    let format_upper = format.to_uppercase();
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

    let mut image_u32 = vec![0u32; pixels];
    let mut rgba = Vec::with_capacity(rgba_bytes);

    if is_uncompressed || (!is_bc3 && !is_bc2 && !is_bc1 && bytes.len() >= pixels * 4) {
        // Uncompressed 32-bit (RGBA/BGRA)
        let bgra = format_upper.contains("BGRA");
        let argb = format_upper.contains("ARGB");
        for i in 0..pixels {
            let off = i * 4;
            if off + 4 > bytes.len() {
                break;
            }
            let (r, g, b, a) = if bgra {
                (bytes[off + 2], bytes[off + 1], bytes[off], bytes[off + 3])
            } else if argb {
                (bytes[off + 1], bytes[off + 2], bytes[off + 3], bytes[off])
            } else {
                (bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3])
            };
            image_u32[i] = (a as u32) << 24 | (r as u32) << 16 | (g as u32) << 8 | (b as u32);
        }
    } else if is_bc2 {
        texture2ddecoder::decode_bc2(bytes, width as usize, height as usize, &mut image_u32)
            .map_err(|e| e.to_string())?;
    } else if is_bc3 || (!is_bc1 && bytes.len() >= pixels) {
        // BC3 / DXT5 (16 bytes per 4x4 block)
        texture2ddecoder::decode_bc3(bytes, width as usize, height as usize, &mut image_u32)
            .map_err(|e| e.to_string())?;
    } else {
        // BC1 / DXT1 (8 bytes per 4x4 block)
        texture2ddecoder::decode_bc1(bytes, width as usize, height as usize, &mut image_u32)
            .map_err(|e| e.to_string())?;
    }

    // Single conversion loop for all formats
    for px in image_u32 {
        rgba.push(((px >> 16) & 0xff) as u8);
        rgba.push(((px >> 8) & 0xff) as u8);
        rgba.push((px & 0xff) as u8);
        rgba.push(((px >> 24) & 0xff) as u8);
    }

    if rgba.len() < rgba_bytes {
        return Err("decoded data too small".into());
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
