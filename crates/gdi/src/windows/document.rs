// SPDX-License-Identifier: MIT OR Apache-2.0
//! The GDI side of [`crate::Document`]: one printer DC, one started document, pages begun and
//! ended in turn.

use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::RECT;
use windows_sys::Win32::Graphics::Gdi::{
    COLORONCOLOR, DEVMODEW, DIB_RGB_COLORS, DeleteEnhMetaFile, HALFTONE, PlayEnhMetaFile, ResetDCW,
    SRCCOPY, SetBrushOrgEx, SetEnhMetaFileBits, SetStretchBltMode, StretchDIBits,
};
use windows_sys::Win32::Storage::Xps::{AbortDoc, DOCINFOW, EndDoc, EndPage, StartDocW, StartPage};
use windows_sys::Win32::UI::ColorSystem::{ICM_DONE_OUTSIDEDC, ICM_MODE, ICM_OFF, SetICMMode};

use super::{Dc, aligned_copy, dib, last_error, properties, wide};
use crate::{ColorPolicy, DevMode, DocumentJob, Page, PrintError, Result, RgbImage};

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Between,
    InPage,
    Closed,
}

pub struct Doc {
    dc: Dc,
    page: Page,
    state: State,
    color_mode: ICM_MODE,
    manual_copies: bool,
    printer: String,
}

fn closed() -> PrintError {
    PrintError::Invalid("the print job has already ended".into())
}

impl Doc {
    pub fn start(job: &DocumentJob<'_>) -> Result<Self> {
        let dc = Dc::create(job.printer, job.settings, job.manual_copies)?;
        let page = dc.page()?;
        let title = wide(job.title)?;
        let output = job.output_file.map(wide).transpose()?;
        let info = DOCINFOW {
            cbSize: std::mem::size_of::<DOCINFOW>() as i32,
            lpszDocName: title.as_ptr(),
            lpszOutput: output.as_ref().map_or(null(), |o| o.as_ptr()),
            lpszDatatype: null(),
            fwType: 0,
        };
        let color_mode = match job.color_policy {
            ColorPolicy::DriverManaged => ICM_OFF,
            ColorPolicy::ApplicationManaged | ColorPolicy::Unmanaged => ICM_DONE_OUTSIDEDC,
        };
        // SAFETY: `info` and the strings it points to are alive for the call; StartDoc copies them.
        if unsafe { StartDocW(dc.0, &info) } <= 0 {
            return Err(last_error("starting the print job"));
        }
        Ok(Doc {
            dc,
            page,
            state: State::Between,
            color_mode,
            manual_copies: job.manual_copies,
            printer: job.printer.to_owned(),
        })
    }

    pub fn page(&self) -> Page {
        self.page
    }

    /// Cancels the job and hands back `error`.
    fn abort(&mut self, error: PrintError) -> PrintError {
        if self.state != State::Closed {
            self.state = State::Closed;
            // SAFETY: the document was started on this DC and is aborted once.
            unsafe { AbortDoc(self.dc.0) };
        }
        error
    }

    fn in_page(&self) -> Result<()> {
        match self.state {
            State::InPage => Ok(()),
            State::Between => Err(PrintError::Invalid("no page has been started".into())),
            State::Closed => Err(closed()),
        }
    }

    pub fn begin_page(&mut self, settings: Option<&DevMode>) -> Result<Page> {
        match self.state {
            State::Between => {}
            State::InPage => return Err(PrintError::Invalid("a page is already started".into())),
            State::Closed => return Err(closed()),
        }
        if let Some(settings) = settings {
            self.reset(settings).map_err(|e| self.abort(e))?;
        }
        // SAFETY: inside a started document on a valid DC, between pages.
        if unsafe { StartPage(self.dc.0) } <= 0 {
            let e = last_error("starting a page");
            return Err(self.abort(e));
        }
        self.state = State::InPage;
        // StartPage may reset the DC's state: the color policy is set for every page.
        // SAFETY: a valid printer DC and a documented SetICMMode value.
        if unsafe { SetICMMode(self.dc.0, self.color_mode) } == 0 {
            let e = last_error("setting GDI color policy");
            return Err(self.abort(e));
        }
        Ok(self.page)
    }

