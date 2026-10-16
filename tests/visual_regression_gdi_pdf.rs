//! End-to-end visual regression for [`GdiPdfPrinter`].

#![cfg(all(windows, feature = "test-utils"))]

mod common;
use common::print;
use common::visual::VisualExpectation;
use winprint::{
    printer::{FilePrinter, GdiPdfPrinter},
    ticket::{
        FeatureOptionPackWithPredefined, PredefinedMediaName, PrintCapabilities, PrintTicket,
    },
};

#[test]
fn gdi_pdf_visual_regression() {
    let _ = env_logger::try_init();
    let visual_regression_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("test_data")
        .join("visual_regression");
    let expectation = VisualExpectation::new(visual_regression_dir.join("data.tiff"));
    let actual_pages = print::print_pages(|device| {
        let origin = visual_regression_dir.join("data.pdf");
        let printer = GdiPdfPrinter::new(device.clone());
        let capabilities = PrintCapabilities::fetch(device).unwrap();
        let media_size = capabilities
            .page_media_sizes()
            .find(|x| x.as_predefined_name() == Some(PredefinedMediaName::ISOA4))
            .unwrap();
        let print_ticket: PrintTicket = media_size.into();
        printer
            .print(origin.as_path(), print_ticket)
            .expect("failed to print the document");
    });
    expectation.assert(&actual_pages);
}
