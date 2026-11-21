//! Vertex buffer decoding and normal generation.

use crate::geo::types::GeoVertex;

const NORMAL_LEN_SQ_EPSILON: f32 = 1e-10;
const HALF_SUBNORMAL_SCALE: f32 = 5.960_464_5e-8; // 2^-24

/// Component-wise interpretation of a `POSITION` declaration element.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum PositionFormat {
    #[default]
    Float3,
    Float16_4,
    Short4N,
}

/// IEEE-754 binary16 (half) to `f32`.
#[inline]
pub(crate) fn half_to_f32(bits: u16) -> f32 {
    let sign = u32::from(bits >> 15);
    let exponent = i32::from((bits >> 10) & 0x1F);
    let mantissa = bits & 0x03FF;
    match exponent {
        0 => {
            if mantissa == 0 {
                return if sign == 0 { 0.0 } else { -0.0 };
            }
            let magnitude = f32::from(mantissa) * HALF_SUBNORMAL_SCALE;
            return if sign == 0 { magnitude } else { -magnitude };
        }
        31 => {
            return if mantissa == 0 {
                if sign == 0 {
                    f32::INFINITY
                } else {
                    f32::NEG_INFINITY
                }
            } else {
                f32::NAN
            };
        }
        _ => {}
    }
    let exp = (exponent - 15 + 127) as u32;
    let bits32 = (sign << 31) | (exp << 23) | (u32::from(mantissa) << 13);
    f32::from_bits(bits32)
}

#[inline]
pub(crate) fn read_vec3(data: &[u8], offset: usize) -> Result<[f32; 3], String> {
    let end = offset.checked_add(12).ok_or("vertex offset overflow")?;
    let bytes = data
        .get(offset..end)
        .ok_or("position outside vertex record")?;
    Ok([
        f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        f32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
        f32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
    ])
}

#[inline]
pub(crate) fn read_vec2(data: &[u8], offset: usize) -> Result<[f32; 2], String> {
    let end = offset.checked_add(8).ok_or("vertex offset overflow")?;
    let bytes = data.get(offset..end).ok_or("UV outside vertex record")?;
    Ok([
        f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        f32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
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
                    f32::from(value) / 32767.0
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

#[inline]
fn length_squared(v: [f32; 3]) -> f32 {
    v[0] * v[0] + v[1] * v[1] + v[2] * v[2]
}

#[inline]
fn normalize_or_none(v: [f32; 3]) -> Option<[f32; 3]> {
    let len_sq = length_squared(v);
    if len_sq >= NORMAL_LEN_SQ_EPSILON {
        let inv_len = 1.0 / len_sq.sqrt();
        Some([v[0] * inv_len, v[1] * inv_len, v[2] * inv_len])
    } else {
        None
    }
}

pub(crate) fn fill_missing_normals(vertices: &mut [GeoVertex], indices: &[u32]) {
    let has_missing = vertices
        .iter()
        .any(|vertex| length_squared(vertex.normal) < NORMAL_LEN_SQ_EPSILON);

    if !has_missing {
        for vertex in vertices.iter_mut() {
            if let Some(unit) = normalize_or_none(vertex.normal) {
                vertex.normal = unit;
            }
        }
        return;
    }

    let mut generated = vec![[0.0f32; 3]; vertices.len()];
    for &[i0, i1, i2] in indices.as_chunks::<3>().0 {
        let a = vertices[i0 as usize].position;
        let b = vertices[i1 as usize].position;
        let c = vertices[i2 as usize].position;
        let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
        let ac = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
        let normal = [
            ab[1] * ac[2] - ab[2] * ac[1],
            ab[2] * ac[0] - ab[0] * ac[2],
            ab[0] * ac[1] - ab[1] * ac[0],
        ];
        for index in [i0, i1, i2] {
            let vertex_index = index as usize;
            if length_squared(vertices[vertex_index].normal) >= NORMAL_LEN_SQ_EPSILON {
                continue;
            }
            let acc = &mut generated[vertex_index];
            acc[0] += normal[0];
            acc[1] += normal[1];
            acc[2] += normal[2];
        }
    }

    for (vertex, generated_normal) in vertices.iter_mut().zip(generated) {
        vertex.normal = normalize_or_none(vertex.normal)
            .or_else(|| normalize_or_none(generated_normal))
            .unwrap_or([0.0, 1.0, 0.0]);
    }
}