mod layout;

use crate::printer::FilePrinter;
use crate::printer::PrinterDevice;
use crate::ticket::PrintTicket;
use crate::ticket::ToDevModeError;
use crate::utils::com::ComInitializer;
use crate::utils::wchar;
use layout::{DeviceCaps, Placement};
use std::collections::TryReserveError;
use std::mem;
use std::path::Path;
use thiserror::Error;
use windows::core::{w, HSTRING, PCWSTR};
use windows::Data::Pdf::{PdfDocument, PdfPage, PdfPageRenderOptions};
use windows::Storage::StorageFile;
use windows::Storage::Streams::InMemoryRandomAccessStream;
use windows::Win32::Graphics::Gdi::{
    CreateDCW, DeleteDC, GetDeviceCaps, SetBrushOrgEx, SetStretchBltMode, StretchDIBits,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DEVMODEW, DIB_RGB_COLORS, GDI_ERROR, HALFTONE, HDC,
    LOGPIXELSX, LOGPIXELSY, PHYSICALHEIGHT, PHYSICALOFFSETX, PHYSICALOFFSETY, PHYSICALWIDTH,
    SRCCOPY,
};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat24bppBGR, IWICImagingFactory,
    WICBitmapDitherTypeNone, WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::Storage::Xps::{AbortDoc, EndDoc, EndPage, StartDocW, StartPage, DOCINFOW};
use windows::Win32::System::Com::{CoCreateInstance, IStream, CLSCTX_INPROC_SERVER};
use windows::Win32::System::WinRT::CreateStreamOverRandomAccessStream;

