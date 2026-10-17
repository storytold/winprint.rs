use super::DxgiPrintContext;
use super::DxgiPrintContextError;
use crate::printer::FilePrinter;
use crate::printer::PrinterDevice;
use crate::ticket::PrintTicket;
use std::path::Path;
use thiserror::Error;
use windows::core::HSTRING;
use windows::Data::Pdf::PdfDocument;
use windows::Storage::StorageFile;
use windows::Win32::Graphics::Direct2D::Common::D2D_SIZE_F;
use windows::Win32::System::WinRT::Pdf::PdfCreateRenderer;
use windows::Win32::System::WinRT::Pdf::PDF_RENDER_PARAMS;
use windows_numerics::Matrix3x2;

#[derive(Error, Debug)]
/// Represents an error from [`WinPdfPrinter`].
pub enum WinPdfPrinterError {
    /// DXGI print context error.
    #[error("DXGI print context error")]
    DxgiPrintContextError(#[from] DxgiPrintContextError),
    /// Invalid path.
    #[error("Invalid path")]
    InvalidPath(#[source] std::io::Error),
    /// Failed to open the document.
    #[error("Failed to open the document")]
    FailedToOpenDocument(#[source] windows::core::Error),
    /// The page does not fit the requested resolution.
    #[error("The page ({dips} DIPs) cannot be rendered at {scale} times its size")]
    PageSize {
        /// The side of the page in device-independent pixels.
        dips: f32,
        /// The factor of the requested resolution over 96 dpi.
        scale: f32,
    },
    /// Render error.
    #[error("Render error")]
    RenderError(#[source] windows::core::Error),
}

/// A printer that uses [`Windows.Data.Pdf`](https://learn.microsoft.com/en-us/uwp/api/windows.data.pdf?view=winrt-26100) to to print PDF documents.
pub struct WinPdfPrinter {
    printer: PrinterDevice,
}

impl WinPdfPrinter {
    /// Create a new [`WinPdfPrinter`] for the given printer device.
    pub fn new(printer: PrinterDevice) -> Self {
        Self { printer }
    }
}

impl FilePrinter for WinPdfPrinter {
    type Options = PrintTicket;
    type Error = WinPdfPrinterError;
    fn print(&self, path: &Path, options: PrintTicket) -> Result<(), WinPdfPrinterError> {
        self.print_with_progress(path, options, |_, _| {})
    }
}

impl WinPdfPrinter {
    /// Print one PDF as one job, reporting each page after Direct2D accepts it.
    ///
    /// Counts describe pages processed, not physical output or terminal spooler success.
    /// The final result still reports cancellation/failure even after every page was accepted.
    /// The callback runs synchronously on the calling thread; it does not require `Send`.
    pub fn print_with_progress(
        &self,
        path: &Path,
        options: PrintTicket,
        progress: impl FnMut(u32, u32),
    ) -> Result<(), WinPdfPrinterError> {
        self.print_at(path, options, None, progress)
    }
}

/// The resolution Direct2D (and `Windows.Data.Pdf` with it) draws a page at: 96 per inch.
const DIP_PER_INCH: f32 = 96.0;

impl WinPdfPrinter {
    /// Print `path` rendering at `dpi`, else at the job's resolution (the DEVMODE of the ticket).
    ///
    /// `Windows.Data.Pdf` draws the images of a page at the destination size, 96 dpi by default
    /// whatever the images are, so a page is rendered `dpi / 96` times larger and the transform
    /// scales it back onto the page.
    fn print_at(
        &self,
        path: &Path,
        options: PrintTicket,
        dpi: Option<u32>,
        mut progress: impl FnMut(u32, u32),
    ) -> Result<(), WinPdfPrinterError> {
        let context = DxgiPrintContext::new(
            &self.printer,
            &options,
            path.file_name().unwrap_or(path.as_ref()),
        )?;
        let dxgi_device = &context.dxgi_device;
        let print_control = &context.print_control;
        let d2d_context = &context.d2d_context;
        let scale = dpi.unwrap_or(context.dpi) as f32 / DIP_PER_INCH;
        unsafe {
            let absolute_path =
                std::path::absolute(path).map_err(WinPdfPrinterError::InvalidPath)?;
            let file = StorageFile::GetFileFromPathAsync(&HSTRING::from(absolute_path.as_path()))
                .map_err(WinPdfPrinterError::FailedToOpenDocument)?
                .join()
                .map_err(WinPdfPrinterError::FailedToOpenDocument)?;
            let pdf_document = PdfDocument::LoadFromFileAsync(&file)
                .map_err(WinPdfPrinterError::FailedToOpenDocument)?
                .join()
                .map_err(WinPdfPrinterError::FailedToOpenDocument)?;

            let pdf_renderer =
                PdfCreateRenderer(dxgi_device).map_err(WinPdfPrinterError::RenderError)?;
            let page_count = pdf_document
                .PageCount()
                .map_err(WinPdfPrinterError::RenderError)?;
            progress(0, page_count);
            for i in 0..page_count {
                let pdf_page = pdf_document
                    .GetPage(i)
                    .map_err(WinPdfPrinterError::RenderError)?;

                let pdf_size = pdf_page.Size().map_err(WinPdfPrinterError::RenderError)?;
                let pdf_render_options = PDF_RENDER_PARAMS {
                    DestinationWidth: scaled(pdf_size.Width, scale)?,
                    DestinationHeight: scaled(pdf_size.Height, scale)?,
                    IgnoreHighContrast: true.into(),
                    ..Default::default()
                };
                let command_list = d2d_context
                    .CreateCommandList()
                    .map_err(WinPdfPrinterError::RenderError)?;
                d2d_context.SetTarget(&command_list);
                d2d_context.BeginDraw();
                d2d_context.SetTransform(&Matrix3x2 {
                    M11: 1.0 / scale,
                    M22: 1.0 / scale,
                    ..Matrix3x2::identity()
                });
                let rendered = pdf_renderer.RenderPageToDeviceContext(
                    &pdf_page,
                    d2d_context,
                    Some(&pdf_render_options),
                );
                d2d_context.SetTransform(&Matrix3x2::identity());
                rendered.map_err(WinPdfPrinterError::RenderError)?;
                d2d_context
                    .EndDraw(None, None)
                    .map_err(WinPdfPrinterError::RenderError)?;
                command_list
                    .Close()
                    .map_err(WinPdfPrinterError::RenderError)?;

                let dx_size = D2D_SIZE_F {
                    width: pdf_size.Width as f32,
                    height: pdf_size.Height as f32,
                };
                print_control
                    .AddPage(&command_list, dx_size, None, None, None)
                    .map_err(WinPdfPrinterError::RenderError)?;
                progress(i + 1, page_count);
            }
        }
        context.close_and_wait()?;
        Ok(())
    }
}

/// A page side in pixels at `scale` times its size in DIPs.
fn scaled(dips: f32, scale: f32) -> Result<u32, WinPdfPrinterError> {
    let pixels = (dips * scale).ceil();
    if pixels >= 1.0 && pixels < u32::MAX as f32 {
        Ok(pixels as u32)
    } else {
        Err(WinPdfPrinterError::PageSize { dips, scale })
    }
}

#[cfg(test)]
mod tests {
    use super::{scaled, WinPdfPrinter};
    use crate::{printer::FilePrinter, test_utils::null_device};
    use std::path::Path;

    #[test]
    fn print_simple_pdf_document() {
        let device = null_device::thread_local();
        let pdf = WinPdfPrinter::new(device);
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("test_data/test_document.pdf");
        pdf.print(path.as_path(), Default::default()).unwrap();
    }

    /// A4 at 1600 dpi is 13229 x 18709 px, taller than the 16384 texels Direct3D 11 allows a
    /// texture. The page is still rendered whole, in one pass: `RenderPageToDeviceContext` records
    /// into a command list and never allocates a page-sized texture.
    #[test]
    fn a_page_larger_than_a_texture_prints_at_a_high_resolution() {
        let device = null_device::thread_local();
        let pdf = WinPdfPrinter::new(device);
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("test_data/test_document.pdf");
        pdf.print_at(path.as_path(), Default::default(), Some(1600), |_, _| {})
            .unwrap();
    }

    #[test]
    fn a_page_side_is_scaled_and_never_overflows() {
        assert_eq!(scaled(793.7, 1600.0 / 96.0).unwrap(), 13229);
        assert_eq!(scaled(1122.5, 1.0).unwrap(), 1123);
        assert!(scaled(0.0, 1.0).is_err());
        assert!(scaled(f32::NAN, 1.0).is_err());
        assert!(scaled(1e12, 100.0).is_err());
    }
}
