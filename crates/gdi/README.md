# craft-winprint-gdi

Safe Windows driver settings, printable geometry and RGB/GDI transport. Other platforms return Unsupported. Callers compose page pixels and vector strokes and perform ICC conversion; the transport owns no app or document state. Manual copies use a validated single-copy DEVMODE. Application-managed and unmanaged RGB use ICM_DONE_OUTSIDEDC, reapplied after StartPage; driver-managed RGB uses ICM_OFF. Driver automatic color correction still belongs to its settings.

`Document` sends a job page by page, so no job holds every page in memory: each page is begun (optionally with new paper or orientation, applied with ResetDC after the driver validates it), drawn with RGB images placed in points or pixel bands placed in device pixels, and enhanced metafiles played into a rect (their vector calls reach the driver as vector calls), then ended. Metafiles are checked for a whole header before GDI reads them. Dropping an unfinished document cancels the job.

The Win32 module is isolated; no unsafe code is required in consumers. This package is a separate Cargo workspace, so tests do not enable the root package's PDFium feature. Run cargo test --manifest-path crates/gdi/Cargo.toml. Ignored driver tests require Microsoft Print to PDF and must be run only in a task-owned Windows test environment.

Derived from @Moiztermind's MIT OR Apache-2.0 PhotoCraft proposal https://github.com/storytold/photocraft/pull/3013, with qualification fixes by @skylartaylor. This package retains its source licenses; the root winprint package remains BSD-3-Clause.
