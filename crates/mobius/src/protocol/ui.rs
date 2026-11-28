//! Model-composed interface documents that each native client renders with its own views.

use serde::{Deserialize, Serialize};
use serde_json::Number;

/// One element of a document; sections are headings, not containers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiBlock {
    /// Heading that starts a group of following blocks.
    Section {
        /// The title.
        title: String,
    },
    /// Markdown prose.
    Text {
        /// The markdown.
        markdown: String,
    },
    /// Rows with optional thumbnail, detail and link.
    List {
        /// The items.
        items: Vec<UiListItem>,
    },
    /// Plain-text table.
    Table {
        /// Column headings.
        columns: Vec<String>,
        /// Rows, each with one cell per column.
        rows: Vec<Vec<String>>,
    },
    /// Key figures shown as tiles.
    Stats {
        /// The items.
        items: Vec<UiStat>,
    },
    /// Rich link preview.
    Link {
        /// HTTPS address opened on activation.
        url: String,
        /// The title.
        title: String,
        /// The description.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        /// HTTPS preview image.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        image_url: Option<String>,
    },
    /// Standalone HTTPS image.
    Image {
        /// The url.
        url: String,
        /// Accessible description.
        alt: String,
        /// The caption.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caption: Option<String>,
    },
    /// Bar, line, area or pie chart.
    Chart {
        /// The kind.
        kind: UiChartKind,
        /// The title.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        /// Category labels; every series has one value per label.
        labels: Vec<String>,
        /// The series.
        series: Vec<UiSeries>,
        /// Unit appended to values, such as `%` or `km`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        unit: Option<String>,
    },
}

/// One list row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiListItem {
    /// The title.
    pub title: String,
    /// The subtitle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    /// Short trailing value, such as a price or time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trailing: Option<String>,
    /// HTTPS thumbnail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_url: Option<String>,
    /// HTTPS address opened on activation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// One key figure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiStat {
    /// The label.
    pub label: String,
    /// Preformatted value.
    pub value: String,
    /// Secondary line, such as a change or comparison.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Direction that tints the detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trend: Option<UiTrend>,
}

/// Direction of a figure's change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiTrend {
    /// Selects the up case.
    Up,
    /// Selects the down case.
    Down,
    /// Selects the flat case.
    Flat,
}

/// Chart presentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiChartKind {
    /// Selects the bar case.
    Bar,
    /// Selects the line case.
    Line,
    /// Selects the area case.
    Area,
    /// Selects the pie case.
    Pie,
}

/// One named data series.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiSeries {
    /// The name.
    pub name: String,
    /// Finite values, one per chart label.
    pub values: Vec<Number>,
}