    /// New settings from the next page on, validated by the driver like the job's own.
    fn reset(&mut self, settings: &DevMode) -> Result<()> {
        let settings = if self.manual_copies {
            settings.for_manual_copies()?
        } else {
            settings.clone()
        };
        let merged = properties(&self.printer, Some(&settings), None, false)?.ok_or_else(|| {
            PrintError::Invalid(format!(
                "printer “{}” refused this page's settings",
                self.printer
            ))
        })?;
        let buf = aligned_copy(&merged);
        // SAFETY: a valid printer DC between pages; `buf` is aligned settings from this printer's
        // driver, alive for the call.
        let hdc = unsafe { ResetDCW(self.dc.0, buf.as_ptr().cast::<DEVMODEW>()) };
        if hdc.is_null() {
            return Err(last_error("changing the page setup"));
        }
        self.page = self.dc.page()?;
        Ok(())
    }

    fn stretch(&mut self, image: &RgbImage<'_>, dest: (i32, i32, i32, i32)) -> Result<()> {
        let (x, y, w, h) = dest;
        let (bmi, bits) = dib(image).map_err(|e| self.abort(e))?;
        let mode = if crate::is_device_size(image, (w, h)) {
            COLORONCOLOR
        } else {
            HALFTONE
        };
        // SAFETY: a valid page DC and a documented stretch mode; halftoning needs the brush origin.
        unsafe {
            SetStretchBltMode(self.dc.0, mode);
            SetBrushOrgEx(self.dc.0, 0, 0, null_mut());
        }
        // SAFETY: `bits` is a bottom-up DIB exactly as `bmi` describes; the source rectangle is
        // the whole image.
        let lines = unsafe {
            StretchDIBits(
                self.dc.0,
                x,
                y,
                w,
                h,
                0,
                0,
                image.width as i32,
                image.height as i32,
                bits.as_ptr().cast(),
                &bmi,
                DIB_RGB_COLORS,
                SRCCOPY,
            )
        };
        if lines <= 0 {
            let e = last_error("sending the image to the printer");
            return Err(self.abort(e));
        }
        Ok(())
    }

    pub fn draw_image(&mut self, image: &RgbImage<'_>, rect: (f64, f64, f64, f64)) -> Result<()> {
        self.in_page()?;
        let dest = crate::device_rect(&self.page, rect);
        self.stretch(image, dest)
    }

    pub fn draw_pixels(&mut self, image: &RgbImage<'_>, at: (i32, i32)) -> Result<()> {
        self.in_page()?;
        self.stretch(image, (at.0, at.1, image.width as i32, image.height as i32))
    }

    pub fn play_metafile(&mut self, emf: &[u8], rect: (f64, f64, f64, f64)) -> Result<()> {
        self.in_page()?;
        let (x, y, w, h) = crate::device_rect(&self.page, rect);
        let dest = RECT {
            left: x,
            top: y,
            right: x.saturating_add(w),
            bottom: y.saturating_add(h),
        };
        let Ok(len) = u32::try_from(emf.len()) else {
            return Err(PrintError::Invalid("the page picture is too large".into()));
        };
        // SAFETY: `emf` is `len` readable bytes holding a checked EMF header; the system copies them.
        let handle = unsafe { SetEnhMetaFileBits(len, emf.as_ptr()) };
        if handle.is_null() {
            let e = last_error("reading the page picture");
            return Err(self.abort(e));
        }
        // SAFETY: a valid page DC, the live metafile just made, and a rect alive for the call.
        let played = unsafe { PlayEnhMetaFile(self.dc.0, handle, &dest) } != 0;
        let error = (!played).then(|| last_error("drawing the page picture"));
        // SAFETY: the metafile is deleted once, after its only use.
        unsafe { DeleteEnhMetaFile(handle) };
        match error {
            Some(e) => Err(self.abort(e)),
            None => Ok(()),
        }
    }

    pub fn end_page(&mut self) -> Result<()> {
        self.in_page()?;
        // SAFETY: closes the page started on this DC.
        if unsafe { EndPage(self.dc.0) } <= 0 {
            let e = last_error("finishing a page");
            return Err(self.abort(e));
        }
        self.state = State::Between;
        Ok(())
    }

    pub fn finish(mut self) -> Result<()> {
        if self.state == State::InPage {
            self.end_page()?;
        }
        if self.state == State::Closed {
            return Err(closed());
        }
        self.state = State::Closed;
        // SAFETY: closes the document started on this DC.
        if unsafe { EndDoc(self.dc.0) } <= 0 {
            return Err(last_error("finishing the print job"));
        }
        Ok(())
    }
}

