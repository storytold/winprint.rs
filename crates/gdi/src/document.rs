// SPDX-License-Identifier: MIT OR Apache-2.0
//! A print job sent page by page: each page is started, drawn with pixels and metafiles where the
//! caller places them, and ended before the next one is made, so no job holds every page in
//! memory. A page may change the paper or orientation (`ResetDC`) before it starts.

use crate::{ColorPolicy, DevMode, Page, PrintError, Result, RgbImage};

/// The largest metafile one page plays.
pub const MAX_METAFILE_BYTES: usize = 512 * 1024 * 1024;
/// The largest image one call draws.
pub const MAX_IMAGE_BYTES: usize = 512 * 1024 * 1024;

/// `EMR_HEADER` record type, the `" EMF"` signature, and the header's smallest size ([MS-EMF]
/// 2.3.4.2).
const EMR_HEADER: u32 = 1;
const EMF_SIGNATURE: u32 = 0x464D_4520;
const EMF_HEADER_MIN: usize = 88;

/// How a [`Document`] is opened.
pub struct DocumentJob<'a> {
    pub printer: &'a str,
    pub settings: Option<&'a DevMode>,
    /// The name in the print queue.
    pub title: &'a str,
    /// Print to this file instead of the device (drivers that write files, and tests).
    pub output_file: Option<&'a str>,
    pub color_policy: ColorPolicy,
    /// The caller sends every copy as pages: the driver is told to make one copy, and a driver
    /// that refuses that fails the job before anything is sent.
    pub manual_copies: bool,
}

/// An open print job. Dropping it before [`Document::finish`] cancels the job.
pub struct Document {
    #[cfg(windows)]
    inner: crate::windows::document::Doc,
    #[cfg(not(windows))]
    _never: std::convert::Infallible,
}

impl Document {
    /// Opens the printer and starts the job. The driver validates `job.settings` first and a
    /// refusal is an error, never a job on the defaults.
    pub fn start(job: &DocumentJob<'_>) -> Result<Self> {
        #[cfg(windows)]
        {
            Ok(Self {
                inner: crate::windows::document::Doc::start(job)?,
            })
        }
        #[cfg(not(windows))]
        {
            let _ = job;
            Err(PrintError::Unsupported)
        }
    }

    /// The sheet the next page prints on, as the current settings make it.
    pub fn page(&self) -> Page {
        #[cfg(windows)]
        {
            self.inner.page()
        }
        #[cfg(not(windows))]
        match self._never {}
    }

    /// Starts a page. `settings` (paper, orientation) replace the current ones from this page on;
    /// the driver validates them first. Returns the sheet the page prints on.
    pub fn begin_page(&mut self, settings: Option<&DevMode>) -> Result<Page> {
        #[cfg(windows)]
        {
            self.inner.begin_page(settings)
        }
        #[cfg(not(windows))]
        {
            let _ = settings;
            match self._never {}
        }
    }

    /// Draws `image` into `rect` (points from the sheet's top-left: x, y, width, height). An image
    /// at the rect's device pixels is copied as it is; a scaled one is resampled with halftoning.
    pub fn draw_image(&mut self, image: &RgbImage<'_>, rect: (f64, f64, f64, f64)) -> Result<()> {
        check_image(image)?;
        check_rect(rect)?;
        #[cfg(windows)]
        {
            self.inner.draw_image(image, rect)
        }
        #[cfg(not(windows))]
        match self._never {}
    }

    /// Copies `image` pixel for pixel with its top-left corner at `at`, in device pixels from the
    /// printable area's corner (as [`crate::device_rect`] maps points). Bands of one picture
    /// rendered at the device's resolution meet exactly.
    pub fn draw_pixels(&mut self, image: &RgbImage<'_>, at: (i32, i32)) -> Result<()> {
        check_image(image)?;
        #[cfg(windows)]
        {
            self.inner.draw_pixels(image, at)
        }
        #[cfg(not(windows))]
        {
            let _ = at;
            match self._never {}
        }
    }

    /// Plays an enhanced metafile into `rect` (points from the sheet's top-left), its header frame
    /// stretched onto the rect. Vector content reaches the driver as vector calls.
    pub fn play_metafile(&mut self, emf: &[u8], rect: (f64, f64, f64, f64)) -> Result<()> {
        check_emf(emf)?;
        check_rect(rect)?;
        #[cfg(windows)]
        {
            self.inner.play_metafile(emf, rect)
        }
        #[cfg(not(windows))]
        match self._never {}
    }

    /// Ends the page started last.
    pub fn end_page(&mut self) -> Result<()> {
        #[cfg(windows)]
        {
            self.inner.end_page()
        }
        #[cfg(not(windows))]
        match self._never {}
    }

    /// Ends the job (a page still open is ended first).
    pub fn finish(self) -> Result<()> {
        #[cfg(windows)]
        {
            self.inner.finish()
        }
        #[cfg(not(windows))]
        match self._never {}
    }
}

pub(crate) fn check_image(image: &RgbImage<'_>) -> Result<()> {
    let len = (image.width as usize)
        .checked_mul(3)
        .and_then(|row| row.checked_mul(image.height as usize));
    if image.width == 0
        || image.height == 0
        || image.width > i32::MAX as u32
        || image.height > i32::MAX as u32
        || len != Some(image.data.len())
    {
        return Err(PrintError::Invalid("nothing to print".into()));
    }
    if image.data.len() > MAX_IMAGE_BYTES {
        return Err(PrintError::Invalid(
            "print image exceeds supported limits".into(),
        ));
    }
    Ok(())
}

