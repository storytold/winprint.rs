use super::DxgiPrintContext;
use super::DxgiPrintContextError;
use crate::printer::FilePrinter;
use crate::printer::PrinterDevice;
use crate::ticket::PrintTicket;
use std::path::Path;
use thiserror::Error;
use windows::core::HSTRING;
use windows::Data::Pdf::PdfDocument;
use windows::Storage::FileAccessMode;
use windows::Storage::Streams::IRandomAccessStream;
use windows::Win32::Graphics::Direct2D::Common::D2D_SIZE_F;
use windows::Win32::System::WinRT::CreateRandomAccessStreamOnFile;
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

// StorageFile's broker can refuse otherwise readable desktop job files. Open a
// read-only, file-backed WinRT stream directly: no whole-file copy and no per-chunk
// DataWriter/StoreAsync operations. The PDF renderer reads the file on demand.
fn open_document_stream(path: &Path) -> Result<IRandomAccessStream, WinPdfPrinterError> {
    // On Windows, absolute also rejects embedded NULs before any native stream call.
    let absolute_path = std::path::absolute(path).map_err(WinPdfPrinterError::InvalidPath)?;
    let file_path = HSTRING::from(absolute_path.as_path());
    // SAFETY: HSTRING owns a NUL-terminated path for the duration of the call.
    // The windows binding supplies IRandomAccessStream's IID and owns the returned
    // COM reference. Read access does not create or modify the input file.
    unsafe { CreateRandomAccessStreamOnFile(&file_path, FileAccessMode::Read.0 as u32) }
        .map_err(WinPdfPrinterError::FailedToOpenDocument)
}

fn load_document(path: &Path) -> Result<PdfDocument, WinPdfPrinterError> {
    let stream = open_document_stream(path)?;
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
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .attributes(2)
            .open(&path)
            .unwrap(); // FILE_ATTRIBUTE_HIDDEN
        let path = scopeguard::guard(path, |path| {
            let _ = std::fs::remove_file(path);
        });
        // Handles created below the guard are released before its cleanup on a panic.
        {
            let mut output = file;
            output
                .write_all(include_bytes!("../../test_data/test_document.pdf"))
                .unwrap();
        }
        {
            let document = super::load_document(&path).unwrap();
            assert!(document.PageCount().unwrap() > 0);
            let page = document.GetPage(0).unwrap();
            let size = page.Size().unwrap();
            assert!(size.Width > 0.0 && size.Height > 0.0);
        }
        // Verify normal cleanup only after the page and document have been released.
        std::fs::remove_file(&*path).unwrap();
    }

    #[test]
    fn file_stream_reads_the_end_of_a_large_file_on_demand() {
        use std::io::{Seek, SeekFrom, Write};
        use std::time::{Duration, Instant};
        use windows::Storage::Streams::{Buffer, DataReader, InputStreamOptions};
        let _com = crate::utils::com::ComInitializer::new();
        let path =
            std::env::temp_dir().join(format!("winprint-large-{}.bin", uuid::Uuid::new_v4()));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let path = scopeguard::guard(path, |path| {
            let _ = std::fs::remove_file(path);
        });
        let size = 256 * 1024 * 1024;
        {
            let mut output = file;
            output.set_len(size).unwrap();
            output.seek(SeekFrom::End(-1)).unwrap();
            output.write_all(&[0xa5]).unwrap();
            output.sync_all().unwrap();
        }
        {
            let stream = super::open_document_stream(&path).unwrap();
            assert_eq!(stream.Size().unwrap(), size);
            assert!(stream.CanRead().unwrap());
            assert!(!stream.CanWrite().unwrap());
            let input = stream.GetInputStreamAt(size - 1).unwrap();
            let buffer = Buffer::Create(1).unwrap();
            let read = input
                .ReadAsync(&buffer, 1, InputStreamOptions::None)
                .unwrap();
            let started = Instant::now();
            // AsyncStatus::Started is 0. Poll the native operation rather than waiting
            // indefinitely for a completion callback: this test previously hung in CI.
            while read.Status().unwrap().0 == 0 {
                if started.elapsed() >= Duration::from_secs(30) {
                    let _ = read.Cancel();
                    panic!("the one-byte file-backed read did not complete within 30 seconds");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let bytes = read.GetResults().unwrap();
            assert_eq!(bytes.Length().unwrap(), 1);
            let reader = DataReader::FromBuffer(&bytes).unwrap();
            assert_eq!(reader.ReadByte().unwrap(), 0xa5);
        }
        std::fs::remove_file(&*path).unwrap();
    }

    #[test]
    fn a_missing_job_is_an_error_without_creating_a_file() {
        let _com = crate::utils::com::ComInitializer::new();
        let path =
            std::env::temp_dir().join(format!("winprint-missing-{}.pdf", uuid::Uuid::new_v4()));
        assert!(super::load_document(&path).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn windows_path_normalization_rejects_embedded_nul() {
        let _com = crate::utils::com::ComInitializer::new();
        let path = std::path::Path::new("job.pdf\0ignored");
        assert!(matches!(
            super::open_document_stream(path),
            Err(super::WinPdfPrinterError::InvalidPath(error))
                if error.kind() == std::io::ErrorKind::InvalidInput
        ));
    }

    #[test]
    fn document_stream_is_read_only() {
        let _com = crate::utils::com::ComInitializer::new();
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("test_data/test_document.pdf");
        let stream = super::open_document_stream(&path).unwrap();
        assert!(stream.CanRead().unwrap());
        assert!(!stream.CanWrite().unwrap());
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
