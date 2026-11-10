//! Agent settings for one Bot or for the agent defaults: one draft per scope, saved explicitly
//! (AgentSettingsView.swift). The same body serves the settings pages and the chat side tab.

use super::*;
use gpui_component::input::{InputEvent, InputState, TextareaState};
use mobius::protocol::{FrontendSettingKind, FrontendSettingValue};
use mobius_gateway::wire::{
    AgentComposition, BotShape, HarnessConfig, ProviderTint, ReadyPayload, SessionActivityState,
};

/// Whose settings a draft edits.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::ui) enum AgentScope {
    Defaults,
    Bot(Arc<str>),
}

impl AgentScope {
    pub(super) fn key(&self) -> String {
        match self {
            Self::Defaults => "defaults".into(),
            Self::Bot(id) => format!("bot:{id}"),
        }
    }
}

/// The saved values a draft starts from.
#[derive(Clone, PartialEq)]
pub(super) struct Saved {
    pub(super) revision: u64,
    pub(super) config: HarnessConfig,
    pub(super) identity: Option<(Arc<str>, Arc<str>, ProviderTint)>,
    /// The Bot's face silhouette, edited with its colour.
    pub(super) shape: BotShape,
}

/// Unsaved edits of one scope; survives switching between its sections.
pub(super) struct AgentDraft {
    pub(super) base: Saved,
    pub(super) config: HarnessConfig,
    pub(super) tint: ProviderTint,
    pub(super) shape: BotShape,
    pub(super) name: Entity<InputState>,
    pub(super) description: Entity<TextareaState>,
    pub(super) prompt: Entity<TextareaState>,
    pub(super) steps: Entity<InputState>,
    /// Integer capability settings by feature and setting ID, with the initial value.
    pub(super) numbers: BTreeMap<String, BTreeMap<String, (Entity<InputState>, i64)>>,
    pub(super) saving: Option<Arc<str>>,
    pub(super) rejected: Option<Arc<str>>,
    _subscriptions: Vec<Subscription>,
}

fn number_input(value: i64, label: &str, window: &mut Window, cx: &mut App) -> Entity<InputState> {
    field(&value.to_string(), label, window, cx)
}

