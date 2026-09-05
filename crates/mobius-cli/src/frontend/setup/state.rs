use std::collections::BTreeSet;

use mobius::backend::model::provider::HostedWebSearch;
use mobius::protocol::{
    FrontendSettingKind, FrontendSettingOption, FrontendSettingValue, MiddlewareFeature,
};
use mobius::{Error, Result};
use mobius_gateway::wire::{
    AgentComposition, ExtensionRecord, MiddlewareConfig, ProviderAuthKind, ProviderConfig,
    ProviderEndpointAuth, ProviderInstance, ProviderStatus, ReadyPayload,
};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use uuid::Uuid;

use super::{
    MAX_API_KEY_BYTES, MAX_ENDPOINT_BYTES, MAX_MODEL_IDS_BYTES, MAX_PROVIDER_LABEL_BYTES, SetupMode,
};

pub(super) struct ProviderEntry {
    pub(super) status: ProviderStatus,
    /// The configured setup this row edits, or `None` when it starts a new one.
    pub(super) instance: Option<ProviderInstance>,
}

/// The editable fields of the authentication page, in tab order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AuthField {
    Label,
    Credential,
    Endpoint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Page {
    Provider,
    Authentication,
    Models,
    Agent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Flow {
    Continue,
    Authenticate,
    Remove(String),
    Finish,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ApplyTarget {
    Bot,
    Default,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MiddlewareRow {
    Feature(usize),
    Setting { feature: usize, setting: usize },
    Extension { feature: usize, extension: usize },
}

pub(super) struct Progress {
    pub(super) title: &'static str,
    pub(super) detail: String,
    pub(super) verification: Option<(String, String)>,
}

pub(super) struct SetupState {
    pub(super) mode: SetupMode,
    pub(super) providers: Vec<ProviderEntry>,
    pub(super) original: AgentComposition,
    pub(super) page: Page,
    pub(super) provider: usize,
    pub(super) new_instance: String,
    pub(super) credential: String,
    pub(super) api_key_entered: bool,
    pub(super) endpoint: String,
    pub(super) label: String,
    pub(super) auth_field: AuthField,
    pub(super) authenticated: Option<(String, Option<String>)>,
    pub(super) model: usize,
    pub(super) custom_model: String,
    pub(super) reasoning: usize,
    pub(super) web_search: usize,
    pub(super) features: Vec<MiddlewareFeature>,
    pub(super) available_extensions: Vec<ExtensionRecord>,
    pub(super) selected_extensions: BTreeSet<String>,
    pub(super) expanded_features: BTreeSet<String>,
    pub(super) middleware: MiddlewareConfig,
    pub(super) target: ApplyTarget,
    pub(super) default_only: bool,
    pub(super) row: usize,
    pub(super) error: Option<String>,
    pub(super) progress: Option<Progress>,
    pub(super) remove_confirmation: Option<String>,
}

impl SetupState {
    pub(super) fn new(
        mode: SetupMode,
        preferred_provider: Option<&str>,
        gateway: &ReadyPayload,
        original: AgentComposition,
        default_only: bool,
    ) -> Result<Self> {
        let mut state = Self::from_parts(
            mode,
            validated_providers(&gateway.providers, &gateway.provider_instances)?,
            gateway.middleware_features.clone(),
            gateway.extensions.clone(),
            original,
            default_only,
        )?;
        if let Some(provider) = preferred_provider {
            state.select_provider(provider)?;
        }
        Ok(state)
    }

    pub(super) fn from_parts(
        mode: SetupMode,
        providers: Vec<ProviderEntry>,
        features: Vec<MiddlewareFeature>,
        available_extensions: Vec<ExtensionRecord>,
        original: AgentComposition,
        default_only: bool,
    ) -> Result<Self> {
        if providers.is_empty() {
            return Err(Error::Config(
                "the gateway did not advertise any providers".into(),
            ));
        }
        let provider = providers
            .iter()
            .position(|entry| {
                entry.instance.as_ref().is_some_and(|instance| {
                    instance.selection.instance == original.provider.instance
                })
            })
            .or_else(|| {
                providers
                    .iter()
                    .position(|entry| entry.status.provider == original.provider.provider)
            })
            .ok_or_else(|| {
                Error::Config(format!(
                    "the gateway did not advertise the active provider `{}`",
                    original.provider.provider
                ))
            })?;
        validate_active_provider(
            &providers[provider].status,
            providers[provider].instance.as_ref(),
            &original.provider,
        )?;
        let extension_ids = available_extensions
            .iter()
            .map(|extension| extension.id.as_str())
            .collect::<BTreeSet<_>>();
        if extension_ids.len() != available_extensions.len() {
            return Err(Error::Config(
                "the gateway advertised duplicate extension IDs".into(),
            ));
        }
        if let Some(extension) = available_extensions.iter().find(|extension| {
            !features
                .iter()
                .any(|feature| feature.id == extension.capability)
        }) {
            return Err(Error::Config(format!(
                "extension `{}` targets an unavailable capability",
                extension.id
            )));
        }
        let middleware = original.middleware.clone();
        let selected_extensions = original.extensions.clone();
        let mut state = Self {
            mode,
            providers,
            original,
            page: match mode {
                SetupMode::Login => Page::Provider,
                SetupMode::Bot => Page::Agent,
                SetupMode::BotModel => Page::Models,
            },
            provider,
            new_instance: Uuid::new_v4().to_string(),
            credential: String::new(),
            api_key_entered: false,
            endpoint: String::new(),
            label: String::new(),
            auth_field: AuthField::Label,
            authenticated: None,
            model: 0,
            custom_model: String::new(),
            reasoning: 0,
            web_search: 0,
            features,
            available_extensions,
            selected_extensions,
            expanded_features: BTreeSet::new(),
            middleware,
            target: if default_only {
                ApplyTarget::Default
            } else {
                ApplyTarget::Bot
            },
            default_only,
            row: 0,
            error: None,
            progress: None,
            remove_confirmation: None,
        };
        state.reset_provider_fields();
        Ok(state)
    }

    pub(super) fn entry(&self) -> &ProviderEntry {
        &self.providers[self.provider]
    }

    pub(super) fn definition(&self) -> &ProviderStatus {
        &self.entry().status
    }

    /// Fields the active provider actually offers, in tab order.
    pub(super) fn auth_fields(&self) -> Vec<AuthField> {
        let mut fields = vec![AuthField::Label];
        if self.definition().auth == ProviderAuthKind::ApiKey {
            fields.push(AuthField::Credential);
        }
        if self.definition().configurable_base_url() {
            fields.push(AuthField::Endpoint);
        }
        fields
    }

    fn cycle_auth_field(&mut self, forward: bool) {
        let fields = self.auth_fields();
        let current = fields
            .iter()
            .position(|field| *field == self.auth_field)
            .unwrap_or(0);
        let next = if forward {
            (current + 1) % fields.len()
        } else {
            (current + fields.len() - 1) % fields.len()
        };
        self.auth_field = fields[next];
    }

    /// The label the setup will be registered under, defaulting to the manifest name.
    pub(super) fn effective_label(&self) -> String {
        let label = self.label.trim();
        if label.is_empty() {
            self.definition().label.clone()
        } else {
            label.to_string()
        }
    }

    pub(super) fn target_instance(&self) -> String {
        self.instance().map_or_else(
            || self.new_instance.clone(),
            |entry| entry.selection.instance.clone(),
        )
    }

    pub(super) fn instance(&self) -> Option<&ProviderInstance> {
        self.entry().instance.as_ref()
    }

    /// The configured model catalog of the edited setup, empty while adding one.
    pub(super) fn instance_model_ids(&self) -> &[String] {
        self.instance().map_or(&[], |entry| &entry.model_ids)
    }

    pub(super) fn instance_reasoning_efforts(&self) -> &[String] {
        self.instance()
            .map_or(&[], |entry| &entry.reasoning_efforts)
    }

    pub(super) fn select_provider(&mut self, provider: &str) -> Result<()> {
        self.provider = self
            .providers
            .iter()
            .position(|entry| entry.status.provider == provider)
            .ok_or_else(|| {
                Error::Config(format!(
                    "provider `{provider}` is not advertised by this gateway; run `/login` to choose an available provider"
                ))
            })?;
        self.reset_provider_fields();
        Ok(())
    }

    pub(super) fn model_choice_count(&self) -> usize {
        self.definition().models.len().max(1)
    }

    pub(super) fn reasoning_choice_count(&self) -> usize {
        self.definition()
            .models
            .get(self.model)
            .map_or(1, |model| model.reasoning.len() + 1)
    }

    pub(super) fn search_choice_count(&self) -> usize {
        let count = self.definition().web_search.len();
        if count > 1 { count } else { 0 }
    }

    pub(super) fn models_action_start(&self) -> usize {
        self.model_choice_count() + self.reasoning_choice_count() + self.search_choice_count()
    }

    pub(super) fn agent_action_start(&self) -> usize {
        self.middleware_row_count()
    }

    pub(super) fn middleware_row_count(&self) -> usize {
        self.features.len()
            + self
                .features
                .iter()
                .filter(|feature| self.expanded_features.contains(&feature.id))
                .map(|feature| {
                    feature.settings.len()
                        + self
                            .available_extensions
                            .iter()
                            .filter(|extension| extension.capability == feature.id)
                            .count()
                })
                .sum::<usize>()
    }

    pub(super) fn middleware_row(&self, row: usize) -> Option<MiddlewareRow> {
        let mut current = 0;
        for (feature, definition) in self.features.iter().enumerate() {
            if row == current {
                return Some(MiddlewareRow::Feature(feature));
            }
            current += 1;
            if !self.expanded_features.contains(&definition.id) {
                continue;
            }
            for setting in 0..definition.settings.len() {
                if row == current {
                    return Some(MiddlewareRow::Setting { feature, setting });
                }
                current += 1;
            }
            for (extension, _) in self
                .available_extensions
                .iter()
                .enumerate()
                .filter(|(_, extension)| extension.capability == definition.id)
            {
                if row == current {
                    return Some(MiddlewareRow::Extension { feature, extension });
                }
                current += 1;
            }
        }
        None
    }

    pub(super) fn feature_has_children(&self, feature: usize) -> bool {
        !self.features[feature].settings.is_empty()
            || self
                .available_extensions
                .iter()
                .any(|extension| extension.capability == self.features[feature].id)
    }

    pub(super) fn toggle_feature_expansion(&mut self, feature: usize) {
        if !self.feature_has_children(feature) {
            return;
        }
        let id = self.features[feature].id.clone();
        if !self.expanded_features.remove(&id) {
            self.expanded_features.insert(id);
        }
    }

    pub(super) fn apply_target_for_row(&self) -> Option<ApplyTarget> {
        let start = match self.page {
            Page::Models => self.models_action_start(),
            Page::Agent => self.agent_action_start(),
            Page::Provider | Page::Authentication => return None,
        };
        match (self.default_only, self.row.checked_sub(start)) {
            (true, Some(0)) => Some(ApplyTarget::Default),
            (false, Some(0)) => Some(ApplyTarget::Bot),
            _ => None,
        }
    }

    pub(super) fn row_count(&self) -> usize {
        match self.page {
            Page::Provider => self.providers.len(),
            Page::Authentication => 0,
            Page::Models => {
                self.model_choice_count()
                    + self.reasoning_choice_count()
                    + self.search_choice_count()
                    + 1
            }
            Page::Agent => self.agent_action_start() + 1,
        }
    }

    pub(super) fn move_selection(&mut self, delta: isize) {
        match self.page {
            Page::Provider => {
                self.provider = (self.provider as isize + delta)
                    .rem_euclid(self.providers.len() as isize)
                    as usize;
                self.reset_provider_fields();
            }
            Page::Models | Page::Agent => {
                self.row =
                    (self.row as isize + delta).rem_euclid(self.row_count() as isize) as usize;
            }
            Page::Authentication => {}
        }
        self.error = None;
    }

    pub(super) fn handle_key(&mut self, key: KeyEvent) -> Flow {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return Flow::Continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'd'))
        {
            return Flow::Cancel;
        }
        if let Some(instance) = self.remove_confirmation.take() {
            return match key.code {
                KeyCode::Char('y' | 'Y') => Flow::Remove(instance),
                KeyCode::Char('n' | 'N') | KeyCode::Esc => Flow::Continue,
                _ => {
                    self.remove_confirmation = Some(instance);
                    Flow::Continue
                }
            };
        }
        match self.page {
            Page::Provider => self.handle_provider_key(key),
            Page::Authentication => self.handle_authentication_key(key),
            Page::Models => self.handle_models_key(key),
            Page::Agent => self.handle_agent_key(key),
        }
    }

    pub(super) fn handle_provider_key(&mut self, key: KeyEvent) -> Flow {
        match key.code {
            KeyCode::Esc => return Flow::Cancel,
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::Char('x') | KeyCode::Delete => {
                self.remove_confirmation = self
                    .instance()
                    .map(|instance| instance.selection.instance.clone());
            }
            KeyCode::Enter => {
                self.label = self
                    .instance()
                    .map_or_else(String::new, |instance| instance.label.clone());
                self.auth_field = AuthField::Label;
                self.page = Page::Authentication;
                self.error = None;
            }
            _ => {}
        }
        Flow::Continue
    }

    pub(super) fn handle_authentication_key(&mut self, key: KeyEvent) -> Flow {
        match key.code {
            KeyCode::Esc => {
                self.page = Page::Provider;
                self.error = None;
            }
            KeyCode::Tab | KeyCode::BackTab if self.auth_fields().len() > 1 => {
                self.cycle_auth_field(key.code == KeyCode::Tab);
                self.error = None;
            }
            KeyCode::Enter => {
                if let Err(error) = self.authentication_ready() {
                    self.error = Some(error.to_string());
                    return Flow::Continue;
                }
                self.error = None;
                return Flow::Authenticate;
            }
            KeyCode::Backspace if self.authentication_is_editable() => {
                match self.auth_field {
                    AuthField::Label => self.label.pop(),
                    AuthField::Endpoint => self.endpoint.pop(),
                    AuthField::Credential => self.credential.pop(),
                };
                self.error = None;
            }
            KeyCode::Char(character)
                if self.authentication_is_editable()
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.push_text(&character.to_string());
            }
            _ => {}
        }
        Flow::Continue
    }

    pub(super) fn handle_models_key(&mut self, key: KeyEvent) -> Flow {
        let custom_row = self.definition().model_ids_configurable.then_some(0);
        match key.code {
            KeyCode::Esc => {
                if self.mode == SetupMode::BotModel {
                    return Flow::Cancel;
                }
                self.page = Page::Authentication;
                self.error = None;
            }
            KeyCode::Backspace if Some(self.row) == custom_row => {
                self.model = 0;
                self.custom_model.pop();
                self.error = None;
            }
            KeyCode::Char(character)
                if Some(self.row) == custom_row
                    && character != ' '
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.model = 0;
                self.push_text(&character.to_string());
            }
            KeyCode::Up | KeyCode::BackTab => self.move_selection(-1),
            KeyCode::Down | KeyCode::Tab => self.move_selection(1),
            KeyCode::Char(' ') => {
                if let Some(target) = self.apply_target_for_row() {
                    self.target = target;
                    return self.finish();
                }
                self.select_model_row();
            }
            KeyCode::Enter => {
                if let Some(target) = self.apply_target_for_row() {
                    self.target = target;
                    return self.finish();
                }
                self.select_model_row();
            }
            _ => {}
        }
        Flow::Continue
    }

    pub(super) fn handle_agent_key(&mut self, key: KeyEvent) -> Flow {
        let middleware_row = self.middleware_row(self.row);
        match key.code {
            KeyCode::Esc => return Flow::Cancel,
            KeyCode::Up | KeyCode::BackTab => self.move_selection(-1),
            KeyCode::Down | KeyCode::Tab => self.move_selection(1),
            KeyCode::Char(' ') if matches!(middleware_row, Some(MiddlewareRow::Feature(_))) => {
                let MiddlewareRow::Feature(index) = middleware_row.expect("guarded feature row")
                else {
                    unreachable!()
                };
                let feature = &self.features[index];
                if !feature.required {
                    let enabled = !self.middleware.enabled(&feature.id);
                    self.middleware.set_enabled(&feature.id, enabled);
                    self.error = None;
                }
            }
            KeyCode::Enter | KeyCode::Right
                if matches!(middleware_row, Some(MiddlewareRow::Feature(_))) =>
            {
                let MiddlewareRow::Feature(feature) = middleware_row.expect("guarded feature row")
                else {
                    unreachable!()
                };
                if !self.expanded_features.contains(&self.features[feature].id) {
                    self.toggle_feature_expansion(feature);
                }
            }
            KeyCode::Left if matches!(middleware_row, Some(MiddlewareRow::Feature(_))) => {
                let MiddlewareRow::Feature(feature) = middleware_row.expect("guarded feature row")
                else {
                    unreachable!()
                };
                if self.expanded_features.contains(&self.features[feature].id) {
                    self.toggle_feature_expansion(feature);
                }
            }
            KeyCode::Char(' ') | KeyCode::Right
                if matches!(middleware_row, Some(MiddlewareRow::Setting { .. })) =>
            {
                self.adjust_middleware_setting(middleware_row.expect("guarded setting row"), 1);
            }
            KeyCode::Left if matches!(middleware_row, Some(MiddlewareRow::Setting { .. })) => {
                self.adjust_middleware_setting(middleware_row.expect("guarded setting row"), -1);
            }
            KeyCode::Char(' ')
                if matches!(middleware_row, Some(MiddlewareRow::Extension { .. })) =>
            {
                let MiddlewareRow::Extension { feature, extension } =
                    middleware_row.expect("guarded extension row")
                else {
                    unreachable!()
                };
                let extension = &self.available_extensions[extension];
                let id = extension.id.clone();
                if !self.selected_extensions.remove(&id) {
                    self.selected_extensions.insert(id);
                    self.middleware
                        .set_enabled(&self.features[feature].id, true);
                }
                self.error = None;
            }
            KeyCode::Enter | KeyCode::Char(' ') if self.apply_target_for_row().is_some() => {
                self.target = self
                    .apply_target_for_row()
                    .expect("guard requires an apply target");
                return self.finish();
            }
            _ => {}
        }
        Flow::Continue
    }

    pub(super) fn select_model_row(&mut self) {
        let models = self.model_choice_count();
        if self.row < models {
            if self.model != self.row {
                self.model = self.row;
                self.reasoning = 0;
            }
        } else if self.row < models + self.reasoning_choice_count() {
            self.reasoning = self.row - models;
        } else if self.row < self.models_action_start() {
            self.web_search = self.row - models - self.reasoning_choice_count();
        }
        self.error = None;
    }

    pub(super) fn adjust_middleware_setting(&mut self, row: MiddlewareRow, delta: isize) {
        let MiddlewareRow::Setting { feature, setting } = row else {
            return;
        };
        self.error = self
            .adjust_setting(feature, setting, delta)
            .err()
            .map(|error| error.to_string());
    }

    pub(super) fn adjust_setting(
        &mut self,
        feature: usize,
        setting: usize,
        delta: isize,
    ) -> Result<()> {
        let feature = &self.features[feature];
        let setting = &feature.settings[setting];
        if !feature.required && !self.middleware.enabled(&feature.id) {
            return Ok(());
        }
        let value = match &setting.kind {
            FrontendSettingKind::Integer { min, max, step } => {
                let Some(FrontendSettingValue::Integer(current)) =
                    self.middleware.setting(&feature.id, &setting.id)
                else {
                    return Err(Error::Config(format!(
                        "{} requires an integer value",
                        setting.label
                    )));
                };
                let step = (*step).max(1);
                let next = if delta.is_positive() {
                    current.saturating_add(step)
                } else {
                    current.saturating_sub(step)
                };
                FrontendSettingValue::Integer(
                    max.map_or(next.max(*min), |max| next.max(*min).min(max)),
                )
            }
            FrontendSettingKind::Select {
                options,
                unset_label,
            } => {
                let offset = usize::from(unset_label.is_some());
                let count = options.len() + offset;
                if count == 0 {
                    return Err(Error::Config(format!(
                        "{} has no advertised choices",
                        setting.label
                    )));
                }
                let current = match self.middleware.setting(&feature.id, &setting.id) {
                    Some(FrontendSettingValue::String(value)) => options
                        .iter()
                        .position(|option| option.value == *value)
                        .map(|index| index + offset)
                        .ok_or_else(|| {
                            Error::Config(format!(
                                "{} is not in the gateway catalog",
                                setting.label
                            ))
                        })?,
                    None if unset_label.is_some() => 0,
                    Some(FrontendSettingValue::Integer(_)) | None => {
                        return Err(Error::Config(format!(
                            "{} requires a selected value",
                            setting.label
                        )));
                    }
                };
                let next = (current as isize + delta).rem_euclid(count as isize) as usize;
                if next < offset {
                    self.middleware.set_setting(&feature.id, &setting.id, None);
                    return Ok(());
                }
                FrontendSettingValue::String(options[next - offset].value.clone())
            }
        };
        self.middleware
            .set_setting(&feature.id, &setting.id, Some(value));
        Ok(())
    }

    pub(super) fn finish(&mut self) -> Flow {
        if let Err(error) = self.authentication_ready() {
            self.error = Some(error.to_string());
            return Flow::Continue;
        }
        if let Err(error) = self.agent_composition(&self.original) {
            self.error = Some(error.to_string());
            return Flow::Continue;
        }
        Flow::Finish
    }

    pub(super) fn authentication_is_editable(&self) -> bool {
        self.auth_field != AuthField::Credential
            || self.definition().auth == ProviderAuthKind::ApiKey
    }

    pub(super) fn paste(&mut self, text: &str) {
        if self.page == Page::Authentication && self.authentication_is_editable() {
            self.push_text(text.trim());
        } else if self.page == Page::Models
            && self.definition().model_ids_configurable
            && self.row == 0
        {
            self.model = 0;
            self.push_text(text.trim());
        }
    }

    pub(super) fn push_text(&mut self, text: &str) {
        let (target, limit) = if self.page == Page::Models {
            (&mut self.custom_model, MAX_MODEL_IDS_BYTES)
        } else {
            match self.auth_field {
                AuthField::Label => (&mut self.label, MAX_PROVIDER_LABEL_BYTES),
                AuthField::Endpoint => (&mut self.endpoint, MAX_ENDPOINT_BYTES),
                AuthField::Credential => (&mut self.credential, MAX_API_KEY_BYTES),
            }
        };
        let mut rejected = false;
        for character in text.chars().filter(|character| !character.is_control()) {
            if target.len() + character.len_utf8() > limit {
                rejected = true;
                break;
            }
            target.push(character);
        }
        self.error = rejected.then(|| format!("input is limited to {limit} bytes"));
    }

    pub(super) fn reset_provider_fields(&mut self) {
        self.credential.clear();
        self.api_key_entered = false;
        self.authenticated = None;
        let definition = self.entry().status.clone();
        let current = &self.original.provider;
        let same_instance = self
            .instance()
            .is_some_and(|instance| instance.selection.instance == current.instance);
        self.endpoint = if same_instance {
            current
                .base_url
                .as_deref()
                .or(definition.default_base_url.as_deref())
        } else {
            definition.default_base_url.as_deref()
        }
        .unwrap_or_default()
        .into();
        self.model = if definition.model_ids_configurable {
            0
        } else if same_instance {
            definition
                .models
                .iter()
                .position(|model| model.id == current.model)
                .expect("active provider model was validated")
        } else {
            0
        };
        self.custom_model = if definition.model_ids_configurable {
            let mut model_ids = self.instance_model_ids().to_vec();
            if same_instance && !model_ids.contains(&current.model) {
                model_ids.insert(0, current.model.clone());
            }
            model_ids.join(", ")
        } else {
            String::new()
        };
        let reasoning = if same_instance {
            current.reasoning_effort.as_deref()
        } else {
            definition
                .models
                .get(self.model)
                .and_then(|model| model.default_reasoning.as_deref())
        };
        self.reasoning = definition
            .models
            .get(self.model)
            .and_then(|model| {
                reasoning.and_then(|effort| {
                    model
                        .reasoning
                        .iter()
                        .position(|preset| preset.id == effort)
                })
            })
            .map_or(0, |index| index + 1);
        self.web_search = if same_instance {
            definition
                .web_search
                .iter()
                .position(|search| search.value == current.web_search.id())
                .expect("active provider search mode was validated")
        } else {
            0
        };
        self.auth_field = AuthField::Label;
        self.row = self.model;
        self.error = None;
    }

    pub(super) fn configured_model_ids(&self) -> Result<Vec<String>> {
        if !self.definition().model_ids_configurable {
            return Ok(Vec::new());
        }
        let model_ids = self
            .custom_model
            .split(',')
            .map(str::trim)
            .map(str::to_string)
            .collect::<Vec<_>>();
        if model_ids.iter().any(String::is_empty) {
            return Err(Error::Config(
                "Enter one or more model IDs separated by commas".into(),
            ));
        }
        if model_ids.iter().collect::<BTreeSet<_>>().len() != model_ids.len() {
            return Err(Error::Config("Model IDs must be unique".into()));
        }
        Ok(model_ids)
    }

    pub(super) fn selected_base_url(&self) -> Option<String> {
        self.definition()
            .configurable_base_url()
            .then(|| self.endpoint.trim().to_string())
    }

    pub(super) fn authentication_target(&self) -> (String, Option<String>) {
        (self.definition().provider.clone(), self.selected_base_url())
    }

    pub(super) fn authentication_succeeded(&mut self) {
        self.authenticated = Some(self.authentication_target());
        self.progress = None;
        self.page = Page::Models;
        self.row = self.model;
    }

    pub(super) fn has_matching_credential(&self) -> bool {
        let target = self.authentication_target();
        self.authenticated.as_ref() == Some(&target)
            || self.instance().is_some_and(|entry| {
                entry.configured
                    && entry.selection.provider == target.0.as_str()
                    && entry.selection.base_url.as_deref() == target.1.as_deref()
            })
            || self.definition().auth == ProviderAuthKind::DeviceCode
                && self.providers.iter().any(|entry| {
                    entry.status.provider == target.0
                        && entry
                            .instance
                            .as_ref()
                            .is_some_and(|instance| instance.configured)
                })
    }

    pub(super) fn authentication_ready(&self) -> Result<()> {
        if self.mode != SetupMode::Login {
            return Ok(());
        }
        if self.definition().configurable_base_url()
            && self
                .selected_base_url()
                .is_none_or(|url| url.trim().is_empty())
        {
            return Err(Error::Config("Base URL is required".into()));
        }
        Ok(())
    }

    pub(super) fn take_authentication(&mut self) -> Result<Authentication> {
        self.authentication_ready()?;
        if self.mode != SetupMode::Login {
            return Ok(Authentication::Reuse);
        }
        match self.definition().auth {
            ProviderAuthKind::ApiKey => {
                let credential = take_trimmed(&mut self.credential);
                if !credential.is_empty() {
                    self.api_key_entered = true;
                    Ok(Authentication::ApiKey(credential))
                } else {
                    Ok(Authentication::Reuse)
                }
            }
            ProviderAuthKind::DeviceCode if self.has_matching_credential() => {
                Ok(Authentication::Reuse)
            }
            ProviderAuthKind::DeviceCode => Ok(Authentication::DeviceCode),
        }
    }

    pub(super) fn agent_composition(&self, current: &AgentComposition) -> Result<AgentComposition> {
        let mut config = current.clone();
        if self.mode == SetupMode::Bot {
            config.middleware = self.middleware.clone();
            config.extensions = self.selected_extensions.clone();
            return Ok(config);
        }
        let definition = self.definition();
        let model_ids = self.configured_model_ids()?;
        let model = definition.models.get(self.model).map_or_else(
            || model_ids.first().map_or("", String::as_str),
            |model| model.id.as_str(),
        );
        let reasoning_effort = if let Some(model) = definition.models.get(self.model) {
            self.reasoning
                .checked_sub(1)
                .and_then(|index| model.reasoning.get(index))
                .map(|preset| preset.id.to_string())
        } else if current.provider.instance == self.target_instance()
            && current.provider.model == model
        {
            current.provider.reasoning_effort.clone()
        } else {
            None
        };
        let web_search = definition
            .web_search
            .get(self.web_search)
            .ok_or_else(|| Error::Config("Hosted web-search selection is invalid".into()))?
            .value
            .parse::<HostedWebSearch>()?;
        let base_url = self.selected_base_url();
        let endpoint_auth = if self.api_key_entered {
            ProviderEndpointAuth::ProviderDefault
        } else if current.provider.instance == self.target_instance()
            && current.provider.base_url.as_deref() == base_url.as_deref()
        {
            current.provider.endpoint_auth
        } else {
            ProviderEndpointAuth::ProviderDefault
        };
        if model.is_empty() {
            return Err(Error::Config("Model is required".into()));
        }
        config.provider = ProviderConfig {
            instance: self.target_instance(),
            provider: definition.provider.clone(),
            model: model.into(),
            base_url,
            endpoint_auth,
            reasoning_effort,
            web_search,
        };
        if config.realtime_voice.as_ref().is_some_and(|voice| {
            !definition
                .realtime_voices(config.provider.base_url.as_deref())
                .contains(voice)
        }) {
            config.realtime_voice = None;
        }
        if self.mode == SetupMode::BotModel {
            config.middleware = current.middleware.clone();
            config.extensions = current.extensions.clone();
        }
        Ok(config)
    }

    pub(super) fn set_progress(&mut self, title: &'static str, detail: impl Into<String>) {
        self.progress = Some(Progress {
            title,
            detail: detail.into(),
            verification: None,
        });
    }

    pub(super) fn show_device_code(&mut self, verification_url: String, user_code: String) {
        self.progress = Some(Progress {
            title: "Complete device login",
            detail: "Open the verification URL and enter this one-time code.".into(),
            verification: Some((verification_url, user_code)),
        });
    }
}

