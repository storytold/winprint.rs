//! Page placement onto a GDI printer DC and 64-row 2D tight bounding-box white trimming.
//! Kept free of Windows types so layout and pixel-packing logic can be unit tested directly.

use std::collections::TryReserveError;

/// Highest raster resolution per axis (dots per inch).
/// At 300 dpi an A4 sheet is ~8.7 MP (~26 MB 24-bit DIB); keeps text crisp while bounding spool size.
pub(crate) const MAX_DPI: i32 = 300;

/// Maximum raster pixel count per page (12 MP). A4 (8.7 MP) and Legal (10.7 MP) stay at 300 dpi;
/// larger sheets such as Tabloid/A2 scale down proportionally to bound memory and GDI transfer time.
pub(crate) const MAX_PIXELS: u64 = 12_000_000;

/// Maximum source rows inspected per horizontal band before trimming 2D white margins.
pub(crate) const BAND_ROWS: usize = 64;

/// Physical metrics read from a GDI printer device context via `GetDeviceCaps`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DeviceCaps {
    /// `LOGPIXELSX`, `LOGPIXELSY`.
    pub dpi: (i32, i32),
    /// `PHYSICALWIDTH`, `PHYSICALHEIGHT` in device units.
    pub physical: (i32, i32),
    /// `PHYSICALOFFSETX`, `PHYSICALOFFSETY` in device units.
    pub offset: (i32, i32),
}

/// Target destination rectangle on the printer DC and raster dimensions in pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Placement {
    /// Left edge relative to the printable area's origin (may be negative).
    pub left: i32,
    /// Top edge relative to the printable area's origin (may be negative).
    pub top: i32,
    /// Destination width in device units.
    pub width: i32,
    /// Destination height in device units.
    pub height: i32,
    /// Source raster dimensions `(width, height)` in pixels.
    pub raster: (u32, u32),
}

/// One non-white horizontal band tightly cropped around non-white pixels and packed as a
/// 4-byte-aligned bottom-up 24-bit BGR DIB.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PreparedBand {
    pub x0: u32,
    pub y0: u32,
    pub width: u32,
    pub height: u32,
    pub dst_left: i32,
    pub dst_top: i32,
    pub dst_width: i32,
    pub dst_height: i32,
    pub stride: usize,
    pub pixels: Vec<u8>,
}

/// Fit `page` proportionally onto the physical sheet described by `caps` and choose a bounded
/// raster resolution capped at `min(device_dpi, MAX_DPI)` per axis and `MAX_PIXELS` overall.
pub(crate) fn place(page: (f64, f64), caps: DeviceCaps) -> Option<Placement> {
    let (page_width, page_height) = page;
    let usable = |v: f64| v.is_finite() && v > 0.0;
    if !usable(page_width) || !usable(page_height) {
        return None;
    }
    let (sheet_width, sheet_height) = caps.physical;
    let (dpi_x, dpi_y) = caps.dpi;
    let (offset_x, offset_y) = caps.offset;
    if sheet_width <= 0
        || sheet_height <= 0
        || dpi_x <= 0
        || dpi_y <= 0
        || offset_x < 0
        || offset_y < 0
    {
        return None;
    }

    let inches = |length: i32, dpi: i32| f64::from(length) / f64::from(dpi);
    let scale = f64::min(
        inches(sheet_width, dpi_x) / page_width,
        inches(sheet_height, dpi_y) / page_height,
    );
    let fit = |length: f64, dpi: i32, sheet: i32| -> Option<i32> {
        let device = (length * scale * f64::from(dpi)).round();
        device
            .is_finite()
            .then(|| device.clamp(1.0, f64::from(sheet)) as i32)
    };
    let width = fit(page_width, dpi_x, sheet_width)?;
    let height = fit(page_height, dpi_y, sheet_height)?;
    let left = ((sheet_width - width) / 2).checked_sub(offset_x)?;
    let top = ((sheet_height - height) / 2).checked_sub(offset_y)?;

    let per_axis = |length: i32, dpi: i32| {
        f64::from(length) * f64::min(1.0, f64::from(MAX_DPI) / f64::from(dpi))
    };
    let (mut raster_x, mut raster_y) = (per_axis(width, dpi_x), per_axis(height, dpi_y));
    let pixels = raster_x * raster_y;
    if pixels > MAX_PIXELS as f64 {
        let shrink = (MAX_PIXELS as f64 / pixels).sqrt();
        raster_x *= shrink;
        raster_y *= shrink;
    }
    let x = (raster_x.floor().max(1.0) as u64).min(MAX_PIXELS);
    let y = (raster_y.floor().max(1.0) as u64).min(MAX_PIXELS / x);
    let raster = (u32::try_from(x).ok()?, u32::try_from(y).ok()?);
    Some(Placement {
        left,
        top,
        width,
        height,
        raster,
    })
}

