//! Where a page goes on a GDI printer DC, how many pixels its raster gets, and how it is cut
//! into bands. Plain arithmetic on what `GetDeviceCaps` reports, kept free of Windows types so
//! it is tested alone.

/// The highest raster resolution per axis. Every page is a 24-bit bitmap in the spool file:
/// about 26 MB for A4 at 300 dpi, four times that at 600 dpi, and `EndPage`'s documentation warns
/// that a spooled page over about 350 MB may fail to print without an error. 300 dpi keeps the
/// spool small and text legible.
pub(crate) const MAX_DPI: i32 = 300;

/// The most pixels one page's raster may have: 12 MP. US Legal (10.7 MP) and A4 (8.7 MP) fit at
/// 300 dpi; larger sheets are rastered at a lower resolution. A page's 24-bit bitmap takes at
/// most 36 MB plus row padding.
pub(crate) const MAX_PIXELS: u64 = 12_000_000;

/// The most DIB data one `StretchDIBits` call is given: 4 MiB. A page is drawn in horizontal
/// bands of whole rows, so the driver and the spooler never take its whole bitmap at once.
pub(crate) const MAX_BAND_BYTES: u32 = 4 << 20;

/// What `GetDeviceCaps` reports for the printer DC, in device units.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DeviceCaps {
    /// `LOGPIXELSX`, `LOGPIXELSY`.
    pub dpi: (i32, i32),
    /// `PHYSICALWIDTH`, `PHYSICALHEIGHT`: the whole sheet.
    pub physical: (i32, i32),
    /// `PHYSICALOFFSETX`, `PHYSICALOFFSETY`: the sheet edge to the printable area's origin.
    pub offset: (i32, i32),
}

/// A page's destination on the DC and the size of its raster.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Placement {
    /// Left edge in device units, from the printable area's origin (negative: in the margin).
    pub left: i32,
    /// Top edge in device units, from the printable area's origin (negative: in the margin).
    pub top: i32,
    /// Width in device units.
    pub width: i32,
    /// Height in device units.
    pub height: i32,
    /// Raster width and height in pixels; never larger than the destination.
    pub raster: (u32, u32),
}

/// Fit a page of `page` size (any unit; only its shape counts) onto the whole physical sheet,
/// keeping its proportions and centring it. A print-ready page already has the sheet's size,
/// so it fills the sheet. The raster has the device's resolution, capped at [`MAX_DPI`] per
/// axis and [`MAX_PIXELS`] in all. `None` when the page or the device reports an unusable size.
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
    // Fitted in inches: a device can have a different resolution on each axis.
    let inches = |length: i32, dpi: i32| f64::from(length) / f64::from(dpi);
    let scale = f64::min(
        inches(sheet_width, dpi_x) / page_width,
        inches(sheet_height, dpi_y) / page_height,
    );
    let fit = |length: f64, dpi: i32, sheet: i32| -> Option<i32> {
        let device = (length * scale * f64::from(dpi)).round();
        // Clamped to [1, sheet] before the cast, so it is exact.
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
    // Rounded down, at least one pixel each way, and at most `width` × `height`. A side below
    // one pixel is raised to one, so the other is limited again to keep within MAX_PIXELS.
    let x = (raster_x.floor().max(1.0) as u64).min(MAX_PIXELS);
    let y = (raster_y.floor().max(1.0) as u64).min(MAX_PIXELS / x);
    // Both are at most MAX_PIXELS, so they fit in a u32.
    let raster = (u32::try_from(x).ok()?, u32::try_from(y).ok()?);
    Some(Placement {
        left,
        top,
        width,
        height,
        raster,
    })
}

/// The row stride and buffer length of a top-down 24-bit DIB of `width` × `height`: rows are
/// padded to four bytes. `None` when it is empty or over [`MAX_PIXELS`].
pub(crate) fn dib_24bpp(width: u32, height: u32) -> Option<(u32, usize)> {
    if width == 0 || height == 0 || u64::from(width).checked_mul(u64::from(height))? > MAX_PIXELS {
        return None;
    }
    let stride = width.checked_mul(3)?.checked_add(3)? & !3;
    let length = usize::try_from(stride)
        .ok()?
        .checked_mul(usize::try_from(height).ok()?)?;
    Some((stride, length))
}

/// One horizontal band of a page's raster and the device rows it is stretched over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Band {
    /// The band's first raster row.
    pub row: u32,
    /// Its number of raster rows, at least one.
    pub rows: u32,
    /// Top edge on the DC, in device units.
    pub top: i32,
    /// Height on the DC in device units, at least one.
    pub height: i32,
}