impl AgentDraft {
    fn new(
        saved: Saved,
        gateway: &ReadyPayload,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Self {
        let (name, description, tint) = saved
            .identity
            .as_ref()
            .map_or_else(Default::default, |(name, description, tint)| {
                (Arc::clone(name), Arc::clone(description), *tint)
            });
        let area = |value: Arc<str>, min, max, window: &mut Window, cx: &mut Context<Workspace>| {
            let input = cx.new(|cx| TextareaState::new(window, cx).auto_grow(min, max));
            input.update(cx, |input, cx| input.set_value(value, window, cx));
            input
        };
        let description = area(description, 3, 6, window, cx);
        let composition = saved.config.mobius().map(Arc::as_ref);
        let prompt = cx.new(|cx| TextareaState::new(window, cx));
        prompt.update(cx, |input, cx| {
            let text = composition.map_or("", |config| config.system_prompt.as_str());
            input.set_value(text, window, cx);
        });
        let name = field(&name, "Bot name", window, cx);
        let steps = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Maximum model steps")
                .step(100.)
                .min(1.)
        });
        let mut numbers = BTreeMap::new();
        if let Some(config) = composition {
            steps.update(cx, |input, cx| {
                input.set_value(config.max_model_steps.to_string(), window, cx);
            });
            for feature in &gateway.agent_catalog.middleware_features {
                for setting in &feature.settings {
                    if let FrontendSettingKind::Integer { min, .. } = setting.kind {
                        let value = match config.middleware.setting(&feature.id, &setting.id) {
                            Some(FrontendSettingValue::Integer(value)) => *value,
                            _ => min,
                        };
                        numbers
                            .entry(feature.id.clone()) // rules-allow: the draft outlives the gateway payload, so its keys own the feature ID
                            .or_insert_with(BTreeMap::new)
                            .insert(
                                setting.id.clone(), // rules-allow: the draft outlives the gateway payload, so its keys own the setting ID
                                (number_input(value, &setting.label, window, cx), value),
                            );
                    }
                }
            }
        }
        let notify = |cx: &mut Context<Workspace>, input: &Entity<InputState>| {
            cx.subscribe(input, |_, _, _: &InputEvent, cx| cx.notify())
        };
        let mut subscriptions = vec![
            notify(cx, &name),
            notify(cx, &steps),
            cx.subscribe(&description, |_, _, _: &InputEvent, cx| cx.notify()),
            cx.subscribe(&prompt, |_, _, _: &InputEvent, cx| cx.notify()),
        ];
        subscriptions.extend(
            numbers
                .values()
                .flat_map(|settings| settings.values())
                .map(|(input, _)| notify(cx, input)),
        );
        Self {
            config: saved.config.clone(), // rules-allow: the draft edits its own copy while `base` keeps the saved values (a möbius composition is shared until first edited)
            tint,
            shape: saved.shape,
            base: saved,
            name,
            description,
            prompt,
            steps,
            numbers,
            saving: None,
            rejected: None,
            _subscriptions: subscriptions,
        }
    }

    /// The möbius composition, for a möbius Bot or the agent defaults.
    pub(super) fn mobius(&self) -> Option<&AgentComposition> {
        self.config.mobius().map(Arc::as_ref)
    }

    /// The möbius composition to edit; the first edit copies it away from the saved one.
    pub(super) fn mobius_mut(&mut self) -> Option<&mut AgentComposition> {
        match &mut self.config {
            HarnessConfig::Mobius(config) => Some(Arc::make_mut(config)),
            HarnessConfig::Codex(_) | HarnessConfig::Claude(_) => None,
        }
    }

    /// The draft as it would be saved.
    pub(super) fn composed(&self, cx: &App) -> Saved {
        let mut config = self.config.clone(); // rules-allow: composing builds a new value and leaves the draft as it is
        if let HarnessConfig::Mobius(composition) = &mut config {
            let config = Arc::make_mut(composition);
            config.system_prompt = self.prompt.read(cx).value().to_string();
            if let Ok(steps) = self.steps.read(cx).value().trim().parse::<u64>() {
                config.max_model_steps = steps;
            }
            for (feature, settings) in &self.numbers {
                for (setting, (input, initial)) in settings {
                    if let Ok(value) = input.read(cx).value().trim().parse::<i64>()
                        && value != *initial
                    {
                        let value = Some(FrontendSettingValue::Integer(value));
                        config.middleware.set_setting(feature, setting, value);
                    }
                }
            }
        }
        let identity = self.base.identity.as_ref().map(|_| {
            (
                Arc::from(self.name.read(cx).value().trim()),
                Arc::from(self.description.read(cx).value().trim()),
                self.tint,
            )
        });
        Saved {
            revision: self.base.revision,
            config,
            identity,
            shape: self.shape,
        }
    }

    pub(super) fn is_dirty(&self, cx: &App) -> bool {
        self.composed(cx) != self.base
    }

    /// Adopts a model the picker saved while this draft has other unsaved edits, when
    /// that is all that changed.
    fn rebase_model(&mut self, saved: Saved, gateway: &ReadyPayload) {
        if self.base.identity != saved.identity || self.base.shape != saved.shape {
            return;
        }
        let adopted = match (&mut self.config, &self.base.config, &saved.config) {
            (
                HarnessConfig::Mobius(draft),
                HarnessConfig::Mobius(old),
                HarnessConfig::Mobius(new),
            ) => rebase_composition(Arc::make_mut(draft), old, new, gateway),
            _ => false,
        };
        if adopted {
            self.base = saved;
        }
    }

    pub(super) fn finish_save(&mut self, rejection: Option<Arc<str>>) {
        self.saving = None;
        self.rejected = rejection;
    }
}

/// Puts the saved model on `draft` when nothing but the model moved since `old`.
fn rebase_composition(
    draft: &mut AgentComposition,
    old: &AgentComposition,
    new: &AgentComposition,
    gateway: &ReadyPayload,
) -> bool {
    let AgentComposition {
        provider: _,
        middleware: _,
        extensions,
        system_prompt,
        max_model_steps,
    } = new;
    let only_model_moved = old.extensions == *extensions
        && old.system_prompt == *system_prompt
        && old.max_model_steps == *max_model_steps;
    if !only_model_moved || draft.provider != old.provider {
        return false;
    }
    if draft.middleware == old.middleware {
        draft.middleware.clone_from(&new.middleware);
    }
    draft.provider.clone_from(&new.provider);
    draft
        .middleware
        .reconcile(&gateway.agent_catalog.middleware_features);
    true
}

impl Workspace {
    pub(super) fn saved_revision(&self, scope: &AgentScope) -> Option<u64> {
        match scope {
            AgentScope::Defaults => self
                .client
                .state
                .gateway
                .as_ref()?
                .agent_catalog
                .bot_defaults
                .as_ref()
                .map(|defaults| defaults.revision),
            AgentScope::Bot(id) => self.client.state.bot(id).map(|bot| bot.config.revision),
        }
    }

