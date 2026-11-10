//! File glyphs and highlight languages by extension (SwiftUI FilesView.swift).

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::ui) struct FileKind {
    pub glyph: &'static str,
    pub language: &'static str,
}

const KINDS: &[(&[&str], &str, &str)] = &[
    (&["py", "pyi", "pyw"], "hi.python", "python"),
    (&["ts"], "hi.typeScript", "typescript"),
    (&["tsx"], "hi.typeScript", "tsx"),
    (&["js", "jsx", "mjs", "cjs"], "hi.javaScript", "javascript"),
    (&["csv", "tsv"], "hi.csv", "text"),
    (&["rs"], "hi.rust", "rust"),
    (&["go"], "hi.go", "go"),
    (&["md", "mdx", "markdown"], "hi.markdown", "markdown"),
    (&["swift"], "hi.fileScript", "swift"),
    (&["c", "h"], "hi.fileScript", "c"),
    (&["cpp", "hpp", "cc", "cxx"], "hi.fileScript", "cpp"),
    (&["java"], "hi.fileScript", "java"),
    (&["kt", "kts"], "hi.fileScript", "kotlin"),
    (&["rb"], "hi.fileScript", "ruby"),
    (&["php"], "hi.fileScript", "text"),
    (&["sh", "zsh", "bash"], "hi.fileScript", "bash"),
    (&["html", "htm"], "hi.fileScript", "html"),
    (&["css", "scss"], "hi.fileScript", "css"),
    (&["diff", "patch"], "hi.plusMinus02", "diff"),
    (&["doc", "docx", "odt", "pages", "rtf"], "hi.doc", "text"),
    (
        &["png", "jpg", "jpeg", "gif", "heic", "webp", "svg"],
        "hi.image01",
        "text",
    ),
    (&["json"], "hi.gear", "json"),
    (&["yaml", "yml"], "hi.gear", "yaml"),
    (&["toml"], "hi.gear", "toml"),
    (&["xml", "ini", "plist"], "hi.gear", "text"),
];

impl FileKind {
    pub fn for_name(name: &str) -> Self {
        let extension = std::path::Path::new(name)
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        KINDS
            .iter()
            .find(|(extensions, ..)| extensions.contains(&extension.as_str()))
            .map_or(
                Self {
                    glyph: "hi.fileText",
                    language: "text",
                },
                |(_, glyph, language)| Self { glyph, language },
            )
    }

    pub fn is_markdown(self) -> bool {
        self.language == "markdown"
    }
}