pub(super) enum Authentication {
    Reuse,
    ApiKey(String),
    DeviceCode,
}

/// Rows are every configured setup first, then one "add setup" row per definition.
pub(super) fn validated_providers(
    statuses: &[ProviderStatus],
    instances: &[ProviderInstance],
) -> Result<Vec<ProviderEntry>> {
    let mut seen = BTreeSet::new();
    let definitions = statuses
        .iter()
        .map(|status| {
            if status.provider.trim().is_empty() || !seen.insert(status.provider.as_str()) {
                return Err(Error::Config(format!(
                    "gateway advertised invalid or duplicate provider `{}`",
                    status.provider
                )));
            }
            if status.label.trim().is_empty()
                || status.description.trim().is_empty()
                || !valid_web_search_options(&status.web_search)
                || status.model_ids_configurable != status.models.is_empty()
            {
                return Err(Error::Config(format!(
                    "gateway advertised an incomplete manifest for `{}`",
                    status.provider
                )));
            }
            Ok(status.clone())
        })
        .collect::<Result<Vec<_>>>()?;

    let mut seen_instances = BTreeSet::new();
    let mut rows = Vec::with_capacity(instances.len() + definitions.len());
    for instance in instances {
        if instance.selection.instance.trim().is_empty()
            || !seen_instances.insert(instance.selection.instance.as_str())
        {
            return Err(Error::Config(format!(
                "gateway advertised invalid or duplicate provider instance `{}`",
                instance.selection.instance
            )));
        }
        let status = definitions
            .iter()
            .find(|status| status.provider == instance.selection.provider)
            .ok_or_else(|| {
                Error::Config(format!(
                    "gateway advertised instance `{}` for unknown provider `{}`",
                    instance.selection.instance, instance.selection.provider
                ))
            })?;
        validate_active_provider(status, Some(instance), &instance.selection)?;
        rows.push(ProviderEntry {
            status: status.clone(),
            instance: Some(instance.clone()),
        });
    }
    rows.extend(definitions.into_iter().map(|status| ProviderEntry {
        status,
        instance: None,
    }));
    Ok(rows)
}

