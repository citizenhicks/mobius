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
use mobius_gateway::wire::{
    RecordedEvent, RenderedPreview, TranscriptItem, TranscriptItemContent, TranscriptItemState,
    TranscriptPage,
};

mod composer;
mod layout;
mod preview;
mod rendering;
mod support;
mod transcript;
