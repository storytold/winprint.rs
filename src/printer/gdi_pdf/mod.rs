//! Direct GDI raster PDF printer using Direct2D + Windows.Data.Pdf with pipelined page
//! preparation and 2D tight bounding-box white trimming.

mod layout;

use crate::printer::win_pdf::{load_document, WinPdfPrinterError};
use crate::printer::{FilePrinter, PrinterDevice};
use crate::ticket::{PrintTicket, ToDevModeError};
use crate::utils::com::ComInitializer;
use crate::utils::wchar;
use layout::{DeviceCaps, PreparedBand};
use std::collections::TryReserveError;
use std::mem;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::mpsc;
use thiserror::Error;
use windows::core::{w, Interface, PCWSTR};
use windows::Data::Pdf::PdfDocument;
use windows::Win32::Foundation::E_UNEXPECTED;
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Bitmap1, ID2D1Device, ID2D1DeviceContext, ID2D1Factory1,
    D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_CPU_READ, D2D1_BITMAP_OPTIONS_TARGET,
    D2D1_BITMAP_PROPERTIES1, D2D1_DEVICE_CONTEXT_OPTIONS_NONE, D2D1_FACTORY_OPTIONS,
    D2D1_FACTORY_TYPE_MULTI_THREADED, D2D1_MAP_OPTIONS_READ, D2D1_UNIT_MODE_PIXELS,
};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{
    CreateDCW, DeleteDC, GetDeviceCaps, SetStretchBltMode, StretchDIBits, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, COLORONCOLOR, DEVMODEW, DIB_RGB_COLORS, GDI_ERROR, HDC, LOGPIXELSX,
    LOGPIXELSY, PHYSICALHEIGHT, PHYSICALOFFSETX, PHYSICALOFFSETY, PHYSICALWIDTH, SRCCOPY,
};
use windows::Win32::Storage::Xps::{AbortDoc, EndDoc, EndPage, StartDocW, StartPage, DOCINFOW};
use windows::Win32::System::WinRT::Pdf::{PdfCreateRenderer, PDF_RENDER_PARAMS};
use windows::Win32::System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED};

