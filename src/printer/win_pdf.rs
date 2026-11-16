use super::DxgiPrintContext;
use super::DxgiPrintContextError;
use crate::printer::FilePrinter;
use crate::printer::PrinterDevice;
use crate::ticket::PrintTicket;
use std::ffi::c_void;
use std::path::Path;
use std::sync::OnceLock;
use thiserror::Error;
use windows::core::s;
use windows::core::w;
use windows::core::Interface;
use windows::core::HRESULT;
use windows::core::HSTRING;
use windows::core::PCWSTR;
use windows::Data::Pdf::PdfDocument;
use windows::Storage::FileAccessMode;
use windows::Storage::Streams::IRandomAccessStream;
use windows::Win32::Foundation::FreeLibrary;
use windows::Win32::Foundation::E_POINTER;
use windows::Win32::Graphics::Direct2D::Common::D2D_SIZE_F;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::LibraryLoader::GetProcAddress;
use windows::Win32::System::LibraryLoader::LoadLibraryExW;
use windows::Win32::System::LibraryLoader::LOAD_LIBRARY_SEARCH_SYSTEM32;
use windows::Win32::System::WinRT::CreateRandomAccessStreamOnFile;
use windows::Win32::System::WinRT::Pdf::IPdfRendererNative;
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
    /// A page could not be rendered or recorded at the requested resolution.
    ///
    /// Direct3D 11 guarantees textures only up to 16384 texels a side
    /// (<https://learn.microsoft.com/windows/win32/direct3d11/overviews-direct3d-11-resources-limits>)
    /// and Direct2D printing limits a bitmap to 65535 device units
    /// (<https://learn.microsoft.com/windows/win32/direct2d/d1163>). Neither was reached when
    /// pages were rendered whole up to A4 at 9600 dpi, but a driver or GPU may have its own limits.
    #[error("The page could not be rendered at {render_dpi:?} dpi ({render_size_px:?} pixels)")]
    RenderAtResolution {
        /// The render resolution.
        render_dpi: (u32, u32),
        /// The render size in pixels at that resolution.
        render_size_px: (u32, u32),
        /// The failing call's error.
        #[source]
        source: windows::core::Error,
    },
    /// Render error.
    #[error("Render error")]
    RenderError(#[source] windows::core::Error),
    /// The system's PDF renderer is not available: `windows.data.pdf.dll` could not be loaded
    /// from the system folder, or it does not export `PdfCreateRenderer`. The source error is
    /// the loader's error code. No print job was created.
    #[error("The Windows PDF renderer (windows.data.pdf.dll) is not available on this system")]
    PdfRendererUnavailable(#[source] windows::core::Error),
}

/// The system library that exports
/// [`PdfCreateRenderer`](https://learn.microsoft.com/windows/win32/api/windows.data.pdf.interop/nf-windows-data-pdf-interop-pdfcreaterenderer).
const PDF_LIBRARY: PCWSTR = w!("windows.data.pdf.dll");

type PdfCreateRendererFn = unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT;

/// `PdfCreateRenderer`, looked up the first time a PDF is printed and kept for the process.
///
/// The function is not imported when the program loads: a program that imports a missing DLL
/// does not start at all, and some Windows installations do not have `windows.data.pdf.dll`.
/// On those, printing a PDF returns [`WinPdfPrinterError::PdfRendererUnavailable`].
fn pdf_create_renderer() -> Result<PdfCreateRendererFn, WinPdfPrinterError> {
    static RESOLVED: OnceLock<Result<PdfCreateRendererFn, HRESULT>> = OnceLock::new();
    let resolved =
        RESOLVED.get_or_init(|| resolve_pdf_create_renderer(PDF_LIBRARY).map_err(|e| e.code()));
    resolved.map_err(|code| WinPdfPrinterError::PdfRendererUnavailable(code.into()))
}

/// Load `library` from the system folder and look up its `PdfCreateRenderer`.
fn resolve_pdf_create_renderer(library: PCWSTR) -> windows::core::Result<PdfCreateRendererFn> {
    // SAFETY: `library` is a NUL-terminated wide string. LOAD_LIBRARY_SEARCH_SYSTEM32 loads
    // only the copy in the system folder, never a file of the same name next to the program
    // or in the current folder. The module stays loaded for the rest of the process, so the
    // returned function pointer stays valid.
    let module = unsafe { LoadLibraryExW(library, None, LOAD_LIBRARY_SEARCH_SYSTEM32) }?;
    // SAFETY: `module` was loaded above and the name is a NUL-terminated ANSI string.
    let Some(address) = (unsafe { GetProcAddress(module, s!("PdfCreateRenderer")) }) else {
        let error = windows::core::Error::from_thread();
        // SAFETY: nothing from the module is in use.
        let _ = unsafe { FreeLibrary(module) };
        return Err(error);
    };
    // SAFETY: PdfCreateRenderer takes an IDXGIDevice and an out pointer to an
    // IPdfRendererNative, and returns an HRESULT (windows.data.pdf.interop.h).
    Ok(unsafe {
        std::mem::transmute::<unsafe extern "system" fn() -> isize, PdfCreateRendererFn>(address)
    })
}

/// Create the renderer that draws PDF pages into `device`'s Direct2D device contexts.
fn create_pdf_renderer(
    create: PdfCreateRendererFn,
    device: &IDXGIDevice,
) -> Result<IPdfRendererNative, WinPdfPrinterError> {
    let mut renderer = std::ptr::null_mut();
    // SAFETY: `device` holds a reference for the duration of the call. On success the out
    // pointer holds a reference to the new renderer, which `from_raw` takes over.
    unsafe { create(device.as_raw(), &mut renderer) }
        .ok()
        .map_err(WinPdfPrinterError::RenderError)?;
    if renderer.is_null() {
        return Err(WinPdfPrinterError::RenderError(E_POINTER.into()));
    }
    // SAFETY: `renderer` is a non-null IPdfRendererNative that nothing else releases.
    Ok(unsafe { IPdfRendererNative::from_raw(renderer) })
}

/// A printer that uses [`Windows.Data.Pdf`](https://learn.microsoft.com/en-us/uwp/api/windows.data.pdf?view=winrt-26100) to to print PDF documents.
pub struct WinPdfPrinter {
    printer: PrinterDevice,
    override_render_dpi: Option<(u32, u32)>,
}

impl WinPdfPrinter {
    /// Create a new [`WinPdfPrinter`] for the given printer device.
    pub fn new(printer: PrinterDevice) -> Self {
        Self {
            printer,
            override_render_dpi: None,
        }
    }

    /// Override the resolution `Windows.Data.Pdf` draws a page at. By default it is the
    /// resolution the job's ticket asks for ([`DxgiPrintContext::dpi`]).
    pub fn override_render_dpi(&mut self, x: u32, y: u32) {
        self.override_render_dpi = Some((x, y));
    }
}

const DIP_PER_INCH: f32 = 96.0;

/// A page side in pixels at `scale` times its size in DIPs.
fn scaled(dips: f32, scale: f32) -> Result<u32, WinPdfPrinterError> {
    let pixels = (dips * scale).ceil();
    if pixels >= 1.0 && pixels < u32::MAX as f32 {
        Ok(pixels as u32)
    } else {
        Err(WinPdfPrinterError::PageSize { dips, scale })
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
        // Before the job is created, so a system without the renderer gets no empty job.
        let create_renderer = pdf_create_renderer()?;
        let context = DxgiPrintContext::new(
            &self.printer,
            &options,
            path.file_name().unwrap_or(path.as_ref()),
        )?;
        let dxgi_device = &context.dxgi_device;
        let print_control = &context.print_control;
        let d2d_context = &context.d2d_context;
        let render_dpi = self.override_render_dpi.unwrap_or(context.dpi);
        let scale = (
            render_dpi.0 as f32 / DIP_PER_INCH,
            render_dpi.1 as f32 / DIP_PER_INCH,
        );
        unsafe {
            let pdf_document = load_document(path)?;

            let pdf_renderer = create_pdf_renderer(create_renderer, dxgi_device)?;
            let page_count = pdf_document
                .PageCount()
                .map_err(WinPdfPrinterError::RenderError)?;
            progress(0, page_count);
            for i in 0..page_count {
                let pdf_page = pdf_document
                    .GetPage(i)
                    .map_err(WinPdfPrinterError::RenderError)?;

                // Without a destination size the page is drawn at its size in DIPs, so
                // whatever Windows.Data.Pdf rasterizes (images, Type 3 glyphs) reaches the
                // sheet at about 96 dpi. Draw it at the job's resolution and scale the result
                // back to the page size.
                let pdf_size = pdf_page.Size().map_err(WinPdfPrinterError::RenderError)?;
                let render_size_px = (
                    scaled(pdf_size.Width, scale.0)?,
                    scaled(pdf_size.Height, scale.1)?,
                );
                let at_resolution = |source| WinPdfPrinterError::RenderAtResolution {
                    render_dpi,
                    render_size_px,
                    source,
                };
                let pdf_render_options = PDF_RENDER_PARAMS {
                    DestinationWidth: render_size_px.0,
                    DestinationHeight: render_size_px.1,
                    IgnoreHighContrast: true.into(),
                    ..Default::default()
                };
                let command_list = d2d_context
                    .CreateCommandList()
                    .map_err(WinPdfPrinterError::RenderError)?;
                d2d_context.SetTarget(&command_list);
                d2d_context.BeginDraw();
                // Scale by the final pixel size, not by `scale`: the size is rounded up to
                // whole pixels, and this keeps the page exactly its size.
                d2d_context.SetTransform(&Matrix3x2::scale(
                    pdf_size.Width / pdf_render_options.DestinationWidth as f32,
                    pdf_size.Height / pdf_render_options.DestinationHeight as f32,
                ));
                let rendered = pdf_renderer.RenderPageToDeviceContext(
                    &pdf_page,
                    d2d_context,
                    Some(&pdf_render_options),
                );
                d2d_context.SetTransform(&Matrix3x2::identity());
                rendered.map_err(at_resolution)?;
                d2d_context.EndDraw(None, None).map_err(at_resolution)?;
                command_list
                    .Close()
                    .map_err(WinPdfPrinterError::RenderError)?;

                let dx_size = D2D_SIZE_F {
                    width: pdf_size.Width as f32,
                    height: pdf_size.Height as f32,
                };
                print_control
                    .AddPage(&command_list, dx_size, None, None, None)
                    .map_err(at_resolution)?;
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

    /// A4 at 1600 dpi is 13229 x 18709 px, taller than the 16384 texels Direct3D 11 allows a
    /// texture. The page is still rendered whole, in one pass: `RenderPageToDeviceContext` records
    /// into a command list and never allocates a page-sized texture.
    #[test]
    fn a_page_larger_than_a_texture_prints_at_a_high_resolution() {
        let device = null_device::thread_local();
        let mut pdf = WinPdfPrinter::new(device);
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("test_data/test_document.pdf");
        pdf.override_render_dpi(1600, 1600);
        pdf.print(path.as_path(), Default::default()).unwrap();
    }

    #[test]
    fn a_page_side_is_scaled_and_never_overflows() {
        use super::scaled;
        assert_eq!(scaled(793.7, 1600.0 / 96.0).unwrap(), 13229);
        assert_eq!(scaled(1122.5, 1.0).unwrap(), 1123);
        assert!(scaled(0.0, 1.0).is_err());
        assert!(scaled(f32::NAN, 1.0).is_err());
        assert!(scaled(1e12, 100.0).is_err());
    }

    #[test]
    fn the_pdf_renderer_resolves_from_the_system_library() {
        let system_root = std::env::var_os("SystemRoot").unwrap();
        let installed = std::path::Path::new(&system_root)
            .join("System32")
            .join("windows.data.pdf.dll")
            .is_file();
        match super::pdf_create_renderer() {
            Ok(_) => assert!(installed),
            Err(super::WinPdfPrinterError::PdfRendererUnavailable(_)) => assert!(!installed),
            Err(error) => panic!("unexpected error: {error}"),
        }
    }

    #[test]
    fn a_missing_pdf_library_is_an_error() {
        use windows::Win32::Foundation::ERROR_MOD_NOT_FOUND;
        let error =
            super::resolve_pdf_create_renderer(windows::core::w!("winprint-no-such-library.dll"))
                .err()
                .unwrap();
        assert_eq!(error.code(), ERROR_MOD_NOT_FOUND.to_hresult());
    }

    #[test]
    fn a_library_without_the_renderer_export_is_an_error() {
        use windows::Win32::Foundation::ERROR_PROC_NOT_FOUND;
        let error = super::resolve_pdf_create_renderer(windows::core::w!("kernel32.dll"))
            .err()
            .unwrap();
        assert_eq!(error.code(), ERROR_PROC_NOT_FOUND.to_hresult());
    }

    #[test]
    fn an_unavailable_renderer_names_the_library() {
        use windows::Win32::Foundation::ERROR_MOD_NOT_FOUND;
        let error = super::WinPdfPrinterError::PdfRendererUnavailable(
            ERROR_MOD_NOT_FOUND.to_hresult().into(),
        );
        assert!(error.to_string().contains("windows.data.pdf.dll"));
    }

    /// The names of the DLLs in a PE image's import directory, which the loader must find
    /// before the program starts.
    fn imported_libraries(image: &[u8]) -> Vec<String> {
        let u16_at = |at: usize| u16::from_le_bytes(image[at..at + 2].try_into().unwrap());
        let u32_at = |at: usize| u32::from_le_bytes(image[at..at + 4].try_into().unwrap());
        let pe = u32_at(0x3c) as usize;
        assert_eq!(&image[pe..pe + 4], b"PE\0\0");
        let coff = pe + 4;
        let sections = u16_at(coff + 2) as usize;
        let optional = coff + 20;
        let section_table = optional + u16_at(coff + 16) as usize;
        // The data directories start 96 bytes into a PE32 optional header and 112 bytes into a
        // PE32+ one; the import directory is the second.
        let directories = optional
            + match u16_at(optional) {
                0x10b => 96,
                0x20b => 112,
                magic => panic!("unknown optional header magic {magic:#x}"),
            };
        let offset = |rva: u32| -> usize {
            (0..sections)
                .find_map(|i| {
                    let section = section_table + i * 40;
                    let size = u32_at(section + 8).max(u32_at(section + 16));
                    let address = u32_at(section + 12);
                    (rva >= address && rva - address < size)
                        .then(|| (rva - address + u32_at(section + 20)) as usize)
                })
                .unwrap()
        };
        let mut libraries = Vec::new();
        let mut descriptor = offset(u32_at(directories + 8));
        loop {
            let name = u32_at(descriptor + 12);
            if name == 0 {
                break;
            }
            let name = &image[offset(name)..];
            let end = name.iter().position(|&b| b == 0).unwrap();
            libraries.push(String::from_utf8_lossy(&name[..end]).to_ascii_lowercase());
            descriptor += 20;
        }
        libraries
    }

    /// This test binary contains `WinPdfPrinter::print_with_progress` (the tests above print
    /// with it), so a load-time import of the renderer would appear in its import directory.
    #[test]
    fn the_pdf_library_is_not_imported_at_load_time() {
        let image = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        let libraries = imported_libraries(&image);
        assert!(
            libraries.iter().any(|l| l == "kernel32.dll"),
            "{libraries:?}"
        );
        assert!(
            !libraries.iter().any(|l| l == "windows.data.pdf.dll"),
            "{libraries:?}"
        );
    }
}