/// Map source raster sub-rectangle `(x0, y0, w, h)` to exact device units inside `placement`
/// using integer boundary interpolation so adjacent edges align without gaps or overlap.
pub(crate) fn map_subrect(
    placement: Placement,
    x0: u32,
    y0: u32,
    w: u32,
    h: u32,
) -> Option<(i32, i32, i32, i32)> {
    let (rw, rh) = placement.raster;
    if rw == 0 || rh == 0 || w == 0 || h == 0 || placement.width <= 0 || placement.height <= 0 {
        return None;
    }
    let x1 = x0.checked_add(w)?;
    let y1 = y0.checked_add(h)?;
    if x1 > rw || y1 > rh {
        return None;
    }

    let edge_x = |px: u32| -> Option<i32> {
        i32::try_from(
            i64::from(placement.left)
                + (i64::from(px) * i64::from(placement.width)) / i64::from(rw),
        )
        .ok()
    };
    let edge_y = |py: u32| -> Option<i32> {
        i32::try_from(
            i64::from(placement.top)
                + (i64::from(py) * i64::from(placement.height)) / i64::from(rh),
        )
        .ok()
    };

    let dst_left = edge_x(x0)?;
    let dst_right = edge_x(x1)?;
    let dst_top = edge_y(y0)?;
    let dst_bottom = edge_y(y1)?;
    Some((
        dst_left,
        dst_top,
        (dst_right - dst_left).max(1),
        (dst_bottom - dst_top).max(1),
    ))
}

/// Return the tight horizontal `[min_x, max_x)` range of non-white pixels in one top-down
/// `BGRA` row, or `None` if all pixels are pure white (`B == G == R == 255`).
#[inline]
pub(crate) fn find_row_ink_span(row_bgra: &[u8]) -> Option<(usize, usize)> {
    let pixels = row_bgra.as_chunks::<4>().0;
    let lx = pixels.iter().position(|c| (c[0] & c[1] & c[2]) != 255)?;
    let r_off = pixels[lx..]
        .iter()
        .rposition(|c| (c[0] & c[1] & c[2]) != 255)
        .unwrap_or(0);
    Some((lx, lx + r_off + 1))
}

/// Pack a top-down `BGRA` bitmap (`width` x `height`, `pitch` bytes per source row) into
/// 64-row bands tightly cropped in both X and Y around non-white pixels, formatted as bottom-up
/// 24-bit BGR DIBs (`stride` padded to 4 bytes). Pure-white chunks produce zero bands.
pub(crate) fn pack_trimmed_bands(
    bgra: &[u8],
    pitch: usize,
    width: usize,
    height: usize,
    placement: Placement,
) -> Result<Vec<PreparedBand>, TryReserveError> {
    if width == 0 || height == 0 || pitch < width * 4 {
        return Ok(Vec::new());
    }
    let required_len = (height - 1) * pitch + width * 4;
    if bgra.len() < required_len {
        return Ok(Vec::new());
    }

    let mut bands = Vec::new();
    let mut y_band = 0usize;
    while y_band < height {
        let y_end = (y_band + BAND_ROWS).min(height);
        let mut first_y = None;
        let mut last_y = 0usize;
        let mut min_x = width;
        let mut max_x = 0usize;

        for y in y_band..y_end {
            let row_start = y * pitch;
            let row_slice = &bgra[row_start..row_start + width * 4];
            if let Some((lx, rx)) = find_row_ink_span(row_slice) {
                if first_y.is_none() {
                    first_y = Some(y);
                }
                last_y = y;
                if lx < min_x {
                    min_x = lx;
                }
                if rx > max_x {
                    max_x = rx;
                }
            }
        }

        if let Some(ay0) = first_y {
            let ay1 = last_y + 1;
            let sub_w = max_x - min_x;
            let sub_h = ay1 - ay0;
            if let Some((dst_left, dst_top, dst_width, dst_height)) = map_subrect(
                placement,
                min_x as u32,
                ay0 as u32,
                sub_w as u32,
                sub_h as u32,
            ) {
                let stride = (sub_w * 3 + 3) & !3;
                let total_bytes = stride * sub_h;
                let mut pixels = Vec::new();
                pixels.try_reserve_exact(total_bytes)?;
                pixels.resize(total_bytes, 0);

                // Bottom-up DIB storage order for positive `biHeight`.
                for r in 0..sub_h {
                    let sy = ay0 + r;
                    let dy = sub_h - 1 - r;
                    let srow_start = sy * pitch + min_x * 4;
                    let srow = &bgra[srow_start..srow_start + sub_w * 4];
                    let drow = &mut pixels[dy * stride..dy * stride + sub_w * 3];
                    for (dst, src) in drow
                        .as_chunks_mut::<3>()
                        .0
                        .iter_mut()
                        .zip(srow.as_chunks::<4>().0.iter())
                    {
                        dst[0] = src[0];
                        dst[1] = src[1];
                        dst[2] = src[2];
                    }
                }

                bands.push(PreparedBand {
                    x0: min_x as u32,
                    y0: ay0 as u32,
                    width: sub_w as u32,
                    height: sub_h as u32,
                    dst_left,
                    dst_top,
                    dst_width,
                    dst_height,
                    stride,
                    pixels,
                });
            }
        }

        y_band = y_end;
    }

    Ok(bands)
}

