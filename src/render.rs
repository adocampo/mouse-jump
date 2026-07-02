/// Screenshot decoding and scaling for the overlay thumbnail.
use anyhow::{Context, Result};
use image::GenericImageView;

/// Decode a PNG screenshot and scale it to fit within the given dimensions.
/// Returns RGBA pixel data and the actual (width, height) of the scaled image.
pub fn decode_and_scale(png_data: &[u8], target_width: u32, target_height: u32) -> Result<(Vec<u8>, u32, u32)> {
    let img = image::load_from_memory_with_format(png_data, image::ImageFormat::Png)
        .context("Failed to decode PNG screenshot")?;

    let (orig_w, orig_h) = img.dimensions();
    log::debug!("Original screenshot: {}x{}", orig_w, orig_h);

    // Calculate scaled dimensions maintaining aspect ratio
    let aspect = orig_w as f64 / orig_h as f64;
    let (scaled_w, scaled_h) = if target_width as f64 / target_height as f64 > aspect {
        let h = target_height;
        let w = (h as f64 * aspect) as u32;
        (w, h)
    } else {
        let w = target_width;
        let h = (w as f64 / aspect) as u32;
        (w, h)
    };

    let scaled_w = scaled_w.max(1);
    let scaled_h = scaled_h.max(1);

    log::debug!("Scaled thumbnail: {}x{}", scaled_w, scaled_h);

    // Resize using fastest filter (Nearest is much faster for large downsample ratios)
    let resized = image::imageops::resize(
        &img.to_rgba8(),
        scaled_w,
        scaled_h,
        image::imageops::FilterType::Nearest,
    );

    // Convert to BGRA (wayland wl_shm expects ARGB8888 which is BGRA in little-endian)
    let mut bgra_pixels = Vec::with_capacity((scaled_w * scaled_h * 4) as usize);
    for pixel in resized.pixels() {
        let [r, g, b, a] = pixel.0;
        bgra_pixels.push(b);
        bgra_pixels.push(g);
        bgra_pixels.push(r);
        bgra_pixels.push(a);
    }

    Ok((bgra_pixels, scaled_w, scaled_h))
}

/// Add a subtle border/frame around the thumbnail to make it visually distinct.
/// Modifies the pixel buffer in-place.
pub fn add_border(pixels: &mut [u8], width: u32, height: u32, border_width: u32) {
    let border_color: [u8; 4] = [0x40, 0x40, 0x40, 0xFF]; // Dark gray in BGRA

    for y in 0..height {
        for x in 0..width {
            if x < border_width || x >= width - border_width
                || y < border_width || y >= height - border_width
            {
                let offset = ((y * width + x) * 4) as usize;
                if offset + 3 < pixels.len() {
                    pixels[offset] = border_color[0];
                    pixels[offset + 1] = border_color[1];
                    pixels[offset + 2] = border_color[2];
                    pixels[offset + 3] = border_color[3];
                }
            }
        }
    }
}

/// Draw a crosshair indicator at a specific position on the thumbnail.
/// Used to show where the cursor will land before clicking.
pub fn draw_crosshair(pixels: &mut [u8], width: u32, height: u32, cx: u32, cy: u32) {
    let color: [u8; 4] = [0x00, 0x00, 0xFF, 0xCC]; // Red in BGRA with some transparency
    let size = 10u32;

    // Horizontal line
    let y = cy;
    if y < height {
        let x_start = cx.saturating_sub(size);
        let x_end = (cx + size).min(width - 1);
        for x in x_start..=x_end {
            let offset = ((y * width + x) * 4) as usize;
            if offset + 3 < pixels.len() {
                pixels[offset] = color[0];
                pixels[offset + 1] = color[1];
                pixels[offset + 2] = color[2];
                pixels[offset + 3] = color[3];
            }
        }
    }

    // Vertical line
    let x = cx;
    if x < width {
        let y_start = cy.saturating_sub(size);
        let y_end = (cy + size).min(height - 1);
        for y in y_start..=y_end {
            let offset = ((y * width + x) * 4) as usize;
            if offset + 3 < pixels.len() {
                pixels[offset] = color[0];
                pixels[offset + 1] = color[1];
                pixels[offset + 2] = color[2];
                pixels[offset + 3] = color[3];
            }
        }
    }
}

/// Compose a fullscreen overlay buffer: semi-transparent dark background with the
/// thumbnail drawn at the specified offset. Returns BGRA pixel data for the full surface.
pub fn compose_overlay(
    thumb_pixels: &[u8],
    thumb_w: u32,
    thumb_h: u32,
    offset_x: u32,
    offset_y: u32,
    surface_w: u32,
    surface_h: u32,
) -> Vec<u8> {
    let total_pixels = (surface_w * surface_h * 4) as usize;
    let mut buffer = vec![0u8; total_pixels];

    // Fill with semi-transparent dark background (BGRA)
    for chunk in buffer.chunks_exact_mut(4) {
        chunk[0] = 0x00; // B
        chunk[1] = 0x00; // G
        chunk[2] = 0x00; // R
        chunk[3] = 0x80; // A (50% opacity)
    }

    // Draw a 2px border around thumbnail area
    let border = 2u32;
    let bx = offset_x.saturating_sub(border);
    let by = offset_y.saturating_sub(border);
    let bw = thumb_w + border * 2;
    let bh = thumb_h + border * 2;
    for y in by..(by + bh).min(surface_h) {
        for x in bx..(bx + bw).min(surface_w) {
            let in_thumb = x >= offset_x && x < offset_x + thumb_w
                && y >= offset_y && y < offset_y + thumb_h;
            if !in_thumb {
                let offset = ((y * surface_w + x) * 4) as usize;
                buffer[offset] = 0x80;     // B
                buffer[offset + 1] = 0x80; // G
                buffer[offset + 2] = 0x80; // R
                buffer[offset + 3] = 0xFF; // A
            }
        }
    }

    // Blit thumbnail pixels into position
    for ty in 0..thumb_h {
        let dst_y = offset_y + ty;
        if dst_y >= surface_h {
            break;
        }
        let src_row_start = (ty * thumb_w * 4) as usize;
        let src_row_end = src_row_start + (thumb_w * 4) as usize;
        if src_row_end > thumb_pixels.len() {
            break;
        }
        let dst_row_start = ((dst_y * surface_w + offset_x) * 4) as usize;
        let copy_w = thumb_w.min(surface_w - offset_x) as usize * 4;
        buffer[dst_row_start..dst_row_start + copy_w]
            .copy_from_slice(&thumb_pixels[src_row_start..src_row_start + copy_w]);
    }

    buffer
}