    /// Obsolete clean editors retain no user work; unsaved/rejected/saving drafts stay intact.
    pub(super) fn prune_bot_drafts(&mut self, cx: &App) {
        let Some(gateway) = &self.client.state.gateway else {
            return;
        };
        self.bots.drafts.retain(|scope, draft| {
            let exists = match scope {
                AgentScope::Defaults => true,
                AgentScope::Bot(id) => gateway.bots.iter().any(|bot| &bot.id == id),
            };
            exists || draft.saving.is_some() || draft.rejected.is_some() || draft.is_dirty(cx)
        });
    }

    pub(super) fn saved(&self, scope: &AgentScope) -> Option<Saved> {
        match scope {
            AgentScope::Defaults => {
                let defaults = self
                    .client
                    .state
                    .gateway
                    .as_ref()?
                    .agent_catalog
                    .bot_defaults
                    .as_ref()?;
                Some(Saved {
                    revision: defaults.revision,
                    config: HarnessConfig::Mobius(Arc::new(defaults.config.clone())), // rules-allow: a draft's base outlives the gateway payload it is read from
                    identity: None,
                    shape: BotShape::Circle,
                })
            }
            AgentScope::Bot(id) => self.client.state.bot(id).map(|bot| Saved {
                revision: bot.config.revision,
                config: bot.config.config.clone(), // rules-allow: a draft's base outlives the gateway payload it is read from
                identity: Some((
                    Arc::clone(&bot.name),
                    Arc::clone(&bot.description),
                    bot.tint,
                )),
                shape: bot.shape,
            }),
        }
    }

    /// Starts a draft, or restarts it when the saved values moved and nothing is lost.
    pub(super) fn ensure_draft(
        &mut self,
        scope: &AgentScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(revision) = self.saved_revision(scope) else {
            return;
        };
        if self
            .bots
            .drafts
            .get(scope)
            .is_some_and(|draft| draft.base.revision == revision || draft.saving.is_some())
        {
            return;
        }
        let Some(saved) = self.saved(scope) else {
            return;
        };
        let Some(gateway) = self.client.state.gateway.as_ref() else {
            return;
        };
        if let Some(draft) = self.bots.drafts.get_mut(scope) {
            let mut composed = draft.composed(cx);
            let edited = composed != draft.base;
            // The draft composes with its base revision, so a save of its own moves the
            // revision alone: that is up to date, not changed elsewhere.
            composed.revision = saved.revision;
            if edited && composed != saved {
                draft.rebase_model(saved, gateway);
                return;
            }
        }
        let draft = AgentDraft::new(saved, gateway, window, cx);
        self.bots.drafts.insert(AgentScope::clone(scope), draft);
    }

    /// Edits the draft's configuration and drops capabilities the change excludes.
    pub(super) fn update_draft(
        &mut self,
        scope: &AgentScope,
        update: impl FnOnce(&mut AgentDraft),
        cx: &mut Context<Self>,
    ) {
        let Some(gateway) = self.client.state.gateway.as_ref() else {
            return;
        };
        if let Some(draft) = self.bots.drafts.get_mut(scope) {
            update(draft);
            if let Some(config) = draft.mobius_mut() {
                config
                    .middleware
                    .reconcile(&gateway.agent_catalog.middleware_features);
            }
            draft.rejected = None;
        }
        cx.notify();
    }

    /// The draft's model, voice, capabilities and extensions, as the model picker edits them.
    pub(in crate::ui) fn draft_config(&self, scope: &AgentScope) -> Option<&AgentComposition> {
        self.bots.drafts.get(scope).and_then(AgentDraft::mobius)
    }

    pub(in crate::ui) fn edit_draft_config(
        &mut self,
        scope: &AgentScope,
        edit: impl FnOnce(&mut AgentComposition),
        cx: &mut Context<Self>,
    ) {
        self.update_draft(scope, |draft| draft.mobius_mut().map_or((), edit), cx);
    }

    /// The harness a Bot draft runs on.
    pub(super) fn scope_harness(&self, scope: &AgentScope) -> Option<HarnessKind> {
        match scope {
            AgentScope::Defaults => Some(HarnessKind::Mobius),
            AgentScope::Bot(id) => self.client.state.bot(id).map(|bot| bot.harness),
        }
    }

