//! Transcript rows as SwiftUI draws them (TranscriptView.swift, TranscriptEventViews.swift):
//! user bubbles, incoming session messages, assistant prose, notices, Worked for rows,
//! event groups and file cards. The chat, previews and routines share these rows.

mod actions;
mod attribution;
mod collapsible;
mod disclosure;
mod issue;
mod links;
mod pending;
mod rows;
mod scroll;
mod turn_diff;
mod ui_document;
pub(in crate::ui) use disclosure::BlockLine;
pub(in crate::ui) use issue::{IssueAction, ProviderIssue};
pub(in crate::ui) use links::StreamingMarkdown;
pub(in crate::ui) use scroll::HISTORY;
pub(super) use scroll::TranscriptScroll;

pub(in crate::ui) use rows::row_motion_id;
