//! Keeps the design system in use: text sizes and families come from `Type`, glyph and
//! control sizes from the tokens, corners from `Shape` and colours from `Palette`
//! (`docs/DESIGN.md`). Keeps the interface a frontend: disk, process and network work
//! belongs to `gateway` or `platform` (`docs/backend-frontend-split.md`). A new exception
//! needs a line here and a reason.

use std::path::{Path, PathBuf};

/// Patterns that bypass the system, each with the only places allowed to use it.
const RULES: &[(&str, &[(&str, &str)])] = &[
    (
        "text_size(",
        &[
            // The window root's default size, taken from the scale.
            ("ui/mod.rs", "text_size(Type::Body.size())"),
            // The Apple logo is a font glyph, sized as a glyph.
            ("ui/primitives/controls.rs", "text_size(glyph::LEAD)"),
            // Face features scale with the face.
            (
                "ui/primitives/bot_face/draw.rs",
                "text_size(size * eye_size)",
            ),
            // A drifting z, laid out at the start of its life; its motion grows it.
            (
                "ui/primitives/bot_face/view.rs",
                "text_size(size * 0.2 * 0.6)",
            ),
        ],
    ),
    ("font_family(\"", &[]),
    // gpui's rem-based sizes; the app uses `Type` instead.
    (".text_xs()", &[]),
    (".text_sm()", &[]),
    (".text_base()", &[]),
    (".text_lg()", &[]),
    (".text_xl()", &[]),
    // Glyphs, spinners, rings and control boxes take `glyph::` or `height::` sizes.
    (".size(px(", &[]),
    (".with_size(px(", &[]),
    ("Spinner::new(px(", &[]),
    // Corners come from `Shape` and `pill_quad`, never a radius picked at the call site.
    (".rounded(", &[]),
    (".rounded_", &[]),
    (
        "corner_radii",
        &[
            // The picture behind a frosted surface, painted with the surface's corners.
            ("ui/backdrop.rs", "corner_radii(corners)"),
            // The library draws the chart's bars and takes their corners as data.
            (
                "ui/transcript/ui_document/chart.rs",
                ".corner_radii(Corners {",
            ),
            // A remote picture copies its frame's corners, as GPUI clips to rectangles only.
            (
                "ui/transcript/ui_document/document.rs",
                "style().corner_radii = ",
            ),
        ],
    ),
    (
        "Corners::all(",
        &[("ui/backdrop.rs", "Corners::all(radius)")],
    ),
    // Separators are `hairline`, at the `inset::HAIRLINE` thickness.
    (".h(px(1.))", &[]),
    (".w(px(1.))", &[]),
    // Colours come from `Palette`, which derives them in theme.rs.
    (
        "rgb(0x",
        &[
            ("ui/theme.rs", "rgb(0x"),
            // A remote screen letterboxes on black, whatever the theme.
            ("ui/panel/desktop.rs", "rgb(0x00_00_00)"),
            // macOS system blue marks an available update.
            ("ui/navigation/updates.rs", "rgb(0x0A_84_FF)"),
            // The accent tile is the app icon's fixed dark ground (nord1 to nord0).
            ("ui/primitives/controls.rs", "rgb(0x3b_42_52)"),
            ("ui/primitives/controls.rs", "rgb(0x2e_34_40)"),
        ],
    ),
];

/// The scale itself.
const EXEMPT: &[&str] = &[
    "ui/primitives/shape.rs",
    "ui/primitives/text.rs",
    "ui/primitives/tokens.rs",
    "ui/primitives/guard.rs",
];

/// Disk, process and network calls, each with the interface files that still make them.
/// The list only shrinks.
const IO_RULES: &[(&str, &[(&str, &str)])] = &[
    ("std::fs", &[]),
    ("tokio::fs", &[]),
    ("tokio::net", &[]),
    ("hyper", &[]),
    ("std::process", &[]),
    ("tempfile", &[]),
    ("std::env::home_dir", &[]),
    (".canonicalize(", &[]),
    (".is_dir(", &[]),
    (".is_file(", &[]),
    (".exists(", &[]),
    (".try_exists(", &[]),
    ("Path::canonicalize", &[]),
    ("Path::is_dir", &[]),
    ("Path::is_file", &[]),
    ("Path::exists", &[]),
    ("Path::try_exists", &[]),
];

fn sources(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            sources(&path, files);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
}

/// Lines under `src/ui` that match a rule outside its allowlist. With `production_only`, the
/// snapshot harness, test files and each file's tests (after its first `#[cfg(test)]`) are
/// skipped.
fn violations(
    rules: &[(&str, &[(&str, &str)])],
    exempt: &[&str],
    production_only: bool,
) -> Vec<String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ui");
    let mut files = Vec::new();
    sources(&root, &mut files);
    let mut violations = Vec::new();
    for path in files {
        let relative = path.strip_prefix(root.parent().unwrap()).unwrap();
        let relative = relative.to_string_lossy().replace('\\', "/");
        if exempt.contains(&relative.as_str())
            || production_only
                && (relative.starts_with("ui/snapshot/") || relative.ends_with("tests.rs"))
        {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let production = text
            .lines()
            .take_while(|line| !production_only || line.trim() != "#[cfg(test)]");
        for (number, line) in production.enumerate() {
            for (pattern, allowed) in rules {
                let permitted = allowed
                    .iter()
                    .any(|(file, fragment)| relative == *file && line.contains(fragment));
                if line.contains(pattern) && !permitted {
                    violations.push(format!(
                        "{relative}:{}: `{pattern}` in {}",
                        number + 1,
                        line.trim()
                    ));
                }
            }
        }
    }
    violations
}

#[test]
fn sizes_come_from_the_design_system() {
    let violations = violations(RULES, EXEMPT, false);
    assert!(
        violations.is_empty(),
        "sizes outside the design system (use `Type`, `glyph::`, `height::` or `Shape`, or \
         allowlist a justified case in primitives/guard.rs):\n{}",
        violations.join("\n")
    );
}

#[test]
fn the_interface_does_no_io() {
    let violations = violations(IO_RULES, &["ui/primitives/guard.rs"], true);
    assert!(
        violations.is_empty(),
        "disk, process or network work in the interface (move it to `gateway` or `platform`):\n{}",
        violations.join("\n")
    );
}