pub(crate) fn check_rect(rect: (f64, f64, f64, f64)) -> Result<()> {
    let (x, y, w, h) = rect;
    if [x, y, w, h].iter().all(|v| v.is_finite()) && w > 0.0 && h > 0.0 {
        Ok(())
    } else {
        Err(PrintError::Invalid("the print size is empty".into()))
    }
}

/// An EMF the system will parse: it starts with a header record whose signature is right and
/// whose stated file size is the length given.
pub(crate) fn check_emf(emf: &[u8]) -> Result<()> {
    let word = |at: usize| {
        emf.get(at..at + 4)
            .and_then(|b| <[u8; 4]>::try_from(b).ok())
            .map(u32::from_le_bytes)
    };
    let ok = emf.len() >= EMF_HEADER_MIN
        && emf.len() <= MAX_METAFILE_BYTES
        && word(0) == Some(EMR_HEADER)
        && word(4)
            .is_some_and(|size| size as usize >= EMF_HEADER_MIN && size as usize <= emf.len())
        && word(40) == Some(EMF_SIGNATURE)
        && word(48).map(|n| n as usize) == Some(emf.len());
    if ok {
        Ok(())
    } else {
        Err(PrintError::Invalid(
            "the page picture is not a valid metafile".into(),
        ))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
pub(crate) mod tests {
    use super::*;

    /// A one-rectangle EMF: header, `EMR_RECTANGLE` (0,0)-(100,50), `EMR_EOF`.
    pub(crate) fn emf_with_a_rectangle() -> Vec<u8> {
        let mut out = Vec::new();
        let put = |out: &mut Vec<u8>, v: u32| out.extend_from_slice(&v.to_le_bytes());
        let total = 88 + 24 + 20;
        put(&mut out, EMR_HEADER);
        put(&mut out, 88);
        for v in [0i32, 0, 100, 50] {
            put(&mut out, v as u32); // rclBounds, device units
        }
        for v in [0i32, 0, 2540, 1270] {
            put(&mut out, v as u32); // rclFrame, 0.01 mm
        }
        put(&mut out, EMF_SIGNATURE);
        put(&mut out, 0x0001_0000); // version
        put(&mut out, total); // nBytes
        put(&mut out, 3); // nRecords
        out.extend_from_slice(&1u16.to_le_bytes()); // nHandles
        out.extend_from_slice(&0u16.to_le_bytes());
        for v in [0u32, 0, 0, 1000, 1000, 254, 254] {
            put(&mut out, v); // description, palette, reference device (pixels, mm)
        }
        assert_eq!(out.len(), 88);
        put(&mut out, 43); // EMR_RECTANGLE
        put(&mut out, 24);
        for v in [0i32, 0, 100, 50] {
            put(&mut out, v as u32);
        }
        put(&mut out, 14); // EMR_EOF
        put(&mut out, 20);
        for v in [0u32, 16, 20] {
            put(&mut out, v);
        }
        out
    }

    #[test]
    fn only_a_whole_metafile_with_its_header_is_played() {
        let good = emf_with_a_rectangle();
        assert_eq!(check_emf(&good), Ok(()));
        let mut truncated = good.clone();
        truncated.pop();
        let mut not_emf = good.clone();
        not_emf[40] = b'X';
        let mut no_header = good.clone();
        no_header[0] = 2;
        let mut header_too_long = good.clone();
        header_too_long[4..8].copy_from_slice(&10_000u32.to_le_bytes());
        for bad in [
            vec![],
            good[..87].to_vec(),
            truncated,
            not_emf,
            no_header,
            header_too_long,
        ] {
            assert!(matches!(check_emf(&bad), Err(PrintError::Invalid(_))));
        }
    }

    #[test]
    fn images_and_rects_are_checked_before_any_system_call() {
        let data = [0u8; 12];
        let ok = RgbImage {
            width: 2,
            height: 2,
            data: &data,
        };
        assert_eq!(check_image(&ok), Ok(()));
        for (width, height, len) in [(2, 2, 11), (0, 2, 0), (2, 0, 0), (u32::MAX, 1, 12)] {
            let data = vec![0u8; len];
            let bad = RgbImage {
                width,
                height,
                data: &data,
            };
            assert!(check_image(&bad).is_err(), "{width}x{height}");
        }
        assert_eq!(check_rect((0.0, 0.0, 1.0, 1.0)), Ok(()));
        for bad in [
            (f64::NAN, 0.0, 1.0, 1.0),
            (0.0, 0.0, 0.0, 1.0),
            (0.0, 0.0, 1.0, -1.0),
            (0.0, 0.0, f64::INFINITY, 1.0),
        ] {
            assert!(check_rect(bad).is_err());
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn documents_need_windows() {
        let job = DocumentJob {
            printer: "x",
            settings: None,
            title: "t",
            output_file: None,
            color_policy: ColorPolicy::Unmanaged,
            manual_copies: true,
        };
        assert!(matches!(
            Document::start(&job),
            Err(PrintError::Unsupported)
        ));
    }
}