#[cfg(test)]
mod tests {
    use super::*;

    const A4_600: DeviceCaps = DeviceCaps {
        dpi: (600, 600),
        physical: (4961, 7016),
        offset: (100, 100),
    };

    #[test]
    fn a_sheet_sized_page_fills_the_physical_sheet() {
        let placed = place((595.28, 841.89), A4_600).unwrap();
        assert_eq!((placed.left, placed.top), (-100, -100));
        assert_eq!((placed.width, placed.height), (4961, 7016));
        assert_eq!(placed.raster, (2480, 3508));
    }

    #[test]
    fn high_dpi_device_is_capped_at_300_dpi_per_axis() {
        let caps = DeviceCaps {
            dpi: (1200, 2400),
            physical: (9922, 28064),
            offset: (0, 0),
        };
        let placed = place((595.28, 841.89), caps).unwrap();
        assert_eq!((placed.width, placed.height), (9922, 28064));
        assert_eq!(placed.raster, (2480, 3508));
    }

    #[test]
    fn large_sheet_is_capped_at_max_pixels_keeping_aspect_ratio() {
        let caps = DeviceCaps {
            dpi: (600, 600),
            physical: (9921, 14031),
            offset: (0, 0),
        };
        let placed = place((1190.55, 1683.78), caps).unwrap();
        let (rx, ry) = placed.raster;
        assert!(u64::from(rx) * u64::from(ry) <= MAX_PIXELS);
        assert!(u64::from(rx) * u64::from(ry) > MAX_PIXELS * 99 / 100);
        let ratio_err = (f64::from(rx) / f64::from(ry) - 9921.0 / 14031.0).abs();
        assert!(ratio_err < 0.001, "ratio_err={ratio_err}");
    }

    #[test]
    fn low_dpi_device_keeps_its_native_resolution() {
        let caps = DeviceCaps {
            dpi: (150, 150),
            physical: (1240, 1754),
            offset: (0, 0),
        };
        let placed = place((595.28, 841.89), caps).unwrap();
        assert_eq!(placed.raster, (1240, 1754));
    }

    #[test]
    fn unusable_dimensions_return_none() {
        assert_eq!(place((0.0, 100.0), A4_600), None);
        assert_eq!(place((f64::NAN, 100.0), A4_600), None);
        let bad_caps = DeviceCaps {
            dpi: (0, 600),
            ..A4_600
        };
        assert_eq!(place((100.0, 100.0), bad_caps), None);
        let neg_caps = DeviceCaps {
            physical: (-10, 100),
            ..A4_600
        };
        assert_eq!(place((100.0, 100.0), neg_caps), None);
    }

    #[test]
    fn all_white_bitmap_produces_no_bands() {
        let w = 100usize;
        let h = 150usize;
        let bgra = vec![255u8; w * h * 4];
        let placed = Placement {
            left: 0,
            top: 0,
            width: 200,
            height: 300,
            raster: (w as u32, h as u32),
        };
        let bands = pack_trimmed_bands(&bgra, w * 4, w, h, placed).unwrap();
        assert!(bands.is_empty());
    }

