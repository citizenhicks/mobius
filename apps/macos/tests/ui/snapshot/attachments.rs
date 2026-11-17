//! Attachment scenarios: composer tiles in each upload state and a sent message's images,
//! their thumbnails drawn from generated images by the native pipeline. Nothing uploads.

use super::Scenario;
use super::capture::{open_chat, session_id};
use super::probes::undo_after_capture;
use crate::numeric::{Saturating, ToFloat};
use crate::ui::chat::Scene;

use crate::ui::Workspace;
use crate::ui::runtime::ThumbnailKey;

use gpui::Context;

use mobius::protocol::{
    ContentPart, FrontendBlock, FrontendBlockFormat, FrontendBlockRole, FrontendBlockState,
    FrontendBlockUpdate, FrontendTone, MessageAuthor, MessageDelivery, MessageEvent,
    SessionFileReference, ToolContent,
};
use mobius_gateway::wire::{
    TranscriptChild, TranscriptItem, TranscriptItemContent, TranscriptItemState, TranscriptOp,
    TranscriptPosition, TranscriptRow, TranscriptRowKind,
};
use std::sync::Arc;

pub(super) const SCENARIOS: &[Scenario] = &[
    ("chat-attachments", |this, window, cx| {
        open_chat(this, window, cx);
        window.focus(&this.focus, cx);
        Workspace::when_chat_ready(window, cx, |this, _, cx| {
            this.apply_fixture(Scene::Transcript, cx);
            attachments(this, cx);
            undo_after_capture(Workspace::clear_fixture_attachments);
        });
    }),
    ("chat-transcript-files", |this, window, cx| {
        open_chat(this, window, cx);
        window.focus(&this.focus, cx);
        Workspace::when_chat_ready(window, cx, |this, _, cx| {
            this.apply_fixture(Scene::Running, cx);
            transcript_files(this, cx);
        });
    }),
];

fn file(id: &str, name: &str, media_type: &str, size: u64) -> SessionFileReference {
    SessionFileReference {
        id: id.into(),
        name: name.into(),
        media_type: media_type.into(),
        size,
    }
}

/// Writes `name` into `dir`: a smooth two-colour gradient, so a thumbnail's crop and
/// orientation read at a glance.
fn picture(dir: &std::path::Path, name: &str, width: u32, height: u32) {
    let path = dir.join(name);
    let image = image::RgbImage::from_fn(width, height, |x, y| {
        let (u, v) = (x.to_f32() / width.to_f32(), y.to_f32() / height.to_f32());
        image::Rgb([
            (40. + 180. * u).saturating_u8(),
            (90. + 120. * v).saturating_u8(),
            (200. - 120. * u * v).saturating_u8(),
        ])
    });
    if let Err(error) = image.save(&path) {
        eprintln!("snapshot {}: {error}", path.display());
    }
}

fn attachments(this: &mut Workspace, cx: &mut Context<Workspace>) {
    let Some(dir) = this.downloads.as_ref().map(|dir| dir.path().to_owned()) else {
        return;
    };
    let Some(session) = session_id(this) else {
        return;
    };
    picture(&dir, "landscape.png", 1600, 1000);
    picture(&dir, "portrait.png", 900, 1400);
    let sent = [
        file("fixture-sent-1", "harbour.png", "image/png", 2_400_000),
        file("fixture-sent-2", "tower.png", "image/png", 1_800_000),
        file("fixture-sent-3", "brief.pdf", "application/pdf", 88_000),
    ];
    for (file, source) in sent.iter().zip(["landscape.png", "portrait.png"]) {
        let key = ThumbnailKey::file(&session, &file.id);
        this.draw_thumbnail(key, dir.join(source), false, cx);
    }
    let message = MessageEvent {
        author: MessageAuthor::User,
        delivery: MessageDelivery::Turn,
        text: "Here are the photos and the brief.".into(),
        attachments: sent.into(),
        reply: None,
        message_target: None,
    };
    this.client.state.transcript.apply(entry(
        "message:fixture-attachments",
        10_000,
        TranscriptItemContent::Message { message },
    ));
    this.fixture_attachments(
        vec![
            (
                "harbour.png",
                Some(file("fixture-up-1", "harbour.png", "image/png", 2_400_000)),
                None,
                Some(dir.join("landscape.png")),
            ),
            ("tower.png", None, None, Some(dir.join("portrait.png"))),
            (
                "brief.pdf",
                Some(file("fixture-up-3", "brief.pdf", "application/pdf", 88_000)),
                None,
                None,
            ),
            (
                "huge.mov",
                None,
                Some("This file exceeds the attachment limit."),
                Some(dir.join("landscape.png")),
            ),
        ],
        cx,
    );
    this.gateway_revision += 1;
    cx.notify();
}

