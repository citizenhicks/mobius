//! Auto routing: a new chat asks the configured decision model to pick each target it left
//! on Auto from its first message, and keeps the client's default for any target it cannot.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use mobius::backend::model::decisions::{DecisionQuestion, DecisionRequest, decide};

use super::catalog::refresh_catalog;
use super::rejection::{Rejection, internal, invalid_config};
use super::{GatewayHost, HostHandle, finish_publication};
use crate::config::auto_routing::{self, Settings, Values};
use crate::harness::mobius::assembly::{RouteValue, instantiate_routes};
use crate::harness::mobius::provider_catalog::{configured_model_choices, decision_route};
use crate::wire::{BotRecord, ReadyPayload, RouteTarget, SessionRecord};

/// What a client asks for when it starts a chat.
pub(crate) struct NewChat {
    /// The workspace, and Auto's fallback.
    pub(crate) workspace: PathBuf,
    /// The Bot, and Auto's fallback.
    pub(crate) bot_id: Arc<str>,
    /// The targets the decision model may pick.
    pub(crate) auto: BTreeSet<RouteTarget>,
    /// The first message, which Auto routes on.
    pub(crate) prompt: Option<String>,
}

struct RouteOption {
    criteria: String,
    bot_id: Option<Arc<str>>,
}

type Options = BTreeMap<String, RouteOption>;

/// Everything one decision call needs, taken while the state is locked.
struct Routing {
    model: String,
    settings: Settings,
    questions: BTreeMap<RouteTarget, Options>,
}

impl GatewayHost {
    /// Starts a chat, first routing the targets left on Auto.
    pub(crate) async fn create_routed_session(
        &self,
        mut chat: NewChat,
    ) -> std::result::Result<HostHandle, Rejection> {
        if let Some(prompt) = chat.prompt.take().filter(|prompt| {
            !chat.auto.is_empty()
                && prompt.len() <= mobius::protocol::MAX_MESSAGE_BYTES
                && !prompt.trim().is_empty()
        }) {
            let mutation = self.begin_mutation().await?;
            if let Err(error) = self.route(&mut chat, &prompt).await {
                tracing::warn!(%error, "auto routing kept the defaults");
            }
            drop(mutation);
        }
        self.create_session(&chat.workspace, &chat.bot_id).await
    }

