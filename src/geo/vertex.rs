//! Vertex buffer decoding and normal generation.

use crate::geo::types::GeoVertex;

/// Component-wise interpretation of a `POSITION` declaration element.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum PositionFormat {
    #[default]
    Float3,
    /// `D3DDECLTYPE_FLOAT16_4` / `D3DDECLTYPE_HALF4`: xyz of four half-precision floats.
    Float16_4,
    /// `D3DDECLTYPE_SHORT4N`: xyz of four signed, normalized shorts.
    Short4N,
}

/// IEEE-754 binary16 (half) to f32.
pub(crate) fn half_to_f32(bits: u16) -> f32 {
    let sign = ((bits >> 15) & 1) as u32;
    let exponent = ((bits >> 10) & 0x1F) as i32;
    let mantissa = bits & 0x03FF;
    match exponent {
        0 => {
            if mantissa == 0 {
                return if sign == 0 { 0.0 } else { -0.0 };
            }
            // Denormal: mantissa * 2^-24
            return (sign as f32 * 2.0 - 1.0) * (mantissa as f32) * 5.960464477539063e-8;
        }
        31 => {
            if mantissa == 0 {
                return if sign == 0 {
                    f32::INFINITY
                } else {
                    -f32::INFINITY
                };
            }
            return f32::NAN;
        }
        _ => {}
    }
    let exp = (exponent - 15 + 127) as u32;
    let bits32 = (sign << 31) | (exp << 23) | ((mantissa as u32) << 13);
    f32::from_bits(bits32)
}

pub(crate) fn read_vec3(data: &[u8], offset: usize) -> Result<[f32; 3], String> {
    let end = offset.checked_add(12).ok_or("vertex offset overflow")?;
    let bytes = data
        .get(offset..end)
        .ok_or("position outside vertex record")?;
    Ok([
        f32::from_le_bytes(bytes[0..4].try_into().unwrap()),
        f32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        f32::from_le_bytes(bytes[8..12].try_into().unwrap()),
    ])
}

pub(crate) fn read_vec2(data: &[u8], offset: usize) -> Result<[f32; 2], String> {
    let end = offset.checked_add(8).ok_or("vertex offset overflow")?;
    let bytes = data.get(offset..end).ok_or("UV outside vertex record")?;
    Ok([
        f32::from_le_bytes(bytes[0..4].try_into().unwrap()),
        f32::from_le_bytes(bytes[4..8].try_into().unwrap()),
    ])
}

pub(crate) fn read_position(
    record: &[u8],
    offset: usize,
    format: PositionFormat,
) -> Result<[f32; 3], String> {
    match format {
        PositionFormat::Float3 => read_vec3(record, offset),
        PositionFormat::Float16_4 => {
            let end = offset.checked_add(8).ok_or("vertex offset overflow")?;
            let bytes = record
                .get(offset..end)
                .ok_or("position outside vertex record")?;
            Ok([
                half_to_f32(u16::from_le_bytes([bytes[0], bytes[1]])),
                half_to_f32(u16::from_le_bytes([bytes[2], bytes[3]])),
                half_to_f32(u16::from_le_bytes([bytes[4], bytes[5]])),
            ])
        }
        PositionFormat::Short4N => {
            let end = offset.checked_add(8).ok_or("vertex offset overflow")?;
            let bytes = record
                .get(offset..end)
                .ok_or("position outside vertex record")?;
            let to_f32 = |value: i16| {
                if value == i16::MIN {
                    -1.0
                } else {
                    value as f32 / 32767.0
                }
            };
            Ok([
                to_f32(i16::from_le_bytes([bytes[0], bytes[1]])),
                to_f32(i16::from_le_bytes([bytes[2], bytes[3]])),
                to_f32(i16::from_le_bytes([bytes[4], bytes[5]])),
            ])
        }
    }
}

pub(crate) fn fill_missing_normals(vertices: &mut [GeoVertex], indices: &[u32]) {
    let mut generated = vec![[0.0; 3]; vertices.len()];
    for triangle in indices.chunks_exact(3) {
        let a = vertices[triangle[0] as usize].position;
        let b = vertices[triangle[1] as usize].position;
        let c = vertices[triangle[2] as usize].position;
        let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
        let ac = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
        let normal = [
            ab[1] * ac[2] - ab[2] * ac[1],
            ab[2] * ac[0] - ab[0] * ac[2],
            ab[0] * ac[1] - ab[1] * ac[0],
        ];
        for &index in triangle {
            let vertex_index = index as usize;
            let vertex = &mut vertices[vertex_index];
            let normal_length = vertex.normal.iter().map(|value| value * value).sum::<f32>();
            if normal_length >= 1e-10 {
                continue;
            }
            for axis in 0..3 {
                generated[vertex_index][axis] += normal[axis];
            }
        }
    }
    for (vertex, generated_normal) in vertices.iter_mut().zip(generated) {
        let length = vertex
            .normal
            .iter()
            .map(|value| value * value)
            .sum::<f32>()
            .sqrt();
        if length >= 1e-5 {
            for component in &mut vertex.normal {
                *component /= length;
            }
        } else {
            let generated_length = generated_normal
                .iter()
                .map(|value| value * value)
                .sum::<f32>()
                .sqrt();
            vertex.normal = if generated_length >= 1e-5 {
                generated_normal.map(|value| value / generated_length)
            } else {
                [0.0, 1.0, 0.0]
            };
        }
    }
}
