use super::DxgiPrintContext;
use super::DxgiPrintContextError;
use crate::printer::FilePrinter;
use crate::printer::PrinterDevice;
use crate::ticket::PrintTicket;
use std::io::Read as _;
use std::path::Path;
use thiserror::Error;
use windows::Data::Pdf::PdfDocument;
use windows::Storage::Streams::{DataWriter, InMemoryRandomAccessStream};
use windows::Win32::Graphics::Direct2D::Common::D2D_SIZE_F;
use windows::Win32::System::WinRT::Pdf::PdfCreateRenderer;
use windows::Win32::System::WinRT::Pdf::PDF_RENDER_PARAMS;

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
        unsafe {
            let pdf_document = load_document(path)?;

            let pdf_renderer =
                PdfCreateRenderer(dxgi_device).map_err(WinPdfPrinterError::RenderError)?;
            let pdf_render_options = PDF_RENDER_PARAMS {
                IgnoreHighContrast: true.into(),
                ..Default::default()
            };
            let page_count = pdf_document
                .PageCount()
                .map_err(WinPdfPrinterError::RenderError)?;
            progress(0, page_count);
            for i in 0..page_count {
                let pdf_page = pdf_document
                    .GetPage(i)
                    .map_err(WinPdfPrinterError::RenderError)?;

                let command_list = d2d_context
                    .CreateCommandList()
                    .map_err(WinPdfPrinterError::RenderError)?;
                d2d_context.SetTarget(&command_list);
                d2d_context.BeginDraw();
                pdf_renderer
                    .RenderPageToDeviceContext(&pdf_page, d2d_context, Some(&pdf_render_options))
                    .map_err(WinPdfPrinterError::RenderError)?;
                d2d_context
                    .EndDraw(None, None)
                    .map_err(WinPdfPrinterError::RenderError)?;
                command_list
                    .Close()
                    .map_err(WinPdfPrinterError::RenderError)?;

                let pdf_size = pdf_page.Size().map_err(WinPdfPrinterError::RenderError)?;
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

// StorageFile path resolution can refuse otherwise readable desktop files (for example
// hidden temporary job files). Read with desktop file access and give Windows.Data.Pdf
// a stream instead. Transfer in chunks to avoid a second whole-file byte allocation.
fn load_document(path: &Path) -> Result<PdfDocument, WinPdfPrinterError> {
    let mut file = std::fs::File::open(path).map_err(WinPdfPrinterError::InvalidPath)?;
    let stream =
        InMemoryRandomAccessStream::new().map_err(WinPdfPrinterError::FailedToOpenDocument)?;
    let writer =
        DataWriter::CreateDataWriter(&stream).map_err(WinPdfPrinterError::FailedToOpenDocument)?;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(WinPdfPrinterError::InvalidPath)?;
        if count == 0 {
            break;
        }
        // Read guarantees count is within the supplied buffer.
        writer
            .WriteBytes(&buffer[..count])
            .map_err(WinPdfPrinterError::FailedToOpenDocument)?;
        writer
            .StoreAsync()
            .map_err(WinPdfPrinterError::FailedToOpenDocument)?
            .join()
            .map_err(WinPdfPrinterError::FailedToOpenDocument)?;
    }
    writer
        .DetachStream()
        .map_err(WinPdfPrinterError::FailedToOpenDocument)?;
    stream
        .Seek(0)
        .map_err(WinPdfPrinterError::FailedToOpenDocument)?;
    PdfDocument::LoadFromStreamAsync(&stream)
        .map_err(WinPdfPrinterError::FailedToOpenDocument)?
        .join()
        .map_err(WinPdfPrinterError::FailedToOpenDocument)
}

#[cfg(test)]
mod tests {
    #[test]
    fn loads_a_hidden_job_file_without_storage_broker_path_access() {
        use std::io::Write;
        use std::os::windows::fs::OpenOptionsExt;
        let _com = crate::utils::com::ComInitializer::new();
        let path =
            std::env::temp_dir().join(format!("winprint-hidden-{}.pdf", uuid::Uuid::new_v4()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .attributes(2)
            .open(&path)
            .unwrap(); // FILE_ATTRIBUTE_HIDDEN
        file.write_all(include_bytes!("../../test_data/test_document.pdf"))
            .unwrap();
        drop(file);
        let result = super::load_document(&path);
        std::fs::remove_file(&path).unwrap();
        assert!(result.unwrap().PageCount().unwrap() > 0);
    }

    use super::WinPdfPrinter;
    use crate::{printer::FilePrinter, test_utils::null_device};
    use std::path::Path;

    #[test]
    fn print_simple_pdf_document() {
        let device = null_device::thread_local();
        let pdf = WinPdfPrinter::new(device);
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("test_data/test_document.pdf");
        pdf.print(path.as_path(), Default::default()).unwrap();
    }
}