    #[test]
    fn single_pixel_corners_trim_to_1x1_with_padded_4_byte_stride() {
        let w = 100usize;
        let h = 130usize;
        let mut bgra = vec![255u8; w * h * 4];
        // Ink at (0, 0): red (BGRA = 0, 0, 255, 255)
        bgra[0..4].copy_from_slice(&[0, 0, 255, 255]);
        // Ink at (99, 129): blue (BGRA = 255, 0, 0, 255)
        let last_off = (129 * w + 99) * 4;
        bgra[last_off..last_off + 4].copy_from_slice(&[255, 0, 0, 255]);

        let placed = Placement {
            left: -10,
            top: -20,
            width: 200,
            height: 260,
            raster: (w as u32, h as u32),
        };
        let bands = pack_trimmed_bands(&bgra, w * 4, w, h, placed).unwrap();
        assert_eq!(bands.len(), 2);

        assert_eq!(
            (bands[0].x0, bands[0].y0, bands[0].width, bands[0].height),
            (0, 0, 1, 1)
        );
        assert_eq!(bands[0].stride, 4);
        assert_eq!(bands[0].pixels, vec![0, 0, 255, 0]);
        assert_eq!(
            (
                bands[0].dst_left,
                bands[0].dst_top,
                bands[0].dst_width,
                bands[0].dst_height
            ),
            (-10, -20, 2, 2)
        );

        assert_eq!(
            (bands[1].x0, bands[1].y0, bands[1].width, bands[1].height),
            (99, 129, 1, 1)
        );
        assert_eq!(bands[1].stride, 4);
        assert_eq!(bands[1].pixels, vec![255, 0, 0, 0]);
        assert_eq!(
            (
                bands[1].dst_left,
                bands[1].dst_top,
                bands[1].dst_width,
                bands[1].dst_height
            ),
            (-10 + 198, -20 + 258, 2, 2)
        );
    }

    #[test]
    fn band_pixels_are_bottom_up_24bpp_bgr_and_4byte_padded() {
        let w = 5usize;
        let h = 2usize;
        let pitch = 24usize; // non-tight source pitch (> 5*4 = 20)
        let mut bgra = vec![255u8; pitch * h];
        // Row 0 (top), x=1: Green (0, 255, 0, 255)
        bgra[1 * 4..2 * 4].copy_from_slice(&[0, 255, 0, 255]);
        // Row 1 (bottom), x=2: Red (0, 0, 255, 255)
        bgra[pitch + 2 * 4..pitch + 3 * 4].copy_from_slice(&[0, 0, 255, 255]);

        let placed = Placement {
            left: 0,
            top: 0,
            width: 10,
            height: 4,
            raster: (w as u32, h as u32),
        };
        let bands = pack_trimmed_bands(&bgra, pitch, w, h, placed).unwrap();
        assert_eq!(bands.len(), 1);
        let b = &bands[0];
        assert_eq!((b.x0, b.y0, b.width, b.height), (1, 0, 2, 2));
        // sub_w = 2 -> 6 bytes RGB -> padded to 8
        assert_eq!(b.stride, 8);
        assert_eq!(b.pixels.len(), 16);
        // Stored row 0 is source bottom row (y=1): x=1 white (255,255,255), x=2 Red BGR (0,0,255)
        assert_eq!(&b.pixels[0..6], &[255, 255, 255, 0, 0, 255]);
        // Stored row 1 is source top row (y=0): x=1 Green BGR (0,255,0), x=2 white (255,255,255)
        assert_eq!(&b.pixels[8..14], &[0, 255, 0, 255, 255, 255]);
    }

    #[test]
    fn adjacent_full_bands_meet_edge_to_edge_without_gaps_or_overlaps() {
        let w = 2480usize;
        let h = 200usize;
        let mut bgra = vec![10u8; w * h * 4]; // all dark so every 64-row chunk emits full height
        for px in bgra.chunks_exact_mut(4) {
            px[3] = 255;
        }
        let placed = Placement {
            left: -100,
            top: -100,
            width: 4961,
            height: 701,
            raster: (w as u32, h as u32),
        };
        let bands = pack_trimmed_bands(&bgra, w * 4, w, h, placed).unwrap();
        assert_eq!(bands.len(), 4); // 64 + 64 + 64 + 8
        assert_eq!(bands[0].dst_top, placed.top);
        for pair in bands.windows(2) {
            assert_eq!(pair[0].dst_top + pair[0].dst_height, pair[1].dst_top);
        }
        let last = bands.last().unwrap();
        assert_eq!(last.dst_top + last.dst_height, placed.top + placed.height);
    }

    #[test]
    fn find_row_ink_span_handles_alpha_and_interior_gaps() {
        let mut row = vec![255u8; 8 * 4];
        // Transparent pixel with white BGR stays skipped
        row[1 * 4 + 3] = 0;
        assert_eq!(find_row_ink_span(&row), None);

        // Two separated dark pixels at x=2 and x=6
        row[2 * 4] = 200;
        row[6 * 4 + 2] = 0;
        assert_eq!(find_row_ink_span(&row), Some((2, 7)));
    }
}
