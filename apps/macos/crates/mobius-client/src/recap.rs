//! What a chat said since it was last read, for its on-device "While you were away" recap.
//! Read only: the history pages come over the primary connection without selecting the
//! chat, so reading never starts it.

use mobius::SessionId;
use mobius::protocol::MessageAuthor;
use mobius_gateway::client::GatewayRequests;
use mobius_gateway::wire::{
    ClientMessage, HistoryCursor, ServerMessage, TranscriptItem, TranscriptItemContent,
    TranscriptPage,
};
use mobius_gateway::{Error, Result};
use std::fmt::Write as _;
use std::sync::Arc;

/// Pages read back looking for the last-read point before recapping what was found.
const MAX_PAGES: usize = 4;
/// Turns recapped when the chat was never read in this window.
const FALLBACK_TURNS: usize = 3;

/// A chat's unread part as plain text, and its last message for when no recap is written.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RecentChat {
    pub transcript: String,
    pub last_message: Option<String>,
}

/// Items of `session` recorded after `since` (gateway milliseconds), or its last few turns
/// when it has not been read. History it inherited from a chat it forked is not its own.
pub(super) async fn load(
    link: &GatewayRequests,
    session: &SessionId,
    since: Option<i64>,
) -> Result<RecentChat> {
    let mut items = Vec::new();
    let mut before = None;
    for _ in 0..MAX_PAGES {
        let (page, next) = page(link, session, before).await?;
        let reached = match since {
            Some(since) => page
                .items
                .first()
                .is_none_or(|item| item.recorded_at_ms <= since),
            None => turns(&page.items) + turns(&items) >= FALLBACK_TURNS,
        };
        items.splice(0..0, page.items);
        match next {
            Some(next) if !reached && next.session_id == *session => before = Some(next),
            _ => break,
        }
    }
    Ok(recent(&items, since))
}

async fn page(
    link: &GatewayRequests,
    session: &SessionId,
    before: Option<HistoryCursor>,
) -> Result<(TranscriptPage, Option<HistoryCursor>)> {
    let response = link
        .request(|request_id| ClientMessage::GetSessionHistory {
            request_id,
            session_id: session.share(),
            before,
        })
        .await?;
    match response {
        ServerMessage::SessionHistory {
            session_id,
            page,
            next,
            ..
        } if session_id == *session => Ok((page, next)),
        _ => Err(Error::Protocol(
            "Gateway returned a different chat's history.".into(),
        )),
    }
}

fn turns(items: &[Arc<TranscriptItem>]) -> usize {
    items.iter().filter(|item| item.starts_turn).count()
}

/// The unread part of `items` (oldest first) as a transcript for the model.
pub(super) fn recent(items: &[Arc<TranscriptItem>], since: Option<i64>) -> RecentChat {
    let start = match since {
        Some(since) => items.partition_point(|item| item.recorded_at_ms <= since),
        None => items
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, item)| item.starts_turn)
            .nth(FALLBACK_TURNS - 1)
            .map_or(0, |(index, _)| index),
    };
    let mut chat = RecentChat::default();
    for item in items.get(start..).unwrap_or_default() {
        let (speaker, text) = match &item.content {
            TranscriptItemContent::Message { message } if message.author == MessageAuthor::User => {
                ("Person", message.text.as_str())
            }
            TranscriptItemContent::Message { message } => ("Message", message.text.as_str()),
            TranscriptItemContent::Assistant { text, .. } => ("Assistant", text.as_str()),
            _ => continue,
        };
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        if let Err(error) = writeln!(chat.transcript, "{speaker}: {text}\n") {
            tracing::warn!(%error, "the recap transcript could not be written");
        }
        chat.last_message = Some(text.to_owned());
    }
    chat
}

#[cfg(test)]
mod tests {
    use super::recent;
    use mobius_gateway::wire::TranscriptItem;
    use std::sync::Arc;

    fn ask(at: i64, text: &str) -> Arc<TranscriptItem> {
        Arc::new(TranscriptItem {
            turn_id: Some(format!("turn-{at}").into()),
            recorded_at_ms: at,
            starts_turn: true,
            ..crate::fixture::user_message(format!("message:{at}"), text)
        })
    }

    #[test]
    fn recaps_only_what_came_after_the_last_read_point() {
        let chat = recent(&[ask(10, "old"), ask(30, "new")], Some(20));
        assert_eq!(chat.transcript, "Person: new\n\n");
        assert_eq!(chat.last_message.as_deref(), Some("new"));
    }

    #[test]
    fn an_unread_chat_without_a_read_point_recaps_its_last_turns() {
        let items: Vec<_> = (0..5).map(|n| ask(n * 10, &format!("ask {n}"))).collect();
        let chat = recent(&items, None);
        assert_eq!(
            chat.transcript,
            "Person: ask 2\n\nPerson: ask 3\n\nPerson: ask 4\n\n"
        );
    }
}
