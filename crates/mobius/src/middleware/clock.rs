//! The current time, told to the model sparingly: a short internal note before a request
//! when the context has none, or the last one is older than the interval or from another
//! day. Notes are ordinary saved input, so prompt caches and continuations keep their prefix;
//! compaction drops them and the next request tells the time again.

use chrono::{DateTime, FixedOffset, Local};
use serde_json::Value;
use std::sync::Arc;

use super::{Middleware, ModelContext};
use crate::backend::model::{ModelInput, internal_user_message, set_field};
use crate::protocol::{FrontendSettingValue, internal_message_kind};
use crate::{BoxFuture, Error, Result};

mod text {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Definition {
        pub(super) default_enabled: bool,
        pub(super) manifest_label: String,
        pub(super) manifest_description: String,
        pub(super) prompt_time: String,
        pub(super) time_format: String,
        #[serde(deserialize_with = "crate::middleware::manifest::deserialize_settings")]
        pub(super) settings: Vec<crate::middleware::manifest::MiddlewareSettingManifest>,
    }
    crate::embedded_config! { pub(super) static DEFINITION: Definition = include_str!("clock.toml"); }
}

super::manifest::middleware_manifest! {
    /// Configuration and presentation metadata for time reminders.
    "clock", text::DEFINITION, required: false, settings: &text::DEFINITION.settings
}

const KIND: &str = "clock";
/// When the note was written, as RFC 3339. The leading underscore keeps it out of provider
/// requests.
const AT_FIELD: &str = "_clock_at";

/// Tells the model the time before a request whenever its last note is stale.
#[derive(Debug, Clone, Copy)]
pub struct Clock {
    interval: chrono::Duration,
}

impl Clock {
    /// Reads the reminder interval from the Bot's settings, or its default.
    /// # Errors
    ///
    /// Returns an error when the interval is outside the supported range.
    pub fn from_settings<'a>(
        setting: impl Fn(&str) -> Option<&'a FrontendSettingValue>,
    ) -> Result<Self> {
        let id = "interval_minutes";
        let minutes = match setting(id) {
            None => super::manifest::integer_default(&text::DEFINITION.settings, id),
            Some(FrontendSettingValue::Integer(value)) => *value,
            Some(FrontendSettingValue::String(_)) => {
                return Err(Error::Invalid(format!(
                    "middleware setting `{}.{id}` must be integer",
                    MANIFEST.id
                )));
            }
        };
        let interval = chrono::Duration::try_minutes(minutes)
            .filter(|interval| *interval > chrono::Duration::zero())
            .ok_or_else(|| Error::Invalid(format!("invalid `{}.{id}` interval", MANIFEST.id)))?;
        Ok(Self { interval })
    }

    /// The note to add before a request at `now`, or `None` while the last one is fresh.
    fn note(self, input: ModelInput<'_>, now: DateTime<FixedOffset>) -> Option<Value> {
        let last = input
            .iter()
            .rev()
            .find(|item| internal_message_kind(item) == Some(KIND))
            .and_then(|item| item.get(AT_FIELD)?.as_str())
            .and_then(|at| DateTime::parse_from_rfc3339(at).ok());
        if last.is_some_and(|last| {
            last <= now
                && now - last < self.interval
                && last.with_timezone(now.offset()).date_naive() == now.date_naive()
        }) {
            return None;
        }
        let time = now.format(&text::DEFINITION.time_format).to_string();
        let mut item =
            internal_user_message(KIND, &text::DEFINITION.prompt_time.replace("{time}", &time));
        set_field(&mut item, AT_FIELD, Value::String(now.to_rfc3339()));
        Some(item)
    }
}

impl Middleware for Clock {
    fn name(&self) -> &'static str {
        MANIFEST.id
    }

    fn pre_model<'a>(&'a self, context: &'a mut ModelContext<'_>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if let Some(item) = self.note(context.input(), Local::now().fixed_offset()) {
                context.append_model_input(item);
            }
            Ok(())
        })
    }

    fn prepare_compacted_input(&self, _original: ModelInput<'_>, compacted: &mut Vec<Arc<Value>>) {
        compacted.retain(|item| internal_message_kind(item) != Some(KIND));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339(text).expect("time")
    }

    fn clock(minutes: i64) -> Clock {
        let value = FrontendSettingValue::Integer(minutes);
        Clock::from_settings(|_| Some(&value)).expect("clock")
    }

    #[test]
    fn a_note_is_added_when_none_is_fresh() {
        let clock = clock(30);
        let first = clock
            .note(
                ModelInput::from(&[] as &[Value]),
                at("2026-11-21T14:03:00+00:00"),
            )
            .expect("an empty context is told the time");
        assert_eq!(
            first["content"][0]["text"],
            "Current time: 2026-11-21 14:03 +00:00 (Saturday)"
        );
        assert!(first.get(AT_FIELD).is_some());
        let input = [first];
        let input = ModelInput::from(&input[..]);
        assert!(
            clock.note(input, at("2026-11-21T14:20:00+00:00")).is_none(),
            "a note within the interval is fresh"
        );
        assert!(
            clock.note(input, at("2026-11-21T14:33:00+00:00")).is_some(),
            "the interval has passed"
        );
        let late = [clock
            .note(input, at("2026-11-21T23:50:00+00:00"))
            .expect("the interval has passed")];
        assert!(
            clock
                .note(ModelInput::from(&late[..]), at("2026-11-22T00:01:00+00:00"))
                .is_some(),
            "a new day is told even within the interval"
        );
        let ordinary =
            Arc::new(serde_json::json!({"type":"message","role":"user","content":"Keep this"}));
        let mut compacted = late.into_iter().map(Arc::new).collect::<Vec<_>>();
        compacted.push(Arc::clone(&ordinary));
        clock.prepare_compacted_input(input, &mut compacted);
        assert_eq!(compacted, [ordinary]);
        assert!(
            clock
                .note(
                    ModelInput::from(&compacted[..]),
                    at("2026-11-22T00:01:00+00:00")
                )
                .is_some()
        );
    }

    #[test]
    fn the_interval_must_be_an_integer() {
        let value = FrontendSettingValue::String("soon".into());
        assert!(Clock::from_settings(|_| Some(&value)).is_err());
        let value = FrontendSettingValue::Integer(i64::MAX);
        assert!(
            Clock::from_settings(|_| Some(&value)).is_err(),
            "overflow is a configuration error"
        );
        let default = Clock::from_settings(|_| None).expect("default");
        assert_eq!(default.interval, chrono::Duration::minutes(30));
    }
}
