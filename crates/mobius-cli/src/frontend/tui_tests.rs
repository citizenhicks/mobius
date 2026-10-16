use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::*;
use crate::frontend::catalog::UiCatalog;
use crate::frontend::theme::{Role, current};
use mobius::protocol::{
    Event, FrontendBlock, FrontendBlockRole, FrontendPreviewUpdate, FrontendSlot, FrontendSymbol,
    FrontendTone, FrontendWidget, MessageAuthor, MessageDelivery, MessageEvent, MessageSubmission,
    ModelStepContent, ModelStepContentPhase, RenderedBlock, ReviewDecision,
};
use mobius_gateway::wire::{RecordedEvent, RenderedEvent, RenderedPreview};

#[path = "tui_tests/composer.rs"]
mod composer;
#[path = "tui_tests/layout.rs"]
mod layout;
#[path = "tui_tests/preview.rs"]
mod preview;
#[path = "tui_tests/rendering.rs"]
mod rendering;
#[path = "tui_tests/support.rs"]
mod support;
#[path = "tui_tests/transcript.rs"]
mod transcript;