pub(super) fn validate_active_provider(
    status: &ProviderStatus,
    instance: Option<&ProviderInstance>,
    config: &ProviderConfig,
) -> Result<()> {
    if !status
        .web_search
        .iter()
        .any(|search| search.value == config.web_search.id())
    {
        return Err(Error::Config(format!(
            "gateway active provider `{}` has an unadvertised web-search mode",
            status.provider
        )));
    }
    if status.configurable_base_url() != config.base_url.is_some() {
        return Err(Error::Config(format!(
            "gateway active provider `{}` has invalid endpoint settings",
            status.provider
        )));
    }
    if status.model_ids_configurable {
        let model_ids = instance.map_or(&[][..], |entry| &entry.model_ids);
        let reasoning_efforts = instance.map_or(&[][..], |entry| &entry.reasoning_efforts);
        if !model_ids.iter().any(|model| model == &config.model) {
            return Err(Error::Config(format!(
                "gateway active provider `{}` has unconfigured model `{}`",
                status.provider, config.model
            )));
        }
        if let Some(effort) = config.reasoning_effort.as_deref()
            && !reasoning_efforts.iter().any(|choice| choice == effort)
        {
            return Err(Error::Config(format!(
                "gateway active provider `{}` has unconfigured reasoning `{effort}`",
                status.provider
            )));
        }
        return Ok(());
    }
    let model = status
        .models
        .iter()
        .find(|model| model.id == config.model)
        .ok_or_else(|| {
            Error::Config(format!(
                "gateway active provider `{}` has unadvertised model `{}`",
                status.provider, config.model
            ))
        })?;
    if let Some(effort) = config.reasoning_effort.as_deref()
        && !model.reasoning.iter().any(|choice| choice.id == effort)
    {
        return Err(Error::Config(format!(
            "gateway active model `{}` has unadvertised reasoning `{effort}`",
            model.id
        )));
    }
    Ok(())
}

fn valid_web_search_options(options: &[FrontendSettingOption]) -> bool {
    let mut values = BTreeSet::new();
    options
        .first()
        .is_some_and(|option| option.value == HostedWebSearch::Off.id())
        && options.iter().all(|option| {
            !option.value.trim().is_empty()
                && !option.label.trim().is_empty()
                && !option.description.trim().is_empty()
                && option.value.parse::<HostedWebSearch>().is_ok()
                && values.insert(option.value.as_str())
        })
}

pub(super) fn take_trimmed(value: &mut String) -> String {
    let mut value = std::mem::take(value);
    value.truncate(value.trim_end().len());
    let start = value.len() - value.trim_start().len();
    value.drain(..start);
    value
}
