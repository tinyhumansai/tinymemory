//! The `documents-office` feature reaches `OfficeConverter` through
//! `documents`, and it composes with the default converter chain.
#![cfg(feature = "documents-office")]

use tinymemory_integrations::documents::{
    ConverterChain, DocumentConverter, DocumentFormat, OfficeConverter,
};

#[test]
fn documents_office_feature_exposes_the_office_converter() {
    let chain = ConverterChain::default().prepend(Box::new(OfficeConverter));
    for format in [
        DocumentFormat::Pdf,
        DocumentFormat::Docx,
        DocumentFormat::Xlsx,
        DocumentFormat::Pptx,
    ] {
        assert!(chain.supports(format), "{format}");
    }
}