    pub(super) fn reload_draft(
        &mut self,
        scope: &AgentScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.bots.drafts.remove(scope);
        self.ensure_draft(scope, window, cx);
        cx.notify();
    }

    /// Whether a chat of this Bot is running; the gateway refuses changes meanwhile.
    pub(super) fn scope_busy(&self, scope: &AgentScope) -> bool {
        let AgentScope::Bot(id) = scope else {
            return false;
        };
        self.client.state.sessions.iter().any(|session| {
            &session.session_context.owner_id == id
                && session.activity.state != SessionActivityState::Idle
        })
    }
}

#[cfg(feature = "snapshot")]
impl Workspace {
    pub(in crate::ui) fn snapshot_draft_is_dirty(&self, scope: &AgentScope, cx: &App) -> bool {
        self.bots
            .drafts
            .get(scope)
            .is_some_and(|draft| draft.is_dirty(cx))
    }

    /// Recolours the draft without saving it, to show the unsaved state.
    pub(in crate::ui) fn snapshot_edit_draft(
        &mut self,
        scope: &AgentScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ensure_draft(scope, window, cx);
        self.update_draft(scope, |draft| draft.tint = ProviderTint::Green, cx);
    }

    /// Opens the Model section's picker on the settings page, or in the chat side tab.
    pub(in crate::ui) fn snapshot_agent_model_picker(
        &mut self,
        scope: &Rc<AgentScope>,
        tab: bool,
        cx: &mut Context<Self>,
    ) {
        let surface = if tab {
            super::sections::TAB
        } else {
            super::sections::PAGE
        };
        let target = crate::ui::chat::ModelTarget::Draft(Rc::clone(scope));
        self.snapshot_model_picker(surface, target, cx);
    }

    /// Opens the Voice capability's model picker on the settings page, on its list when `list`.
    pub(in crate::ui) fn snapshot_voice_picker(
        &mut self,
        scope: &Rc<AgentScope>,
        list: bool,
        cx: &mut Context<Self>,
    ) {
        let target = crate::ui::chat::ModelTarget::Setting {
            scope: Rc::clone(scope),
            feature: "voice".into(),
            setting: "model".into(),
        };
        self.snapshot_model_picker(super::sections::PAGE, target, cx);
        self.show_models(list, cx);
    }

    /// Drops the scope's draft, which the next render restarts from the saved values.
    pub(in crate::ui) fn snapshot_reset_draft(&mut self, scope: &AgentScope) {
        self.bots.drafts.remove(scope);
    }

    /// Picks another model into the draft, as the picker would; nothing is saved.
    pub(in crate::ui) fn snapshot_draft_model(
        &mut self,
        scope: &Rc<AgentScope>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ensure_draft(scope, window, cx);
        let (Some(gateway), Some(config)) = (&self.client.state.gateway, self.draft_config(scope))
        else {
            return;
        };
        let current =
            crate::ui::chat::configured_model(gateway, &config.provider).map(|model| &model.model);
        let other = gateway
            .agent_catalog
            .models
            .iter()
            .find(|model| Some(&model.model) != current && model.reasoning_effort.is_some())
            .map(|model| Arc::clone(&model.route));
        if let Some(route) = other {
            let target = crate::ui::chat::ModelTarget::Draft(Rc::clone(scope));
            self.apply_model_route(&target, Some(&route), cx);
        }
    }

    /// Discloses the first capability with a model-route setting and opens that setting's
    /// picker, on its model list when `list`.
    pub(in crate::ui) fn snapshot_setting_picker(
        &mut self,
        scope: &Rc<AgentScope>,
        list: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(gateway) = self.client.state.gateway.as_ref() else {
            return;
        };
        let found = gateway
            .agent_catalog
            .middleware_features
            .iter()
            .find_map(|feature| {
                let setting = feature
                    .settings
                    .iter()
                    .find(|setting| self.setting_route_options(setting).is_some())?;
                Some((feature, setting))
            });
        let Some((feature, setting)) = found else {
            return;
        };
        let key = format!("{}:{}", scope.key(), feature.id);
        let target = crate::ui::chat::ModelTarget::Setting {
            scope: Rc::clone(scope),
            feature: feature.id.clone(), // rules-allow: the picker target owns the IDs it names
            setting: setting.id.clone(), // rules-allow: the picker target owns the IDs it names
        };
        self.bots.collapsed.remove(key.as_str());
        self.snapshot_model_picker(super::sections::PAGE, target, cx);
        self.show_models(list, cx);
    }
}