    /// Saves how chats left on Auto are routed.
    #[expect(
        clippy::significant_drop_tightening,
        reason = "the state guard is handed to publish_ready_after_unlock, which publishes the saved routing under it"
    )]
    pub(crate) async fn configure_auto_routing(
        &self,
        values: Values,
    ) -> std::result::Result<Arc<ReadyPayload>, Rejection> {
        let _mutation = self.begin_mutation().await?;
        let mut state = self.state.lock().await;
        let (publication, model_changed) = {
            let mut current = state.config()?;
            let choices = configured_model_choices(&current, &state.store, &state.credentials)
                .map_err(internal)?;
            auto_routing::validate(&values, Some(choices.catalogs())).map_err(invalid_config)?;
            let next = current
                .replacing_auto_routing(values)
                .map_err(invalid_config)?;
            let publication =
                crate::publication::Outcome::applied(state.store.save(&next)).map_err(internal)?;
            let model_changed = auto_routing::model(current.auto_routing())
                != auto_routing::model(next.auto_routing());
            *current = next.into_inner();
            (publication, model_changed)
        };
        if model_changed {
            state.decision_route = None;
        }
        let follow_up = self.publish_ready_after_unlock(state).await;
        finish_publication(publication.confirm(), follow_up)
    }

    /// Returns the value picked for each target the model answered with enough confidence.
    async fn route(&self, chat: &mut NewChat, prompt: &str) -> crate::Result<()> {
        let Some(mut routing) = self.routing(&chat.auto).await? else {
            return Ok(());
        };
        let Some(route) = self.decision_route(&routing.model).await? else {
            return Ok(());
        };
        let questions = routing
            .questions
            .iter()
            .filter_map(|(target, options)| {
                Some((
                    target.key(),
                    DecisionQuestion::new(
                        routing.settings.questions.get(target)?,
                        options
                            .iter()
                            .map(|(name, option)| (name.as_str(), option.criteria.as_str()))
                            .collect(),
                    ),
                ))
            })
            .collect();
        let request = DecisionRequest {
            state: prompt,
            questions: &questions,
        };
        let mut answers = tokio::time::timeout(
            routing.settings.timeout,
            decide(&*route.model, &route.lifetime, request),
        )
        .await
        .map_err(|_elapsed| crate::Error::Invalid("the decision model timed out".into()))??;
        for target in RouteTarget::ALL {
            let Some(answer) = answers
                .remove(target.key())
                .filter(|answer| answer.confidence >= routing.settings.min_confidence)
            else {
                continue;
            };
            match target {
                RouteTarget::Bot => {
                    if let Some(id) = routing
                        .questions
                        .get_mut(&target)
                        .and_then(|options| options.remove(&answer.choice))
                        .and_then(|option| option.bot_id)
                    {
                        chat.bot_id = id;
                    }
                }
                RouteTarget::Workspace => chat.workspace = answer.choice.into(),
            }
        }
        Ok(())
    }

    /// Lazily shares the selected adapter and its HTTP pool until its provider changes.
    async fn decision_route(&self, model: &str) -> crate::Result<Option<Arc<RouteValue>>> {
        let mut state = self.state.lock().await;
        let route = {
            let config = crate::config::lock(&state.config)?;
            if auto_routing::model(config.auto_routing()) != Some(model) {
                return Ok(None);
            }
            if let Some(route) = &state.decision_route
                && route.choice.route.as_ref() == model
            {
                return Ok(Some(Arc::clone(route)));
            }
            let Some(selected) = decision_route(&config.configured_providers, model)? else {
                return Ok(None);
            };
            let route = instantiate_routes(
                vec![selected],
                &state.store,
                &state.credentials,
                &config.model_transport,
            )?
            .pop()
            .ok_or_else(|| crate::Error::Internal("decision route was not built".into()))?;
            drop(config);
            Arc::new(route)
        };
        state.decision_route = Some(Arc::clone(&route));
        drop(state);
        Ok(Some(route))
    }

    /// Gathers the questions, or `None` when Auto has no usable decision model or nothing
    /// has more than one option.
    async fn routing(&self, auto: &BTreeSet<RouteTarget>) -> crate::Result<Option<Routing>> {
        let (mut routing, bots, handles) = {
            let state = self.state.lock().await;
            let routing = {
                let config = crate::config::lock(&state.config)?;
                let Some(model) = auto_routing::model(config.auto_routing()) else {
                    return Ok(None);
                };
                Routing {
                    model: model.to_owned(),
                    settings: Settings::read(config.auto_routing())?,
                    questions: BTreeMap::new(),
                }
            };
            let bots = if auto.contains(&RouteTarget::Bot)
                && routing.settings.questions.contains_key(&RouteTarget::Bot)
            {
                state.bots.bots()?
            } else {
                Vec::new()
            };
            (routing, bots, state.catalog_handles())
        };
        let limit = routing.settings.max_options;
        for target in auto {
            if !routing.settings.questions.contains_key(target) {
                continue;
            }
            let options = match target {
                RouteTarget::Bot => bot_options(&bots, limit),
                RouteTarget::Workspace => {
                    let mut catalog = handles.1.lock().await;
                    if let Some(snapshot) = &catalog.snapshot {
                        workspace_options(&snapshot.sessions, limit, routing.settings.recent_titles)
                    } else {
                        let sessions = refresh_catalog(&handles.0, &mut catalog).await?;
                        drop(catalog);
                        workspace_options(&sessions, limit, routing.settings.recent_titles)
                    }
                }
            };
            if options.len() > 1 {
                routing.questions.insert(*target, options);
            }
        }
        Ok((!routing.questions.is_empty()).then_some(routing))
    }
}

/// Each Bot by handle, described by its name and description.
fn bot_options(bots: &[BotRecord], limit: usize) -> Options {
    bots.iter()
        .take(limit)
        .map(|bot| {
            let criteria = if bot.description.trim().is_empty() {
                bot.name.to_string()
            } else {
                format!("{}: {}", bot.name, bot.description)
            };
            (
                bot.handle.to_string(),
                RouteOption {
                    criteria,
                    bot_id: Some(Arc::clone(&bot.id)),
                },
            )
        })
        .collect()
}

/// One chat's last update, workspace and title, for chats that have a workspace.
struct ChatWorkspace<'a> {
    updated_at: i64,
    path: &'a str,
    title: Option<&'a str>,
}

fn chat_workspace(session: &SessionRecord) -> Option<ChatWorkspace<'_>> {
    let path = session
        .session_context
        .workspace_label
        .as_deref()
        .filter(|path| !path.is_empty())?;
    Some(ChatWorkspace {
        updated_at: session.updated_at,
        path,
        title: session
            .title
            .as_deref()
            .or(session.first_user_message.as_deref()),
    })
}

/// Recent workspaces, newest first, each described by its path and latest chat titles.
fn workspace_options(sessions: &[SessionRecord], limit: usize, titles: usize) -> Options {
    workspace_choices(
        sessions.iter().filter_map(chat_workspace).collect(),
        limit,
        titles,
    )
}