#[derive(Error, Debug)]
/// Represents an error from [`GdiPdfPrinter`].
pub enum GdiPdfPrinterError {
    /// Invalid path.
    #[error("Invalid path")]
    InvalidPath(#[source] std::io::Error),
    /// Failed to open the document.
    #[error("Failed to open the document")]
    FailedToOpenDocument(#[source] windows::core::Error),
    /// The print ticket could not be turned into a DEVMODE.
    #[error("Print ticket error")]
    PrintTicketError(#[source] ToDevModeError),
    /// Failed to open printer.
    #[error("Failed to open printer")]
    FailedToOpenPrinter(#[source] windows::core::Error),
    /// StartDoc failed.
    #[error("StartDocW failed (returned {0})")]
    StartDocFailed(i32, #[source] windows::core::Error),
    /// The page or the printer reports a size that cannot be printed.
    #[error("Unusable page or printer size for page {0}")]
    UnusableSize(u32),
    /// The page's bitmap could not be allocated.
    #[error("Failed to allocate the bitmap for page {0}")]
    AllocationFailed(u32, #[source] TryReserveError),
    /// Render error.
    #[error("Render error")]
    RenderError(#[source] windows::core::Error),
    /// StartPage failed.
    #[error("StartPage failed for page {0} (returned {1})")]
    StartPageFailed(u32, i32, #[source] windows::core::Error),
    /// StretchDIBits failed.
    #[error("StretchDIBits failed for page {0} (returned {1})")]
    DrawFailed(u32, i32, #[source] windows::core::Error),
    /// EndPage failed.
    #[error("EndPage failed for page {0} (returned {1})")]
    EndPageFailed(u32, i32, #[source] windows::core::Error),
    /// EndDoc failed.
    #[error("EndDoc failed (returned {0})")]
    EndDocFailed(i32, #[source] windows::core::Error),
}

/// A printer that renders each page of a PDF with
/// [`Windows.Data.Pdf`](https://learn.microsoft.com/en-us/uwp/api/windows.data.pdf) to a bitmap
/// and draws it on a GDI printer DC, for drivers that refuse the Direct2D/XPS path of
/// [`WinPdfPrinter`](super::WinPdfPrinter).
///
/// Each page is a bitmap at the device's resolution, at most 300 dpi per axis and 12 megapixels,
/// stretched over the whole physical sheet with its proportions kept (a page with the sheet's
/// size fills it), in horizontal bands of at most 4 MiB. One document holds every page, so
/// copies, collation and two-sided printing from the ticket's DEVMODE apply to the job as a
/// whole. A failed GDI call reports the thread's last error as its source.
pub struct GdiPdfPrinter {
    printer: PrinterDevice,
}

impl GdiPdfPrinter {
    /// Create a new [`GdiPdfPrinter`] for the given printer device.
    pub fn new(printer: PrinterDevice) -> Self {
        Self { printer }
    }
}

impl FilePrinter for GdiPdfPrinter {
    type Options = PrintTicket;
    type Error = GdiPdfPrinterError;
    fn print(&self, path: &Path, options: PrintTicket) -> Result<(), GdiPdfPrinterError> {
        self.print_with_progress(path, options, |_, _| {})
    }
}

/// A printer DC, deleted on drop; a document started on it that did not end is aborted first,
/// so a failed job never stays half-spooled.
struct PrinterDc {
    hdc: HDC,
    document_open: bool,
}

impl Drop for PrinterDc {
    fn drop(&mut self) {
        unsafe {
            if self.document_open {
                let _ = AbortDoc(self.hdc);
            }
            let _ = DeleteDC(self.hdc);
        }
    }
}

impl GdiPdfPrinter {
    /// Print one PDF as one GDI document, reporting each page after `EndPage` accepts it.
    ///
    /// Reports `(0, total)` once the document has started, then `(processed, total)` after each
    /// page. Counts describe pages processed, not physical output. The callback runs
    /// synchronously on the calling thread; it does not require `Send`.
    pub fn print_with_progress(
        &self,
        path: &Path,
        options: PrintTicket,
        mut progress: impl FnMut(u32, u32),
    ) -> Result<(), GdiPdfPrinterError> {
        // Declared first so that it is dropped last, after every COM and WinRT object.
        let _com_initializer = ComInitializer::new();
        unsafe {
            let absolute_path =
                std::path::absolute(path).map_err(GdiPdfPrinterError::InvalidPath)?;
            let file = StorageFile::GetFileFromPathAsync(&HSTRING::from(absolute_path.as_path()))
                .map_err(GdiPdfPrinterError::FailedToOpenDocument)?
                .join()
                .map_err(GdiPdfPrinterError::FailedToOpenDocument)?;
            let pdf_document = PdfDocument::LoadFromFileAsync(&file)
                .map_err(GdiPdfPrinterError::FailedToOpenDocument)?
                .join()
                .map_err(GdiPdfPrinterError::FailedToOpenDocument)?;
            let page_count = pdf_document
                .PageCount()
                .map_err(GdiPdfPrinterError::RenderError)?;
            let wic_factory: IWICImagingFactory =
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
                    .map_err(GdiPdfPrinterError::RenderError)?;

            // The same conversion the Direct2D/XPS path uses. A ticket the driver cannot turn
            // into a DEVMODE is an error, never the queue's defaults: those could print other
            // paper, copies or sides than were asked for.
            let dev_mode = options
                .to_dev_mode(&self.printer)
                .map_err(GdiPdfPrinterError::PrintTicketError)?;
            // "WINSPOOL" as the driver, as PdfiumPrinter and Chromium do.
            let hdc = CreateDCW(
                w!("WINSPOOL"),
                PCWSTR(wchar::to_wide_chars(self.printer.os_name()).as_ptr()),
                None,
                Some(dev_mode.as_ptr() as *const DEVMODEW),
            );
            if hdc.is_invalid() {
                return Err(GdiPdfPrinterError::FailedToOpenPrinter(
                    windows::core::Error::from_thread(),
                ));
            }
            let mut dc = PrinterDc {
                hdc,
                document_open: false,
            };
            SetStretchBltMode(hdc, HALFTONE);
            // After setting HALFTONE, the brush origin must be set or brushes misalign.
            let _ = SetBrushOrgEx(hdc, 0, 0, None);
            let caps = DeviceCaps {
                dpi: (
                    GetDeviceCaps(Some(hdc), LOGPIXELSX),
                    GetDeviceCaps(Some(hdc), LOGPIXELSY),
                ),
                physical: (
                    GetDeviceCaps(Some(hdc), PHYSICALWIDTH),
                    GetDeviceCaps(Some(hdc), PHYSICALHEIGHT),
                ),
                offset: (
                    GetDeviceCaps(Some(hdc), PHYSICALOFFSETX),
                    GetDeviceCaps(Some(hdc), PHYSICALOFFSETY),
                ),
            };

            let mut doc_name = wchar::to_wide_chars(path.file_name().unwrap_or(path.as_ref()));
            let doc_info = DOCINFOW {
                cbSize: mem::size_of::<DOCINFOW>() as i32,
                lpszDocName: PCWSTR(doc_name.as_mut_ptr()),
                ..Default::default()
            };
            let start_doc_ret = StartDocW(hdc, &doc_info);
            if start_doc_ret <= 0 {
                return Err(GdiPdfPrinterError::StartDocFailed(
                    start_doc_ret,
                    windows::core::Error::from_thread(),
                ));
            }
            dc.document_open = true;

            progress(0, page_count);
            for i in 0..page_count {
                let pdf_page = pdf_document
                    .GetPage(i)
                    .map_err(GdiPdfPrinterError::RenderError)?;
                let size = pdf_page.Size().map_err(GdiPdfPrinterError::RenderError)?;
                let placement =
                    layout::place((f64::from(size.Width), f64::from(size.Height)), caps)
                        .ok_or(GdiPdfPrinterError::UnusableSize(i))?;
                // Rendered before StartPage, so a page that cannot be rendered never starts one.
                let bitmap = render_page(&wic_factory, &pdf_page, placement, i)?;
                print_bitmap(hdc, &bitmap, placement, i)?;
                progress(i + 1, page_count);
            }

            let end_doc_ret = EndDoc(hdc);
            if end_doc_ret <= 0 {
                return Err(GdiPdfPrinterError::EndDocFailed(
                    end_doc_ret,
                    windows::core::Error::from_thread(),
                ));
            }
            dc.document_open = false;
        }
        Ok(())
    }
}

/// A top-down 24-bit bitmap.
struct Bitmap {
    width: u32,
    height: u32,
    /// Bytes per row, padded to four.
    stride: u32,
    pixels: Vec<u8>,
}

/// Render a page at `placement`'s raster size: Windows.Data.Pdf encodes it (PNG, on its default
/// white background) into a memory stream, and WIC decodes that into 24-bit BGR rows. This keeps
/// to the in-box PDF renderer the Direct2D/XPS path uses, and needs no Direct3D device or GPU
/// texture limits; the encoded stream of a mostly white page is small.
fn render_page(
    wic_factory: &IWICImagingFactory,
    page: &PdfPage,
    placement: Placement,
    index: u32,
) -> Result<Bitmap, GdiPdfPrinterError> {
    let render_error = GdiPdfPrinterError::RenderError;
    let stream = InMemoryRandomAccessStream::new().map_err(render_error)?;
    let render_options = PdfPageRenderOptions::new().map_err(render_error)?;
    render_options
        .SetDestinationWidth(placement.raster.0)
        .map_err(render_error)?;
    render_options
        .SetDestinationHeight(placement.raster.1)
        .map_err(render_error)?;
    render_options
        .SetIsIgnoringHighContrast(true)
        .map_err(render_error)?;
    page.RenderWithOptionsToStreamAsync(&stream, &render_options)
        .map_err(render_error)?
        .join()
        .map_err(render_error)?;
    stream.Seek(0).map_err(render_error)?;
    // SAFETY: plain COM calls on live interfaces; `CopyPixels` gets a buffer of exactly
    // `stride` × `height` bytes, the size `dib_24bpp` computed for the converter's own size.
    unsafe {
        let com_stream: IStream =
            CreateStreamOverRandomAccessStream(&stream).map_err(render_error)?;

        let decoder = wic_factory
            .CreateDecoderFromStream(
                &com_stream,
                std::ptr::null(),
                WICDecodeMetadataCacheOnDemand,
            )
            .map_err(render_error)?;
        let frame = decoder.GetFrame(0).map_err(render_error)?;
        let converter = wic_factory.CreateFormatConverter().map_err(render_error)?;
        converter
            .Initialize(
                &frame,
                &GUID_WICPixelFormat24bppBGR,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeCustom,
            )
            .map_err(render_error)?;
        let (mut width, mut height) = (0, 0);
        converter
            .GetSize(&mut width, &mut height)
            .map_err(render_error)?;
        // The decoded size is what the renderer produced; it is checked against the cap again.
        let (stride, length) =
            layout::dib_24bpp(width, height).ok_or(GdiPdfPrinterError::UnusableSize(index))?;
        let mut pixels = Vec::new();
        pixels
            .try_reserve_exact(length)
            .map_err(|e| GdiPdfPrinterError::AllocationFailed(index, e))?;
        pixels.resize(length, 0);
        converter
            .CopyPixels(std::ptr::null(), stride, &mut pixels)
            .map_err(render_error)?;
        Ok(Bitmap {
            width,
            height,
            stride,
            pixels,
        })
    }
}

/// Draw one page's bitmap over its placement as one GDI page, in bands of whole rows so that no
/// `StretchDIBits` call carries more than [`layout::MAX_BAND_BYTES`] of it. A band that fails
/// returns at once, without `EndPage`: the caller's [`PrinterDc`] aborts the document, so no
/// partly drawn page is printed.
fn print_bitmap(
    hdc: HDC,
    bitmap: &Bitmap,
    placement: Placement,
    index: u32,
) -> Result<(), GdiPdfPrinterError> {
    // `dib_24bpp` capped the bitmap at MAX_PIXELS, so its width fits in an i32.
    let Ok(width) = i32::try_from(bitmap.width) else {
        return Err(GdiPdfPrinterError::UnusableSize(index));
    };
    let bands = layout::bands(
        bitmap.height,
        bitmap.stride,
        placement.top,
        placement.height,
    )
    .ok_or(GdiPdfPrinterError::UnusableSize(index))?;
    // SAFETY: `hdc` is the open printer DC.
    let start_page_ret = unsafe { StartPage(hdc) };
    if start_page_ret <= 0 {
        return Err(GdiPdfPrinterError::StartPageFailed(
            index,
            start_page_ret,
            windows::core::Error::from_thread(),
        ));
    }
    for band in bands {
        // The band's rows, given to GDI as a top-down DIB of their own.
        let offset = |row: u32| {
            usize::try_from(row)
                .ok()?
                .checked_mul(bitmap.stride as usize)
        };
        let bits = offset(band.row)
            .zip(offset(band.row + band.rows))
            .and_then(|(start, end)| bitmap.pixels.get(start..end));
        let (Some(bits), Ok(rows)) = (bits, i32::try_from(band.rows)) else {
            return Err(GdiPdfPrinterError::UnusableSize(index));
        };
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                // Negative: rows run top-down, as WIC wrote them.
                biHeight: -rows,
                biPlanes: 1,
                biBitCount: 24,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        // SAFETY: `bits` holds exactly `rows` rows of `bitmap.stride` bytes, the four-byte-aligned
        // 24-bit rows `info` describes, and outlives the call.
        let lines = unsafe {
            StretchDIBits(
                hdc,
                placement.left,
                band.top,
                placement.width,
                band.height,
                0,
                0,
                width,
                rows,
                Some(bits.as_ptr().cast()),
                &info,
                DIB_RGB_COLORS,
                SRCCOPY,
            )
        };
        if lines == 0 || lines == GDI_ERROR {
            return Err(GdiPdfPrinterError::DrawFailed(
                index,
                lines,
                windows::core::Error::from_thread(),
            ));
        }
    }
    // SAFETY: `hdc` is the open printer DC, with the page started above.
    let end_page_ret = unsafe { EndPage(hdc) };
    if end_page_ret <= 0 {
        return Err(GdiPdfPrinterError::EndPageFailed(
            index,
            end_page_ret,
            windows::core::Error::from_thread(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::GdiPdfPrinter;
    use crate::{printer::FilePrinter, test_utils::null_device};
    use std::path::Path;

    // The null device's "Generic / Text Only" driver is not documented to accept raster output,
    // so these run by hand on a raster printer; tests/visual_regression_gdi_pdf.rs covers CI.
    #[test]
    #[ignore = "needs a raster-capable printer driver"]
    fn print_simple_pdf_document() {
        let device = null_device::thread_local();
        let pdf = GdiPdfPrinter::new(device);
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("test_data/test_document.pdf");
        pdf.print(path.as_path(), Default::default()).unwrap();
    }

    #[test]
    #[ignore = "needs a raster-capable printer driver"]
    fn progress_counts_every_page_after_the_start() {
        let device = null_device::thread_local();
        let pdf = GdiPdfPrinter::new(device);
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("test_data/test_document.pdf");
        let mut seen = Vec::new();
        pdf.print_with_progress(path.as_path(), Default::default(), |done, total| {
            seen.push((done, total))
        })
        .unwrap();
        let total = seen.first().unwrap().1;
        assert!(total > 0);
        assert_eq!(
            seen,
            (0..=total).map(|done| (done, total)).collect::<Vec<_>>()
        );
    }
}
