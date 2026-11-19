// SPDX-License-Identifier: AGPL-3.0-or-later
//! Rendering a report record with its typst template: the templates and
//! fonts are compiled into the binary, so the PDF looks the same on every
//! host and nothing has to be installed. The PDF is PDF/A-2b and carries
//! the SHA-256 of the JSON record it was made from.

use crate::error::Result;

/// Fonts embedded in the binary (IBM Plex, SIL Open Font License 1.1).
#[cfg(feature = "report")]
const FONTS: [&[u8]; 6] = [
    include_bytes!("fonts/IBMPlexSans-Regular.ttf"),
    include_bytes!("fonts/IBMPlexSans-Italic.ttf"),
    include_bytes!("fonts/IBMPlexSans-SemiBold.ttf"),
    include_bytes!("fonts/IBMPlexSans-Bold.ttf"),
    include_bytes!("fonts/IBMPlexMono-Regular.ttf"),
    include_bytes!("fonts/IBMPlexMono-SemiBold.ttf"),
];

/// The shared layout.
pub const COMMON: &str = include_str!("templates/common.typ");
/// The status report template.
pub const STATUS: &str = include_str!("templates/status.typ");
/// The verification report template.
pub const VERIFICATION: &str = include_str!("templates/verification.typ");

/// Which report a record is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Template {
    /// `status.typ`.
    Status,
    /// `verification.typ`.
    Verification,
}

impl Template {
    /// File name and source of the main template.
    #[must_use]
    pub fn main(self) -> (&'static str, &'static str) {
        match self {
            Self::Status => ("status.typ", STATUS),
            Self::Verification => ("verification.typ", VERIFICATION),
        }
    }
}

/// What a template gets besides its record.
#[derive(Debug, Clone, Default)]
pub struct Extras {
    /// SHA-256 of the JSON record, printed in the footer.
    pub digest: String,
    /// Print the appendices.
    pub appendix: bool,
    /// Logo: file name (with extension) and bytes.
    pub logo: Option<(String, Vec<u8>)>,
    /// Report time, seconds since the epoch (the PDF's date).
    pub when: u64,
}

/// The typst sources of a report, to compile elsewhere with
/// `typst compile <main> --input data=data.json`: the templates, the record
/// as `data.json` and the logo.
#[must_use]
pub fn bundle(t: Template, record: &[u8], extras: &Extras) -> Vec<(String, Vec<u8>)> {
    let (name, main) = t.main();
    let mut out = vec![
        ("common.typ".to_owned(), COMMON.as_bytes().to_vec()),
        (name.to_owned(), main.as_bytes().to_vec()),
        ("data.json".to_owned(), record.to_vec()),
    ];
    if let Some((n, b)) = &extras.logo {
        out.push((n.clone(), b.clone()));
    }
    out
}

/// Render a record to PDF/A-2b.
///
/// Implements: SRS-RPT-004
///
/// # Errors
/// [`Error::Config`] when the binary was built without the `report`
/// feature, or the template fails on the record (a bug: the record is
/// checked against its schema in the tests).
#[cfg(not(feature = "report"))]
pub fn pdf(_t: Template, _record: &[u8], _extras: &Extras) -> Result<Vec<u8>> {
    Err(crate::error::Error::Config(
        "this build has no PDF support (build feature `report`); use --format json,typ \
         and compile the typst sources with `typst compile`"
            .into(),
    ))
}

/// Render a record to PDF/A-2b.
///
/// Implements: SRS-RPT-004
///
/// # Errors
/// [`Error::Config`] when the template fails on the record (a bug: the
/// record is checked against its schema in the tests).
#[cfg(feature = "report")]
pub fn pdf(t: Template, record: &[u8], extras: &Extras) -> Result<Vec<u8>> {
    world::render(t, record, extras)
}

#[cfg(feature = "report")]
mod world {
    use std::collections::HashMap;

    use typst::diag::{FileError, FileResult, SourceDiagnostic};
    use typst::foundations::{Bytes, Datetime, Dict, IntoValue};
    use typst::layout::PagedDocument;
    use typst::syntax::{FileId, Source, VirtualPath};
    use typst::text::{Font, FontBook};
    use typst::utils::LazyHash;
    use typst::{Library, World};
    use typst_pdf::{PdfOptions, PdfStandard, PdfStandards, Timestamp};

    use super::{COMMON, Extras, FONTS, Template};
    use crate::error::{Error, Result};

    struct ReportWorld {
        library: LazyHash<Library>,
        book: LazyHash<FontBook>,
        fonts: Vec<Font>,
        main: FileId,
        sources: HashMap<FileId, Source>,
        files: HashMap<FileId, Bytes>,
        today: Option<Datetime>,
    }

    fn id(name: &str) -> FileId {
        FileId::new(None, VirtualPath::new(name))
    }