impl Drop for Doc {
    fn drop(&mut self) {
        if self.state != State::Closed {
            // SAFETY: the document was started on this DC and not yet ended.
            unsafe { AbortDoc(self.dc.0) };
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use crate::{ColorPolicy, Document, DocumentJob, PrintError, RgbImage};

    /// A streamed job through Windows' "Microsoft Print to PDF" driver, written to a file: a page
    /// of pixel bands, then a landscape page holding a metafile (needs that printer; run by hand,
    /// `WINPRINT_TEST_OUT` naming the output folder).
    #[test]
    #[ignore]
    fn streams_bands_and_a_metafile_through_a_real_driver() {
        let printer = "Microsoft Print to PDF";
        let dir =
            std::env::var("WINPRINT_TEST_OUT").map_or_else(|_| std::env::temp_dir(), Into::into);
        let out = dir.join(format!("pc-winprint-doc-{}.pdf", std::process::id()));
        let _ = std::fs::remove_file(&out);
        let out_s = out.to_string_lossy().into_owned();
        let settings = crate::default_settings(printer).unwrap();
        let job = DocumentJob {
            printer,
            settings: Some(&settings),
            title: "winprint document test",
            output_file: Some(&out_s),
            color_policy: ColorPolicy::Unmanaged,
            manual_copies: true,
        };
        let mut doc = Document::start(&job).unwrap();
        let page = doc.begin_page(None).unwrap();
        let (w, h) = (page.dpi.0, page.dpi.1 / 4);
        for band in 0..4u32 {
            let shade = 60 + 40 * band as u8;
            let data = vec![shade; (w * h * 3) as usize];
            let image = RgbImage {
                width: w,
                height: h,
                data: &data,
            };
            doc.draw_pixels(&image, (0, (band * h) as i32)).unwrap();
        }
        assert!(
            matches!(doc.begin_page(None), Err(PrintError::Invalid(_))),
            "one page at a time"
        );
        doc.end_page().unwrap();
        let mut landscape = settings.as_bytes().to_vec();
        landscape[76..78].copy_from_slice(&2i16.to_le_bytes());
        let fields = u32::from_le_bytes(landscape[72..76].try_into().unwrap()) | 1;
        landscape[72..76].copy_from_slice(&fields.to_le_bytes());
        let landscape = crate::DevMode::from_bytes(landscape).unwrap();
        let wide = doc.begin_page(Some(&landscape)).unwrap();
        assert!(
            wide.paper.0 > wide.paper.1,
            "the second page turned: {:?}",
            wide.paper
        );
        let emf = crate::document::tests::emf_with_a_rectangle();
        doc.play_metafile(&emf, (72.0, 72.0, 288.0, 144.0)).unwrap();
        doc.finish().unwrap();
        let pdf = std::fs::read(&out).unwrap();
        assert!(
            pdf.starts_with(b"%PDF") && pdf.len() > 1000,
            "the driver wrote a PDF"
        );
        let pages = pdf
            .windows(10)
            .filter(|w| w == b"/Type/Page" || w == b"/Type /Pag")
            .count();
        assert!(pages >= 2, "two pages, found {pages}");
        if std::env::var("WINPRINT_TEST_OUT").is_err() {
            let _ = std::fs::remove_file(&out);
        }
    }

    /// Dropping a job before it finishes cancels it, and calls on a page that was never begun fail.
    #[test]
    #[ignore]
    fn an_unfinished_job_is_cancelled() {
        let printer = "Microsoft Print to PDF";
        let dir =
            std::env::var("WINPRINT_TEST_OUT").map_or_else(|_| std::env::temp_dir(), Into::into);
        let out = dir.join(format!("pc-winprint-abort-{}.pdf", std::process::id()));
        let _ = std::fs::remove_file(&out);
        let out_s = out.to_string_lossy().into_owned();
        let job = DocumentJob {
            printer,
            settings: None,
            title: "winprint abort test",
            output_file: Some(&out_s),
            color_policy: ColorPolicy::Unmanaged,
            manual_copies: false,
        };
        let mut doc = Document::start(&job).unwrap();
        let data = [0u8; 3];
        let pixel = RgbImage {
            width: 1,
            height: 1,
            data: &data,
        };
        assert!(matches!(
            doc.draw_pixels(&pixel, (0, 0)),
            Err(PrintError::Invalid(_))
        ));
        assert!(matches!(doc.end_page(), Err(PrintError::Invalid(_))));
        drop(doc);
        assert!(
            std::fs::metadata(&out).map_or(true, |m| m.len() == 0),
            "no document was written"
        );
    }
}