/// One item shown on its own row at `sequence`.
fn entry(id: &str, sequence: u64, content: TranscriptItemContent) -> Vec<TranscriptOp> {
    let id: Arc<str> = id.into();
    let item = TranscriptItem {
        id: Arc::clone(&id),
        turn_id: None,
        recorded_at_ms: 1_790_000_100_000 + sequence.cast_signed(),
        state: TranscriptItemState::Complete,
        terminal: false,
        starts_turn: false,
        resumed: false,
        inherited: false,
        target: None,
        content,
    };
    let row = TranscriptRow {
        id: Arc::clone(&id),
        position: TranscriptPosition {
            depth: 0,
            sequence,
            index: 0,
        },
        items: vec![TranscriptChild::Item(id)],
        kind: TranscriptRowKind::Entry { turn_changes: None },
    };
    vec![
        TranscriptOp::Item {
            item: Arc::new(item),
        },
        TranscriptOp::Row { row: Arc::new(row) },
    ]
}

fn block(id: &str, sequence: u64, block: FrontendBlock) -> Vec<TranscriptOp> {
    let content = TranscriptItemContent::Block {
        capability: "tools".into(),
        block,
        group_title: None,
    };
    entry(&format!("block:tools:{id}"), sequence, content)
}

fn files_block(
    id: &str,
    title: &str,
    format: FrontendBlockFormat,
    files: impl IntoIterator<Item = SessionFileReference>,
) -> FrontendBlock {
    FrontendBlock {
        id: Some(id.into()),
        group: None,
        update: FrontendBlockUpdate::Replace,
        state: FrontendBlockState::Complete,
        role: FrontendBlockRole::Artifact,
        title: title.into(),
        text: String::new(),
        symbol: None,
        links: Vec::new(),
        files: Vec::new(),
        content: ToolContent(
            files
                .into_iter()
                .map(|file| ContentPart::File { file })
                .collect(),
        ),
        format,
        tone: FrontendTone::Neutral,
        ui: Vec::new(),
    }
}

/// A sent message with five files, then the Bot's generated image and artifacts: tiles of
/// every shape, on both sides of the transcript.
fn transcript_files(this: &mut Workspace, cx: &mut Context<Workspace>) {
    let Some(dir) = this.downloads.as_ref().map(|dir| dir.path().to_owned()) else {
        return;
    };
    let Some(session) = session_id(this) else {
        return;
    };
    let (landscape, portrait, square) = (
        "files-landscape.png",
        "files-portrait.png",
        "files-square.png",
    );
    picture(&dir, landscape, 1600, 1000);
    picture(&dir, portrait, 900, 1400);
    picture(&dir, square, 800, 800);
    let sent = [
        file("fixture-files-1", "harbour.png", "image/png", 2_400_000),
        file("fixture-files-2", "tower.png", "image/png", 1_800_000),
        file("fixture-files-3", "brief.pdf", "application/pdf", 88_000),
        file("fixture-files-4", "notes.txt", "text/plain", 3_100),
        file("fixture-files-5", "logo.png", "image/png", 120_000),
    ];
    let made = [
        file("fixture-files-6", "pills-a.png", "image/png", 640_000),
        file("fixture-files-7", "pills-b.png", "image/png", 610_000),
        file("fixture-files-8", "layout.md", "text/markdown", 4_200),
    ];
    let generated = file("fixture-files-9", "hero.png", "image/png", 1_200_000);
    let pictures = [
        (&sent[0], landscape),
        (&sent[1], portrait),
        (&sent[4], square),
        (&made[0], landscape),
        (&made[1], portrait),
        (&generated, landscape),
    ];
    for (file, source) in pictures {
        let key = ThumbnailKey::file(&session, &file.id);
        this.draw_thumbnail(key, dir.join(source), false, cx);
    }
    let message = MessageEvent {
        author: MessageAuthor::User,
        delivery: MessageDelivery::Queue,
        text: "Here are the photos, the brief and my notes.".into(),
        attachments: sent.into(),
        reply: None,
        message_target: None,
    };
    let transcript = &mut this.client.state.transcript;
    transcript.apply(entry(
        "message:fixture-files-10001",
        10_001,
        TranscriptItemContent::Message { message },
    ));
    let hero = files_block(
        "fixture-hero",
        "Generated hero.png",
        FrontendBlockFormat::Image,
        [generated],
    );
    transcript.apply(block("fixture-hero", 10_002, hero));
    let made = files_block(
        "fixture-made",
        "Sent 3 files",
        FrontendBlockFormat::PlainText,
        made,
    );
    transcript.apply(block("fixture-made", 10_003, made));
    this.gateway_revision += 1;
    cx.notify();
}