fn workspace_choices(mut chats: Vec<ChatWorkspace<'_>>, limit: usize, titles: usize) -> Options {
    chats.sort_unstable_by_key(|chat| (std::cmp::Reverse(chat.updated_at), chat.path, chat.title));
    let mut recent = BTreeMap::<&str, Vec<&str>>::new();
    for ChatWorkspace { path, title, .. } in chats {
        if recent.len() == limit && !recent.contains_key(path) {
            continue;
        }
        let entry = recent.entry(path).or_default();
        if let Some(title) = title
            && entry.len() < titles
        {
            entry.push(title);
        }
    }
    recent
        .into_iter()
        .map(|(path, chats)| {
            let criteria = if chats.is_empty() {
                path.into()
            } else {
                format!("{path} — recent chats: {}", chats.join("; "))
            };
            (
                path.into(),
                RouteOption {
                    criteria,
                    bot_id: None,
                },
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat<'a>(path: &'a str, title: &'a str, updated_at: i64) -> ChatWorkspace<'a> {
        ChatWorkspace {
            updated_at,
            path,
            title: Some(title),
        }
    }

    #[tokio::test]
    async fn decision_connection_is_lazy_and_tracks_provider_and_credential_changes() {
        use crate::config::{ProviderRegistration, test_provider};
        use mobius::backend::model::provider::BaseUrl;
        use mobius::protocol::FrontendSettingValue;

        let crate::host::tests::TestGateway {
            root,
            workspace,
            gateway,
        } = crate::host::tests::test_gateway().await;
        let endpoint = BaseUrl::new("http://127.0.0.1:9/v1").unwrap();
        let register = |ids: &[&str]| ProviderRegistration {
            selection: crate::wire::ProviderConfig {
                base_url: Some(endpoint.share()),
                ..test_provider("test", "openrouter", "test/chat")
            },
            label: None,
            tint: None,
            models: Vec::new(),
            image_model_ids: None,
            decision_model_ids: Some(ids.iter().map(|id| (*id).to_owned()).collect()),
            operator_managed: false,
        };
        let rotate = |key: &str| {
            gateway.set_credential(
                "test",
                "openrouter",
                key.to_owned(),
                Some(endpoint.share()),
                None,
            )
        };
        rotate("fixture-first").await.unwrap();
        gateway
            .register_provider(true, register(&["test/decide"]), false, false)
            .await
            .unwrap();
        let model = "test::test/decide::default";
        gateway
            .configure_auto_routing(Values::from([(
                "model".into(),
                FrontendSettingValue::String(model.into()),
            )]))
            .await
            .unwrap();
        let mut chat = NewChat {
            workspace,
            bot_id: "default".into(),
            auto: RouteTarget::ALL.into_iter().collect(),
            prompt: None,
        };
        gateway.route(&mut chat, "one choice").await.unwrap();
        assert!(gateway.state.lock().await.decision_route.is_none());
        let first = gateway.decision_route(model).await.unwrap().unwrap();
        assert!(Arc::ptr_eq(
            &first,
            &gateway.decision_route(model).await.unwrap().unwrap()
        ));
        rotate("fixture-next").await.unwrap();
        let next = gateway.decision_route(model).await.unwrap().unwrap();
        assert!(!Arc::ptr_eq(&first, &next));
        gateway
            .register_provider(true, register(&[]), false, false)
            .await
            .unwrap();
        assert!(gateway.decision_route(model).await.unwrap().is_none());
        drop(gateway);
        drop(root);
    }

    #[test]
    fn workspaces_are_newest_first_with_their_recent_titles() {
        let sessions = vec![
            chat("/a", "old a", 1),
            chat("/b", "b", 2),
            chat("/a", "new a", 3),
            chat("/c", "c", 0),
        ];
        let options = workspace_choices(sessions, 2, 2);
        assert_eq!(options.len(), 2, "the oldest workspace is past the limit");
        assert_eq!(options["/a"].criteria, "/a — recent chats: new a; old a");
        assert_eq!(options["/b"].criteria, "/b — recent chats: b");
        let options = workspace_choices(
            vec![
                chat("/a", "new", 3),
                chat("/a", "old", 2),
                chat("/b", "b", 1),
            ],
            2,
            0,
        );
        assert_eq!(
            options.keys().map(String::as_str).collect::<Vec<_>>(),
            ["/a", "/b"]
        );
    }

    #[test]
    fn targets_round_trip_through_their_question_keys() {
        for target in RouteTarget::ALL {
            let key = target.key();
            assert_eq!(
                serde_json::from_value::<RouteTarget>(key.into()).ok(),
                Some(target)
            );
        }
    }
}