#[derive(Error, Debug)]
/// Represents an error from [`GdiPdfPrinter`].
pub enum GdiPdfPrinterError {
    /// Invalid file path.
    #[error("Invalid path")]
    InvalidPath(#[source] std::io::Error),
    /// Failed to open the PDF document.
    #[error("Failed to open the document")]
    FailedToOpenDocument(#[source] windows::core::Error),
    /// The print ticket could not be turned into a `DEVMODE`.
    #[error("Print ticket error")]
    PrintTicketError(#[source] ToDevModeError),
    /// The supplied `DEVMODE` buffer was invalid or shorter than a `DEVMODEW` header.
    #[error("Invalid DEVMODE buffer")]
    InvalidDevMode,
    /// Failed to open a printer GDI device context.
    #[error("Failed to open printer")]
    FailedToOpenPrinter(#[source] windows::core::Error),
    /// `StartDocW` failed.
    #[error("StartDocW failed (returned {0})")]
    StartDocFailed(i32, #[source] windows::core::Error),
    /// The page or printer reported an unusable size.
    #[error("Unusable page or printer size for page {0}")]
    UnusableSize(u32),
    /// Failed to allocate the band buffer for a page.
    #[error("Failed to allocate the bitmap for page {0}")]
    AllocationFailed(u32, #[source] TryReserveError),
    /// Failed while rendering PDF content via Windows.Data.Pdf / Direct2D.
    #[error("Render error")]
    RenderError(#[source] windows::core::Error),
    /// `StartPage` failed.
    #[error("StartPage failed for page {0} (returned {1})")]
    StartPageFailed(u32, i32, #[source] windows::core::Error),
    /// `StretchDIBits` failed.
    #[error("StretchDIBits failed for page {0} (returned {1})")]
    DrawFailed(u32, i32, #[source] windows::core::Error),
    /// `EndPage` failed.
    #[error("EndPage failed for page {0} (returned {1})")]
    EndPageFailed(u32, i32, #[source] windows::core::Error),
    /// `EndDoc` failed.
    #[error("EndDoc failed (returned {0})")]
    EndDocFailed(i32, #[source] windows::core::Error),
}

/// A printer that renders each PDF page with `Windows.Data.Pdf` and Direct2D on a worker thread
/// and sends 2D-trimmed 24-bit BGR bands to a GDI printer device context with `StretchDIBits`.
///
/// Designed for v3 GDI drivers (where Windows' XPS-to-GDI converter otherwise rasterizes
/// transparent/image-heavy sheets at full resolution) and drivers whose XPS pipeline rejects a
/// PrintTicket (`0x80040003` / `E_PRINTTICKET_FORMAT`).
pub struct GdiPdfPrinter {
    printer: PrinterDevice,
    output_file: Option<PathBuf>,
}

impl GdiPdfPrinter {
    /// Create a new [`GdiPdfPrinter`] for the given printer device.
    pub fn new(printer: PrinterDevice) -> Self {
        Self {
            printer,
            output_file: None,
        }
    }

    /// Redirect the spooled GDI output to `path` via `DOCINFOW.lpszOutput` instead of sending
    /// output to the physical printer port (useful for automated tests and profiling).
    pub fn with_output_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.output_file = Some(path.into());
        self
    }
}

impl FilePrinter for GdiPdfPrinter {
    type Options = PrintTicket;
    type Error = GdiPdfPrinterError;

    fn print(&self, path: &Path, options: PrintTicket) -> Result<(), GdiPdfPrinterError> {
        self.print_with_progress(path, options, |_, _| {})
    }
}

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

struct PreparedPage {
    index: u32,
    bands: Vec<PreparedBand>,
}

enum WorkerMsg {
    PageCount(u32),
    Page(PreparedPage),
}

fn aligned_dev_mode(bytes: &[u8]) -> Option<Vec<u64>> {
    if bytes.len() < mem::size_of::<DEVMODEW>() {
        return None;
    }
    let mut aligned = vec![0u64; bytes.len().div_ceil(8)];
    // SAFETY: `aligned` holds at least `bytes.len()` writable bytes and neither overlaps.
    unsafe {
        ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            aligned.as_mut_ptr().cast::<u8>(),
            bytes.len(),
        );
    }
    Some(aligned)
}

fn create_d3d_device() -> Result<ID3D11Device, GdiPdfPrinterError> {
    for driver_type in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
        let mut d3d_device: Option<ID3D11Device> = None;
        let hr = unsafe {
            D3D11CreateDevice(
                None,
                driver_type,
                Default::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut d3d_device),
                None,
                None,
            )
        };
        match (hr, d3d_device) {
            (Ok(()), Some(dev)) => return Ok(dev),
            (res, _) if driver_type == D3D_DRIVER_TYPE_WARP => {
                return Err(GdiPdfPrinterError::RenderError(res.err().unwrap_or_else(
                    || windows::core::Error::from_hresult(E_UNEXPECTED),
                )));
            }
            _ => {}
        }
    }
    Err(GdiPdfPrinterError::RenderError(
        windows::core::Error::from_hresult(E_UNEXPECTED),
    ))
}

fn map_open_err(err: WinPdfPrinterError) -> GdiPdfPrinterError {
    match err {
        WinPdfPrinterError::InvalidPath(e) => GdiPdfPrinterError::InvalidPath(e),
        WinPdfPrinterError::FailedToOpenDocument(e) => GdiPdfPrinterError::FailedToOpenDocument(e),
        other => GdiPdfPrinterError::RenderError(windows::core::Error::new(
            E_UNEXPECTED,
            other.to_string(),
        )),
    }
}

fn render_worker(
    pdf_path: &Path,
    caps: DeviceCaps,
    tx: &mpsc::SyncSender<Result<WorkerMsg, GdiPdfPrinterError>>,
) {
    let _com = ComInitializer::new();
    let ro_ok = unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.is_ok();
    scopeguard::defer! {
        if ro_ok {
            unsafe {
                RoUninitialize();
            }
        }
    }

    let pdf_doc: PdfDocument = match load_document(pdf_path).map_err(map_open_err) {
        Ok(doc) => doc,
        Err(e) => {
            let _ = tx.send(Err(e));
            return;
        }
    };
    let page_count = match pdf_doc.PageCount().map_err(GdiPdfPrinterError::RenderError) {
        Ok(n) => n,
        Err(e) => {
            let _ = tx.send(Err(e));
            return;
        }
    };

    let d3d_device = match create_d3d_device() {
        Ok(d) => d,
        Err(e) => {
            let _ = tx.send(Err(e));
            return;
        }
    };
    let dxgi_device: IDXGIDevice = match d3d_device
        .cast::<IDXGIDevice>()
        .map_err(GdiPdfPrinterError::RenderError)
    {
        Ok(d) => d,
        Err(e) => {
            let _ = tx.send(Err(e));
            return;
        }
    };
    let d2d_factory: ID2D1Factory1 = match unsafe {
        D2D1CreateFactory(
            D2D1_FACTORY_TYPE_MULTI_THREADED,
            Some(&D2D1_FACTORY_OPTIONS::default()),
        )
    }
    .map_err(GdiPdfPrinterError::RenderError)
    {
        Ok(f) => f,
        Err(e) => {
            let _ = tx.send(Err(e));
            return;
        }
    };
    let d2d_device: ID2D1Device = match unsafe { d2d_factory.CreateDevice(&dxgi_device) }
        .map_err(GdiPdfPrinterError::RenderError)
    {
        Ok(d) => d,
        Err(e) => {
            let _ = tx.send(Err(e));
            return;
        }
    };
    let d2d_context: ID2D1DeviceContext =
        match unsafe { d2d_device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE) }
            .map_err(GdiPdfPrinterError::RenderError)
        {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(Err(e));
                return;
            }
        };
    unsafe { d2d_context.SetUnitMode(D2D1_UNIT_MODE_PIXELS) };

    let pdf_renderer =
        match unsafe { PdfCreateRenderer(&dxgi_device) }.map_err(GdiPdfPrinterError::RenderError) {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.send(Err(e));
                return;
            }
        };

    if tx.send(Ok(WorkerMsg::PageCount(page_count))).is_err() {
        return;
    }

    let mut cached_size: Option<(u32, u32)> = None;
    let mut target_bmp: Option<ID2D1Bitmap1> = None;
    let mut staging_bmp: Option<ID2D1Bitmap1> = None;

    for i in 0..page_count {
        let page_res = (|| -> Result<PreparedPage, GdiPdfPrinterError> {
            let pdf_page = pdf_doc
                .GetPage(i)
                .map_err(GdiPdfPrinterError::RenderError)?;
            let size = pdf_page.Size().map_err(GdiPdfPrinterError::RenderError)?;
            let placement = layout::place((f64::from(size.Width), f64::from(size.Height)), caps)
                .ok_or(GdiPdfPrinterError::UnusableSize(i))?;
            let (w_px, h_px) = placement.raster;

            if cached_size != Some((w_px, h_px)) {
                let pf = D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                };
                let t_props = D2D1_BITMAP_PROPERTIES1 {
                    pixelFormat: pf,
                    dpiX: 96.0,
                    dpiY: 96.0,
                    bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET,
                    ..Default::default()
                };
                let s_props = D2D1_BITMAP_PROPERTIES1 {
                    pixelFormat: pf,
                    dpiX: 96.0,
                    dpiY: 96.0,
                    bitmapOptions: D2D1_BITMAP_OPTIONS_CPU_READ | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                    ..Default::default()
                };
                let sz = D2D_SIZE_U {
                    width: w_px,
                    height: h_px,
                };
                target_bmp = Some(
                    unsafe { d2d_context.CreateBitmap(sz, None, 0, &t_props) }
                        .map_err(GdiPdfPrinterError::RenderError)?,
                );
                staging_bmp = Some(
                    unsafe { d2d_context.CreateBitmap(sz, None, 0, &s_props) }
                        .map_err(GdiPdfPrinterError::RenderError)?,
                );
                cached_size = Some((w_px, h_px));
            }

            let t_bmp = target_bmp.as_ref().expect("target_bmp initialized above");
            let s_bmp = staging_bmp.as_ref().expect("staging_bmp initialized above");
            let render_params = PDF_RENDER_PARAMS {
                DestinationWidth: w_px,
                DestinationHeight: h_px,
                IgnoreHighContrast: true,
                ..Default::default()
            };
            let white = D2D1_COLOR_F {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 1.0,
            };
            unsafe {
                d2d_context.SetTarget(t_bmp);
                d2d_context.BeginDraw();
                d2d_context.Clear(Some(&white));
                let draw_res = pdf_renderer.RenderPageToDeviceContext(
                    &pdf_page,
                    &d2d_context,
                    Some(&render_params),
                );
                let end_res = d2d_context.EndDraw(None, None);
                d2d_context.SetTarget(None);
                draw_res.map_err(GdiPdfPrinterError::RenderError)?;
                end_res.map_err(GdiPdfPrinterError::RenderError)?;
                s_bmp
                    .CopyFromBitmap(None, t_bmp, None)
                    .map_err(GdiPdfPrinterError::RenderError)?;
            }

            let mapped = unsafe { s_bmp.Map(D2D1_MAP_OPTIONS_READ) }
                .map_err(GdiPdfPrinterError::RenderError)?;
            scopeguard::defer! {
                unsafe {
                    let _ = s_bmp.Unmap();
                }
            }
            let pitch = mapped.pitch as usize;
            let h = h_px as usize;
            let w = w_px as usize;
            let len = h
                .saturating_sub(1)
                .checked_mul(pitch)
                .and_then(|v| v.checked_add(w * 4))
                .ok_or(GdiPdfPrinterError::UnusableSize(i))?;
            // SAFETY: `ID2D1Bitmap1::Map` guarantees readable rows of `pitch` bytes for `h` rows.
            let bgra = unsafe { std::slice::from_raw_parts(mapped.bits, len) };
            let bands = layout::pack_trimmed_bands(bgra, pitch, w, h, placement)
                .map_err(|e| GdiPdfPrinterError::AllocationFailed(i, e))?;
            Ok(PreparedPage { index: i, bands })
        })();

        let is_err = page_res.is_err();
        if tx.send(page_res.map(WorkerMsg::Page)).is_err() || is_err {
            break;
        }
    }
}

impl GdiPdfPrinter {
    /// Print one PDF as one GDI document, reporting `(0, total)` after `StartDocW` and
    /// `(processed, total)` after each `EndPage`.
    pub fn print_with_progress(
        &self,
        path: &Path,
        options: PrintTicket,
        progress: impl FnMut(u32, u32),
    ) -> Result<(), GdiPdfPrinterError> {
        let dev_mode = options
            .to_dev_mode(&self.printer)
            .map_err(GdiPdfPrinterError::PrintTicketError)?;
        self.print_with_dev_mode_and_progress(path, &dev_mode, progress)
    }

    /// Print one PDF via GDI using a pre-converted `DEVMODEW` byte buffer to avoid a duplicate
    /// PrintTicket-to-DEVMODE conversion when the caller already computed one.
    pub fn print_with_dev_mode_and_progress(
        &self,
        path: &Path,
        dev_mode: &[u8],
        mut progress: impl FnMut(u32, u32),
    ) -> Result<(), GdiPdfPrinterError> {
        let aligned_dm = aligned_dev_mode(dev_mode).ok_or(GdiPdfPrinterError::InvalidDevMode)?;
        let printer_name = wchar::to_wide_chars(self.printer.os_name());

        let hdc = unsafe {
            CreateDCW(
                w!("WINSPOOL"),
                PCWSTR(printer_name.as_ptr()),
                None,
                Some(aligned_dm.as_ptr().cast::<DEVMODEW>()),
            )
        };
        if hdc.is_invalid() {
            return Err(GdiPdfPrinterError::FailedToOpenPrinter(
                windows::core::Error::from_thread(),
            ));
        }
        let mut dc = PrinterDc {
            hdc,
            document_open: false,
        };

        let caps = unsafe {
            DeviceCaps {
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
            }
        };

        // Spawn render worker before `StartDocW` so DX/PDF init and page 0 overlap spooler setup.
        let pdf_owned = path.to_path_buf();
        let (tx, rx) = mpsc::sync_channel::<Result<WorkerMsg, GdiPdfPrinterError>>(1);
        let worker = std::thread::spawn(move || {
            render_worker(&pdf_owned, caps, &tx);
        });

        let doc_name = wchar::to_wide_chars(path.file_name().unwrap_or(path.as_ref()));
        let output_wide = self.output_file.as_deref().map(wchar::to_wide_chars);
        let doc_info = DOCINFOW {
            cbSize: mem::size_of::<DOCINFOW>() as i32,
            lpszDocName: PCWSTR(doc_name.as_ptr()),
            lpszOutput: output_wide
                .as_ref()
                .map_or(PCWSTR::null(), |w| PCWSTR(w.as_ptr())),
            ..Default::default()
        };
        let start_doc_ret = unsafe { StartDocW(hdc, &doc_info) };
        if start_doc_ret <= 0 {
            let err = windows::core::Error::from_thread();
            drop(rx);
            let _ = worker.join();
            return Err(GdiPdfPrinterError::StartDocFailed(start_doc_ret, err));
        }
        dc.document_open = true;

        let total_pages = match rx.recv() {
            Ok(Ok(WorkerMsg::PageCount(n))) => n,
            Ok(Ok(WorkerMsg::Page(_))) => {
                drop(rx);
                let _ = worker.join();
                return Err(GdiPdfPrinterError::UnusableSize(0));
            }
            Ok(Err(e)) => {
                drop(rx);
                let _ = worker.join();
                return Err(e);
            }
            Err(_) => {
                let _ = worker.join();
                return Err(GdiPdfPrinterError::FailedToOpenDocument(
                    windows::core::Error::from_hresult(E_UNEXPECTED),
                ));
            }
        };
        progress(0, total_pages);

        loop {
            let page = match rx.recv() {
                Ok(Ok(WorkerMsg::Page(p))) => p,
                Ok(Ok(WorkerMsg::PageCount(_))) => {
                    drop(rx);
                    let _ = worker.join();
                    return Err(GdiPdfPrinterError::UnusableSize(0));
                }
                Ok(Err(e)) => {
                    drop(rx);
                    let _ = worker.join();
                    return Err(e);
                }
                Err(_) => break,
            };

            let start_page_ret = unsafe { StartPage(hdc) };
            if start_page_ret <= 0 {
                let err = windows::core::Error::from_thread();
                drop(rx);
                let _ = worker.join();
                return Err(GdiPdfPrinterError::StartPageFailed(
                    page.index,
                    start_page_ret,
                    err,
                ));
            }

            unsafe {
                SetStretchBltMode(hdc, COLORONCOLOR);
            }

            for band in &page.bands {
                let Ok(bw) = i32::try_from(band.width) else {
                    drop(rx);
                    let _ = worker.join();
                    return Err(GdiPdfPrinterError::UnusableSize(page.index));
                };
                let Ok(bh) = i32::try_from(band.height) else {
                    drop(rx);
                    let _ = worker.join();
                    return Err(GdiPdfPrinterError::UnusableSize(page.index));
                };
                let Ok(sz) = u32::try_from(band.pixels.len()) else {
                    drop(rx);
                    let _ = worker.join();
                    return Err(GdiPdfPrinterError::UnusableSize(page.index));
                };
                let bmi = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: mem::size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: bw,
                        biHeight: bh,
                        biPlanes: 1,
                        biBitCount: 24,
                        biCompression: BI_RGB.0,
                        biSizeImage: sz,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let lines = unsafe {
                    StretchDIBits(
                        hdc,
                        band.dst_left,
                        band.dst_top,
                        band.dst_width,
                        band.dst_height,
                        0,
                        0,
                        bw,
                        bh,
                        Some(band.pixels.as_ptr().cast()),
                        &bmi,
                        DIB_RGB_COLORS,
                        SRCCOPY,
                    )
                };
                if lines == 0 || lines == GDI_ERROR {
                    let err = windows::core::Error::from_thread();
                    drop(rx);
                    let _ = worker.join();
                    return Err(GdiPdfPrinterError::DrawFailed(page.index, lines, err));
                }
            }

            let end_page_ret = unsafe { EndPage(hdc) };
            if end_page_ret <= 0 {
                let err = windows::core::Error::from_thread();
                drop(rx);
                let _ = worker.join();
                return Err(GdiPdfPrinterError::EndPageFailed(
                    page.index,
                    end_page_ret,
                    err,
                ));
            }
            progress(page.index + 1, total_pages);
        }

        let end_doc_ret = unsafe { EndDoc(hdc) };
        if end_doc_ret <= 0 {
            let err = windows::core::Error::from_thread();
            let _ = worker.join();
            return Err(GdiPdfPrinterError::EndDocFailed(end_doc_ret, err));
        }
        dc.document_open = false;
        let _ = worker.join();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{aligned_dev_mode, GdiPdfPrinter, GdiPdfPrinterError};
    use crate::{
        printer::{FilePrinter, PrinterDevice},
        test_utils::null_device,
    };
    use std::mem;
    use std::path::Path;
    use windows::Win32::Graphics::Gdi::DEVMODEW;

    #[test]
    fn aligned_dev_mode_rejects_short_buffer_and_accepts_full_header() {
        let too_short = vec![0u8; mem::size_of::<DEVMODEW>() - 1];
        assert!(aligned_dev_mode(&too_short).is_none());

        let exact = vec![0xA5u8; mem::size_of::<DEVMODEW>()];
        let aligned = aligned_dev_mode(&exact).unwrap();
        assert_eq!(
            aligned.as_ptr().align_offset(mem::align_of::<DEVMODEW>()),
            0
        );
    }

    #[test]
    fn short_dev_mode_errors_before_opening_printer() {
        let device = PrinterDevice::for_test("winprint-unused-test-queue");
        let pdf = GdiPdfPrinter::new(device);
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("test_data/test_document.pdf");
        let err = pdf
            .print_with_dev_mode_and_progress(&path, &[0u8; 16], |_, _| {})
            .unwrap_err();
        assert!(matches!(err, GdiPdfPrinterError::InvalidDevMode));
    }

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