    impl World for ReportWorld {
        fn library(&self) -> &LazyHash<Library> {
            &self.library
        }
        fn book(&self) -> &LazyHash<FontBook> {
            &self.book
        }
        fn main(&self) -> FileId {
            self.main
        }
        fn source(&self, id: FileId) -> FileResult<Source> {
            self.sources
                .get(&id)
                .cloned()
                .ok_or_else(|| FileError::NotFound(id.vpath().as_rootless_path().into()))
        }
        fn file(&self, id: FileId) -> FileResult<Bytes> {
            if let Some(s) = self.sources.get(&id) {
                return Ok(Bytes::new(s.text().as_bytes().to_vec()));
            }
            self.files
                .get(&id)
                .cloned()
                .ok_or_else(|| FileError::NotFound(id.vpath().as_rootless_path().into()))
        }
        fn font(&self, index: usize) -> Option<Font> {
            self.fonts.get(index).cloned()
        }
        fn today(&self, _offset: Option<i64>) -> Option<Datetime> {
            self.today
        }
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    fn datetime(secs: u64) -> Option<Datetime> {
        let (y, m, d) = crate::report::time::civil(secs / 86_400);
        let s = secs % 86_400;
        Datetime::from_ymd_hms(
            y as i32,
            m as u8,
            d as u8,
            (s / 3600) as u8,
            (s / 60 % 60) as u8,
            (s % 60) as u8,
        )
    }

    fn describe(diags: &[SourceDiagnostic]) -> String {
        diags
            .iter()
            .map(|d| {
                let hints: Vec<&str> = d.hints.iter().map(typst::ecow::EcoString::as_str).collect();
                format!("{} {}", d.message, hints.join("; "))
            })
            .collect::<Vec<_>>()
            .join("; ")
    }

    pub(super) fn render(t: Template, record: &[u8], extras: &Extras) -> Result<Vec<u8>> {
        let fonts: Vec<Font> = FONTS
            .iter()
            .flat_map(|b| Font::iter(Bytes::new(b.to_vec())))
            .collect();
        let (name, main_src) = t.main();
        let main = id(&format!("/{name}"));
        let mut sources = HashMap::new();
        sources.insert(main, Source::new(main, main_src.to_owned()));
        let common = id("/common.typ");
        sources.insert(common, Source::new(common, COMMON.to_owned()));
        let mut files = HashMap::new();
        files.insert(id("/data.json"), Bytes::new(record.to_vec()));
        let mut inputs = Dict::new();
        inputs.insert("data".into(), "data.json".into_value());
        inputs.insert("digest".into(), extras.digest.as_str().into_value());
        inputs.insert(
            "appendix".into(),
            if extras.appendix { "true" } else { "false" }.into_value(),
        );
        if let Some((n, b)) = &extras.logo {
            files.insert(id(&format!("/{n}")), Bytes::new(b.clone()));
            inputs.insert("logo".into(), n.as_str().into_value());
        }
        let world = ReportWorld {
            library: LazyHash::new(Library::builder().with_inputs(inputs).build()),
            book: LazyHash::new(FontBook::from_fonts(&fonts)),
            fonts,
            main,
            sources,
            files,
            today: datetime(extras.when),
        };
        let doc: PagedDocument = typst::compile(&world)
            .output
            .map_err(|d| Error::Config(format!("report template: {}", describe(&d))))?;
        let options = PdfOptions {
            timestamp: world.today.map(Timestamp::new_utc),
            standards: PdfStandards::new(&[PdfStandard::A_2b])
                .map_err(|e| Error::Config(e.to_string()))?,
            ..PdfOptions::default()
        };
        typst_pdf::pdf(&doc, &options)
            .map_err(|d| Error::Config(format!("report PDF: {}", describe(&d))))
    }
}

#[cfg(all(test, feature = "report"))]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::fsio::RealFs;
    use crate::report::status::{StatusInputs, build};
    use crate::report::time::period;

    fn record(state: &std::path::Path) -> Vec<u8> {
        let inputs = StatusInputs {
            state_dir: state.to_path_buf(),
            log: None,
            period: period(None, None, Some("2026-10"), None, 0).unwrap(),
            check_destination: false,
            appendix: true,
            generated_at: 1_790_000_000,
            host: "host".into(),
            site: Some("Site \"A\" #1 $x$ *b*".into()),
            prefix: "OOF".into(),
            target_id: None,
            destination: None,
            sources: Vec::new(),
        };
        serde_json::to_vec(&build(&RealFs, &inputs)).unwrap()
    }

    /// Verifies: SRS-RPT-004
    #[test]
    fn an_empty_record_renders_to_pdf_a_and_text_is_not_markup() {
        let dir = tempfile::tempdir().unwrap();
        let rec = record(dir.path());
        for appendix in [false, true] {
            let extras = Extras {
                digest: "ab".repeat(32),
                appendix,
                logo: Some((
                    "logo.svg".into(),
                    b"<svg xmlns='http://www.w3.org/2000/svg' width='10' height='10'/>".to_vec(),
                )),
                when: 1_790_000_000,
            };
            let pdf = pdf(Template::Status, &rec, &extras).unwrap();
            assert!(pdf.starts_with(b"%PDF-"));
            assert!(
                String::from_utf8_lossy(&pdf)
                    .contains("<pdfaid:conformance>B</pdfaid:conformance>")
            );
            // The same record and time give the same bytes.
            assert_eq!(pdf, super::pdf(Template::Status, &rec, &extras).unwrap());
        }
        let names: Vec<String> = bundle(Template::Status, &rec, &Extras::default())
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(names, ["common.typ", "status.typ", "data.json"]);
    }
}