/// Cut a raster of `rows` rows of `stride` bytes into bands of at most [`MAX_BAND_BYTES`] (one
/// row when a row is larger), stretched over the `height` device rows from `top`. A band's edges
/// are its raster rows scaled with integer arithmetic, so consecutive bands share edges exactly:
/// no device row is left out or drawn twice. A band that covers no device row (a raster taller
/// than its destination) is left out. `None` when the raster or the destination is empty, or an
/// edge is out of a device coordinate's range.
pub(crate) fn bands(rows: u32, stride: u32, top: i32, height: i32) -> Option<Vec<Band>> {
    if rows == 0 || stride == 0 || height <= 0 {
        return None;
    }
    let per_band = (MAX_BAND_BYTES / stride).max(1);
    // The device row of the edge above raster row `row`: `top` for row 0, `top + height` for
    // `rows`. `None` past the range of a device coordinate.
    let edge = |row: u32| {
        i32::try_from(i64::from(top) + i64::from(row) * i64::from(height) / i64::from(rows)).ok()
    };
    let mut bands = Vec::new();
    let mut row = 0;
    while row < rows {
        let end = row.saturating_add(per_band).min(rows);
        let (from, to) = (edge(row)?, edge(end)?);
        if to > from {
            bands.push(Band {
                row,
                rows: end - row,
                top: from,
                height: to - from,
            });
        }
        row = end;
    }
    Some(bands)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A4 portrait at 600 dpi with 1/6 inch hardware margins, as a laser driver reports it.
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
        // Rastered at 300 dpi and stretched over the 600 dpi sheet.
        assert_eq!(placed.raster, (2480, 3508));
    }

    #[test]
    fn the_raster_is_capped_at_300_dpi_per_axis() {
        // 1200 × 2400 dpi: the sheet is 9922 × 28064 device units, a different shape from A4.
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
    fn a_large_sheet_is_capped_in_pixels_keeping_its_shape() {
        // A2 at 600 dpi: 9921 × 14031 device units, 34.8 MP once rastered at 300 dpi.
        let caps = DeviceCaps {
            dpi: (600, 600),
            physical: (9921, 14031),
            offset: (0, 0),
        };
        let placed = place((1190.55, 1683.78), caps).unwrap();
        let (x, y) = placed.raster;
        assert!(u64::from(x) * u64::from(y) <= MAX_PIXELS);
        assert!(u64::from(x) * u64::from(y) > MAX_PIXELS * 99 / 100);
        let shape = f64::from(x) / f64::from(y);
        assert!((shape - 9921.0 / 14031.0).abs() < 0.001);
    }

    #[test]
    fn a_low_resolution_device_gets_its_own_resolution() {
        let caps = DeviceCaps {
            dpi: (300, 300),
            physical: (2480, 3508),
            offset: (50, 50),
        };
        assert_eq!(place((2480.0, 3508.0), caps).unwrap().raster, (2480, 3508));
        // A4 in points is a hair wider than 2480 × 3508: one row short, never distorted.
        assert_eq!(place((595.28, 841.89), caps).unwrap().raster, (2480, 3507));
    }

    #[test]
    fn a_page_of_another_shape_is_centred_without_distortion() {
        // A landscape page on a portrait sheet: full width, centred vertically.
        let placed = place((841.89, 595.28), A4_600).unwrap();
        assert_eq!(placed.width, 4961);
        assert_eq!(placed.height, 3508);
        assert_eq!(placed.left, -100);
        assert_eq!(placed.top, (7016 - 3508) / 2 - 100);
    }

    #[test]
    fn unusable_sizes_are_refused() {
        assert!(place((0.0, 842.0), A4_600).is_none());
        assert!(place((f64::NAN, 842.0), A4_600).is_none());
        assert!(place((595.0, f64::INFINITY), A4_600).is_none());
        assert!(place((-595.0, 842.0), A4_600).is_none());
        for caps in [
            DeviceCaps {
                dpi: (0, 600),
                ..A4_600
            },
            DeviceCaps {
                physical: (0, 7016),
                ..A4_600
            },
            DeviceCaps {
                physical: (4961, -1),
                ..A4_600
            },
            DeviceCaps {
                offset: (-1, 0),
                ..A4_600
            },
        ] {
            assert!(place((595.28, 841.89), caps).is_none());
        }
        // A subnormal page has no usable shape.
        assert!(place((1.0e-310, 1.0e-310), A4_600).is_none());
    }

    #[test]
    fn extreme_device_values_stay_in_range() {
        let placed = place(
            (595.28, 841.89),
            DeviceCaps {
                offset: (i32::MAX, 0),
                ..A4_600
            },
        )
        .unwrap();
        assert_eq!(placed.left, -i32::MAX);
        let huge = DeviceCaps {
            dpi: (600, 600),
            physical: (i32::MAX, i32::MAX),
            offset: (0, 0),
        };
        let placed = place((1.0, 1.0), huge).unwrap();
        assert_eq!((placed.width, placed.height), (i32::MAX, i32::MAX));
        let (x, y) = placed.raster;
        assert!(u64::from(x) * u64::from(y) <= MAX_PIXELS);
    }

    #[test]
    fn an_extreme_page_shape_on_a_huge_sheet_stays_within_the_cap() {
        let huge = DeviceCaps {
            dpi: (600, 600),
            physical: (i32::MAX, i32::MAX),
            offset: (0, 0),
        };
        for page in [(1.0, 1.0e9), (1.0e9, 1.0)] {
            let (x, y) = place(page, huge).unwrap().raster;
            assert!(x >= 1 && y >= 1);
            assert!(u64::from(x) * u64::from(y) <= MAX_PIXELS);
        }
    }

    #[test]
    fn an_extreme_page_shape_keeps_at_least_one_pixel() {
        let placed = place((1.0, 1.0e9), A4_600).unwrap();
        assert_eq!(placed.width, 1);
        assert_eq!(placed.raster.0, 1);
        assert!(placed.raster.1 >= 1);
    }

    #[test]
    fn dib_rows_are_padded_to_four_bytes() {
        assert_eq!(dib_24bpp(1, 1), Some((4, 4)));
        assert_eq!(dib_24bpp(5, 2), Some((16, 32)));
        assert_eq!(dib_24bpp(2481, 3508), Some((7444, 7444 * 3508)));
    }

    #[test]
    fn dib_sizes_outside_the_cap_are_refused() {
        assert_eq!(dib_24bpp(0, 10), None);
        assert_eq!(dib_24bpp(10, 0), None);
        assert_eq!(dib_24bpp(u32::MAX, u32::MAX), None);
        assert_eq!(dib_24bpp(4000, 3001), None);
        assert!(dib_24bpp(4000, 3000).is_some());
    }

    /// Every raster row is in exactly one band, in order, and the bands tile the destination.
    fn assert_tiles(bands: &[Band], rows: u32, top: i32, height: i32) {
        let mut next_row = 0;
        let mut next_top = top;
        for band in bands {
            assert!(
                band.row >= next_row && band.rows >= 1 && band.height >= 1,
                "{band:?}"
            );
            assert_eq!(band.top, next_top, "{band:?}");
            next_row = band.row + band.rows;
            next_top = band.top + band.height;
        }
        assert_eq!(next_row, rows);
        assert_eq!(next_top, top + height);
    }

    #[test]
    fn an_a4_page_is_drawn_in_bands_of_at_most_4_mib() {
        // A4 at 300 dpi on the 600 dpi sheet: 7440-byte rows, 563 to a band.
        let placed = place((595.28, 841.89), A4_600).unwrap();
        let (stride, _) = dib_24bpp(placed.raster.0, placed.raster.1).unwrap();
        let bands = bands(placed.raster.1, stride, placed.top, placed.height).unwrap();
        assert_eq!(bands.len(), 7);
        assert!(bands.iter().all(|b| b.rows * stride <= MAX_BAND_BYTES));
        assert_eq!(
            bands[0],
            Band {
                row: 0,
                rows: 563,
                top: -100,
                height: 1126
            }
        );
        // Raster rows run back to back, so no row is skipped or drawn twice.
        assert!(bands.windows(2).all(|w| w[0].row + w[0].rows == w[1].row));
        assert_tiles(&bands, 3508, -100, 7016);
    }

    #[test]
    fn band_edges_are_shared_at_any_scale() {
        for (rows, height) in [
            (3508, 7016),
            (3507, 7016),
            (2480, 4961),
            (1, 7016),
            (997, 1000),
        ] {
            for stride in [4, 7440, 14884, MAX_BAND_BYTES, MAX_BAND_BYTES + 4] {
                for top in [-100, 0, 37] {
                    let bands = bands(rows, stride, top, height).unwrap();
                    assert_tiles(&bands, rows, top, height);
                    let per_band = (MAX_BAND_BYTES / stride).max(1);
                    assert!(bands.iter().all(|b| b.rows <= per_band));
                }
            }
        }
    }

    #[test]
    fn a_raster_taller_than_its_destination_leaves_out_empty_bands() {
        // One row per band: three of every four cover no device row.
        let bands = bands(400, MAX_BAND_BYTES, 10, 100).unwrap();
        assert_eq!(bands.len(), 100);
        assert!(bands.iter().all(|b| b.rows == 1 && b.height == 1));
        assert_tiles(&bands, 400, 10, 100);
    }

    #[test]
    fn empty_rasters_and_destinations_have_no_bands() {
        assert_eq!(bands(0, 4, 0, 10), None);
        assert_eq!(bands(10, 0, 0, 10), None);
        assert_eq!(bands(10, 4, 0, 0), None);
        assert_eq!(bands(10, 4, 0, -1), None);
        // An edge past i32::MAX cannot be drawn.
        assert_eq!(bands(10, 4, i32::MAX, 10), None);
    }
}
