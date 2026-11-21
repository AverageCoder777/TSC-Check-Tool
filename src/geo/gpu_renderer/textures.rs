use std::borrow::Cow;
use std::sync::Arc;

pub(super) fn create_texture_bind_groups(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    textures: &[Arc<super::super::viewer3d::Texture>],
    max_texture_dimension: u32,
) -> Result<(wgpu::BindGroupLayout, Vec<wgpu::BindGroup>), String> {
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("GeoPcDx texture bindings"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    });

    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        address_mode_u: wgpu::AddressMode::Repeat,
        address_mode_v: wgpu::AddressMode::Repeat,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        mipmap_filter: wgpu::FilterMode::Linear,
        anisotropy_clamp: 16,
        ..Default::default()
    });

    let mut bind_groups = Vec::with_capacity(textures.len());
    for texture_data in textures {
        let view = upload_texture(device, queue, texture_data, max_texture_dimension)?;
        bind_groups.push(device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("GeoPcDx texture bindings"),
            layout: &layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        }));
    }

    Ok((layout, bind_groups))
}

fn upload_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture_data: &super::super::viewer3d::Texture,
    max_texture_dimension: u32,
) -> Result<wgpu::TextureView, String> {
    let expected_bytes = (texture_data.width as usize)
        .checked_mul(texture_data.height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or("texture dimensions overflow")?;
    if texture_data.width == 0
        || texture_data.height == 0
        || expected_bytes != texture_data.pixels.len()
    {
        return Err("invalid RGBA texture dimensions or data length".into());
    }

    let (width, height, pixels) = if texture_data.width > max_texture_dimension
        || texture_data.height > max_texture_dimension
    {
        let (sw, sh, scaled) = downscale_rgba(
            texture_data.width,
            texture_data.height,
            &texture_data.pixels,
            max_texture_dimension,
        )?;
        (sw, sh, Cow::Owned(scaled))
    } else {
        (
            texture_data.width,
            texture_data.height,
            Cow::Borrowed(texture_data.pixels.as_slice()),
        )
    };

    let mip_levels = mip_level_count(width, height);
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("GeoPcDx albedo"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: mip_levels,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });

    let mut mip_w = width;
    let mut mip_h = height;
    let mut mip_data = pixels;
    for level in 0..mip_levels {
        queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &texture,
                mip_level: level,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            mip_data.as_ref(),
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(mip_w * 4),
                rows_per_image: Some(mip_h),
            },
            wgpu::Extent3d {
                width: mip_w,
                height: mip_h,
                depth_or_array_layers: 1,
            },
        );
        if level + 1 < mip_levels {
            let (next_w, next_h, next_pixels) =
                downsample_mip_rgba(mip_w, mip_h, mip_data.as_ref());
            mip_w = next_w;
            mip_h = next_h;
            mip_data = Cow::Owned(next_pixels);
        }
    }

    Ok(texture.create_view(&wgpu::TextureViewDescriptor::default()))
}

fn mip_level_count(width: u32, height: u32) -> u32 {
    32 - width.max(height).leading_zeros()
}

fn downsample_mip_rgba(src_w: u32, src_h: u32, src: &[u8]) -> (u32, u32, Vec<u8>) {
    let dst_w = (src_w / 2).max(1);
    let dst_h = (src_h / 2).max(1);
    let src_stride = (src_w as usize) * 4;
    let max_x = (src_w as usize) - 1;
    let max_y = (src_h as usize) - 1;

    let mut dst = Vec::with_capacity((dst_w as usize) * (dst_h as usize) * 4);
    for y in 0..dst_h as usize {
        let sy0 = (y * 2).min(max_y);
        let sy1 = (sy0 + 1).min(max_y);
        let row0 = &src[sy0 * src_stride..(sy0 + 1) * src_stride];
        let row1 = &src[sy1 * src_stride..(sy1 + 1) * src_stride];
        for x in 0..dst_w as usize {
            let sx0 = (x * 2).min(max_x) * 4;
            let sx1 = (x * 2 + 1).min(max_x) * 4;
            let p00 = &row0[sx0..sx0 + 4];
            let p10 = &row0[sx1..sx1 + 4];
            let p01 = &row1[sx0..sx0 + 4];
            let p11 = &row1[sx1..sx1 + 4];
            dst.extend_from_slice(&[
                ((p00[0] as u16 + p10[0] as u16 + p01[0] as u16 + p11[0] as u16 + 2) >> 2) as u8,
                ((p00[1] as u16 + p10[1] as u16 + p01[1] as u16 + p11[1] as u16 + 2) >> 2) as u8,
                ((p00[2] as u16 + p10[2] as u16 + p01[2] as u16 + p11[2] as u16 + 2) >> 2) as u8,
                ((p00[3] as u16 + p10[3] as u16 + p01[3] as u16 + p11[3] as u16 + 2) >> 2) as u8,
            ]);
        }
    }
    (dst_w, dst_h, dst)
}

pub(super) fn downscale_rgba(
    width: u32,
    height: u32,
    pixels: &[u8],
    max_dimension: u32,
) -> Result<(u32, u32, Vec<u8>), String> {
    if width == 0 || height == 0 || max_dimension == 0 {
        return Err("invalid RGBA texture dimensions".into());
    }
    let expected_bytes = (width as usize)
        .checked_mul(height as usize)
        .and_then(|count| count.checked_mul(4))
        .ok_or("texture dimensions overflow")?;
    if pixels.len() != expected_bytes {
        return Err("invalid RGBA texture dimensions or data length".into());
    }
    let largest_dimension = width.max(height);
    if largest_dimension <= max_dimension {
        return Ok((width, height, pixels.to_vec()));
    }

    let scale = max_dimension as f64 / largest_dimension as f64;
    let scaled_width = ((width as f64 * scale).floor() as u32).max(1);
    let scaled_height = ((height as f64 * scale).floor() as u32).max(1);
    let scaled_len = (scaled_width as usize)
        .checked_mul(scaled_height as usize)
        .and_then(|count| count.checked_mul(4))
        .ok_or("scaled texture dimensions overflow")?;

    let src_stride = (width as usize) * 4;
    let mut scaled = Vec::with_capacity(scaled_len);
    for y in 0..scaled_height {
        let src_y = (u64::from(y) * u64::from(height) / u64::from(scaled_height)) as usize;
        let src_row = &pixels[src_y * src_stride..(src_y + 1) * src_stride];
        for x in 0..scaled_width {
            let src_x = (u64::from(x) * u64::from(width) / u64::from(scaled_width)) as usize * 4;
            scaled.extend_from_slice(&src_row[src_x..src_x + 4]);
        }
    }
    Ok((scaled_width, scaled_height, scaled))
}
