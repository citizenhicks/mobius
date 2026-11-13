//! One provider setup's editor: Setup, Model and Credential (ProvidersView.swift).

use super::*;
use gpui_component::input::InputEvent;
use mobius::backend::model::provider::{BaseUrl, HostedWebSearch};
use mobius::protocol::ToolDiscoveryMode;

const IMAGE_MODEL_IDS_INFO: &str = "Enter the exact provider image model IDs separated by commas. Whitespace, empty entries, and duplicates are ignored. Choose one in a Bot's Image generation capability.";
const WEB_SEARCH: [HostedWebSearch; 3] = [
    HostedWebSearch::Off,
    HostedWebSearch::Cached,
    HostedWebSearch::Live,
];

const MODEL_FILES_NOTE: &str = "Context windows and other model details come from the model files in the gateway's models folder. Edit them there; the app and CLI cannot change them.";
const TOOL_DISCOVERY_INFO: &str = "How tools found mid-turn reach the model.";

/// A custom model ID and the reasoning efforts the gateway serves for it.
pub(super) type SavedModel = (String, Vec<String>, Option<String>);

/// One custom model ID with its own comma-separated reasoning efforts.
struct ModelEntry {
    id: SharedString,
    efforts: Entity<InputState>,
    /// The stored default, kept while it stays in the typed list.
    default: Option<String>,
}

pub(super) struct ProviderDraft {
    pub record: ProviderInstance,
    pub definition: ProviderStatus,
    /// Not yet registered on the gateway.
    pub is_new: bool,
    name: Entity<InputState>,
    /// Custom model IDs in order; the wire takes the first as the selected model.
    models: Vec<ModelEntry>,
    /// The same list as the gateway serves it, to tell edits apart.
    saved_models: Vec<SavedModel>,
    /// The model row whose reasoning efforts are open.
    expanded: Option<usize>,
    new_model: Entity<InputState>,
    image_models: Entity<InputState>,
    base_url: Entity<InputState>,
    pub api_key: Entity<InputState>,
    changes: Vec<Subscription>,
}

impl ProviderDraft {
    pub(super) fn new(
        record: ProviderInstance,
        definition: ProviderStatus,
        is_new: bool,
        saved_models: Vec<SavedModel>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Self {
        let mut input = |value: &str, placeholder: &str, masked: bool| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder.to_owned())
                    .masked(masked)
                    .default_value(value.to_owned())
            })
        };
        let name = input(&record.label, &definition.label, false);
        let new_model = input("", "Model ID, then Return", false);
        let models: Vec<ModelEntry> = saved_models
            .iter()
            .map(|(id, efforts, default)| ModelEntry {
                id: id.to_owned().into(),
                efforts: input(&efforts.join(", "), "low, medium, high", false),
                default: default.to_owned(),
            })
            .collect();
        let image_models = input(
            &record.image_model_ids.join(", "),
            "image-model-a, image-model-b",
            false,
        );
        let base_url = input(
            record.selection.base_url.as_deref().unwrap_or_default(),
            "Provider endpoint",
            false,
        );
        let key_placeholder = if is_new {
            "API key"
        } else {
            "Paste a new key to replace it"
        };
        let api_key = input("", key_placeholder, true);
        let mut changes: Vec<Subscription> = [&name, &image_models, &base_url, &api_key]
            .into_iter()
            .chain(models.iter().map(|model| &model.efforts))
            .map(|field| {
                cx.subscribe(field, |_, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        cx.notify();
                    }
                })
            })
            .collect();
        changes.push(cx.subscribe_in(
            &new_model,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.add_provider_model(window, cx);
                }
            },
        ));
        Self {
            record,
            definition,
            is_new,
            name,
            models,
            saved_models,
            expanded: None,
            new_model,
            image_models,
            base_url,
            api_key,
            changes,
        }
    }

    #[cfg(feature = "snapshot")]
    pub(super) fn expand(&mut self, index: Option<usize>) {
        self.expanded = index;
    }

    pub(super) fn registration(&self, cx: &App) -> mobius::Result<Option<ClientMessage>> {
        let mut config = self.record.selection.clone(); // rules-allow: the registration owns its config while the editor keeps editing
        let label = self.name.read(cx).value().trim().to_owned();
        let entries = |field: &Entity<InputState>| -> Vec<String> {
            split_entries(&field.read(cx).value())
                .into_iter()
                .map(str::to_owned)
                .collect()
        };
        let image_model_ids = entries(&self.image_models);
        let values = self.effort_values(cx);
        // Each row as (id, efforts, default); the first is the setup's selected model.
        let rows: Vec<(String, Vec<String>, Option<String>)> =
            if self.definition.model_ids_configurable {
                self.models
                    .iter()
                    .zip(&values)
                    .map(|(model, value)| {
                        let efforts: Vec<String> = split_entries(value)
                            .into_iter()
                            .map(str::to_owned)
                            .collect();
                        let default = kept_effort(&efforts, model.default.as_deref());
                        (model.id.to_string(), efforts, default)
                    })
                    .collect()
            } else {
                Vec::new()
            };
        if let Some((id, efforts, default)) = rows.first() {
            id.clone_into(&mut config.model);
            config.reasoning_effort = kept_effort(efforts, config.reasoning_effort.as_deref())
                .or_else(|| default.to_owned());
        }
        // An unchanged list is omitted, so the gateway keeps whatever another client saved.
        let models: Vec<ConfiguredModel> = rows
            .into_iter()
            .map(|(id, efforts, default)| {
                let unchanged = self
                    .saved_models
                    .iter()
                    .any(|(saved, saved_efforts, _)| *saved == id && *saved_efforts == efforts);
                if unchanged {
                    ConfiguredModel {
                        id,
                        ..ConfiguredModel::default()
                    }
                } else {
                    ConfiguredModel {
                        id,
                        reasoning_efforts: Some(efforts),
                        default_reasoning: default,
                        ..ConfiguredModel::default()
                    }
                }
            })
            .collect();
        if !self.can_register(cx) {
            return Ok(None);
        }
        let base_url = self.base_url.read(cx).value().trim().to_owned();
        config.base_url = (!base_url.is_empty())
            .then(|| BaseUrl::new(base_url))
            .transpose()?;
        Ok(Some(ClientMessage::RegisterProvider {
            request_id: crate::gateway::request_id(),
            config,
            label: Some(label),
            tint: Some(self.record.tint),
            models,
            image_model_ids: self
                .definition
                .image_model_ids_configurable
                .then_some(image_model_ids),
            operator_managed: false,
            preserve_selection: false,
            if_configured: false,
        }))
    }

    /// Each model's typed efforts field, in model order.
    fn effort_values(&self, cx: &App) -> Vec<SharedString> {
        self.models
            .iter()
            .map(|model| model.efforts.read(cx).value())
            .collect()
    }

    pub(super) fn can_register(&self, cx: &App) -> bool {
        !self.name.read(cx).value().trim().is_empty()
            && if !self.is_new && self.record.selection.model.is_empty() {
                true
            } else if self.definition.model_ids_configurable {
                !self.models.is_empty()
            } else {
                !self.record.selection.model.is_empty()
            }
    }

    /// Compare borrowed form values; a request ID and owned config are only made when saving.
    fn is_dirty(&self, live: &ProviderInstance, cx: &App) -> bool {
        if !self.can_register(cx) {
            return true;
        }
        let image_models = self.image_models.read(cx).value();
        let image_model_ids = split_entries(&image_models);
        let model = match self.models.first() {
            Some(first) if self.definition.model_ids_configurable => first.id.as_ref(),
            _ => &self.record.selection.model,
        };
        let base_url = self.base_url.read(cx).value();
        let base_url = base_url.trim();
        self.name.read(cx).value().trim() != live.label
            || self.record.tint != live.tint
            || self.models.len() != self.saved_models.len()
            || self
                .models
                .iter()
                .zip(&self.saved_models)
                .any(|(model, (id, efforts, _))| {
                    model.id.as_ref() != id
                        || !split_entries(&model.efforts.read(cx).value())
                            .into_iter()
                            .eq(efforts.iter().map(String::as_str))
                })
            || !image_model_ids
                .iter()
                .copied()
                .eq(live.image_model_ids.iter().map(String::as_str))
            || model != live.selection.model
            || (!base_url.is_empty()).then_some(base_url) != live.selection.base_url.as_deref()
            || self.record.selection.web_search != live.selection.web_search
    }
}

impl Workspace {
    pub(super) fn provider_editor(&self, cx: &Context<Self>) -> AnyElement {
        let Some(editor) = &self.provider_setup.editor else {
            return self.provider_unavailable();
        };
        let id = &editor.record.selection.instance;
        let live = self.provider_record(id).filter(|_| !editor.is_new);
        let status = match live {
            None => "New setup",
            Some(live) if live.configured => "Configured on gateway",
            Some(_) => "Not configured",
        };
        let title = live.map_or(editor.definition.label.as_str(), |live| live.label.as_str());
        let header =
            PageHeader::new(title).subtitle(format!("{} · {status}", editor.definition.label));
        page(SharedString::from(format!("provider:{id}")), header)
            .child(Section::new("Setup").child(Self::provider_setup_group(editor, cx)))
            .child(
                Section::new("Models")
                    .info(MODEL_FILES_NOTE)
                    .child(self.model_rows(editor, cx)),
            )
            .child(Section::new("Options").child(self.provider_model_group(editor, cx)))
            .child(self.provider_credential(editor, live, cx))
            .into_any_element()
    }

    /// Remove provider… for a setup on the gateway, then Save.
    pub(super) fn provider_page_actions(&self, cx: &Context<Self>) -> Vec<AnyElement> {
        let Some(editor) = &self.provider_setup.editor else {
            return Vec::new();
        };
        let id = &editor.record.selection.instance;
        let live = self.provider_record(id).filter(|_| !editor.is_new);
        let key_entered = !editor.api_key.read(cx).value().trim().is_empty();
        let dirty = live.is_none_or(|live| editor.is_dirty(live, cx));
        let can_save =
            self.client.link.is_ready() && editor.can_register(cx) && (dirty || key_entered);
        let mut actions = Vec::new();
        if live.is_some() {
            actions.push(
                toolbar_button("remove-provider", "Remove provider…", "hi.trash", cx)
                    .disabled(!self.client.link.is_ready())
                    .on_click(
                        cx.listener(|this, _, window, cx| this.remove_provider_dialog(window, cx)),
                    )
                    .into_any_element(),
            );
        }
        actions.push(
            toolbar_button("save-provider", "Save", "hi.check", cx)
                .disabled(!can_save)
                .when(can_save, |button| button.text_color(self.palette.accent))
                .on_click(cx.listener(|this, _, window, cx| this.save_provider(window, cx)))
                .into_any_element(),
        );
        actions
    }

    fn provider_setup_group(editor: &ProviderDraft, cx: &Context<Self>) -> Group {
        let glyph = symbol_icon_or(Some(&editor.definition.symbol), "hi.hardDrives");
        let service = div()
            .flex()
            .items_center()
            .gap(space::S)
            .typography(Type::Body)
            .child(icon(glyph, &palette(cx)).size(glyph::LEAD))
            .child(SharedString::from(editor.definition.label.as_str()));
        Group::new()
            .child(
                Row::new("provider-name", "Name")
                    .subtitle("Shown in model pickers and usage.")
                    .control(text_field(&editor.name).w(width::FIELD)),
            )
            .child(Row::new("provider-service", "Provider").control(service))
            .child(Row::new("provider-colour", "Colour").control(swatch_picker(
                "provider-tint",
                Accent::from(editor.record.tint),
                cx.listener(|this, accent: &Accent, _, cx| {
                    if let Some(editor) = &mut this.provider_setup.editor {
                        editor.record.tint = (*accent).into();
                        cx.notify();
                    }
                }),
            )))
    }

    fn provider_model_group(&self, editor: &ProviderDraft, cx: &Context<Self>) -> Group {
        let muted = |text: String| {
            div()
                .typography(Type::Body)
                .text_color(self.palette.muted)
                .child(text)
        };
        let definition = &editor.definition;
        let group = Group::new();
        let image_models = if definition.image_model_ids_configurable {
            Some(
                StackedField::new("Image model ID(s)")
                    .info(IMAGE_MODEL_IDS_INFO)
                    .control(text_field(&editor.image_models)),
            )
        } else {
            let ids = definition
                .image_models
                .iter()
                .map(|model| model.id.as_str());
            (!definition.image_models.is_empty()).then(|| {
                StackedField::new("Image model ID(s)")
                    .info("Every image model listed is available to Image generation.")
                    .control(muted(ids.collect::<Vec<_>>().join(", ")))
            })
        };
        let voices = definition
            .voice_models
            .iter()
            .map(|model| model.label.as_str());
        let voice_models = (!definition.voice_models.is_empty()).then(|| {
            StackedField::new("Voice model(s)")
                .info("Every voice model listed is available to assign to Bots.")
                .control(muted(voices.collect::<Vec<_>>().join(", ")))
        });
        let base_url = StackedField::new("Base URL").control(text_field(&editor.base_url));
        let discovery = match tool_discovery(editor, cx) {
            ToolDiscoveryMode::Native => "Native dynamic tools",
            ToolDiscoveryMode::Rebuild => "Rebuilds context · cache miss",
        };
        group
            .children(image_models)
            .children(voice_models)
            .child(base_url)
            .child(
                Row::new("provider-tool-discovery", "Tool discovery")
                    .subtitle(TOOL_DISCOVERY_INFO)
                    .value(discovery),
            )
            .children(Self::web_search_row(editor, cx))
    }

    /// One row per model: typed IDs get Remove and an efforts field that drops down;
    /// a fixed catalog shows its efforts read-only.
    fn model_rows(&self, editor: &ProviderDraft, cx: &Context<Self>) -> Group {
        let mut group = Group::new();
        if !editor.definition.model_ids_configurable {
            if editor.record.selection.model.is_empty() {
                return group;
            }
            for (index, model) in editor.definition.models.iter().enumerate() {
                let open = editor.expanded == Some(index);
                group = group.child(
                    Row::new(("provider-preset-model", index), model.label.as_str())
                        .truncate()
                        .control(self.model_caret(index, open, cx)),
                );
                if open {
                    let efforts: Vec<&str> = model
                        .reasoning
                        .iter()
                        .map(|level| level.id.as_str())
                        .collect();
                    let efforts = if efforts.is_empty() {
                        "Provider default".to_owned()
                    } else {
                        efforts.join(", ")
                    };
                    let field = Row::new(("provider-preset-efforts", index), "Reasoning effort(s)")
                        .value(efforts);
                    group = group.child(Self::dropped(field, index, cx));
                }
            }
            return group;
        }
        for (index, model) in editor.models.iter().enumerate() {
            let open = editor.expanded == Some(index);
            group = group.child(
                Row::new(("provider-model", index), SharedString::clone(&model.id))
                    .truncate()
                    .control(Self::remove_model_button(editor, index, cx))
                    .control(self.model_caret(index, open, cx)),
            );
            if open {
                let field = Row::new(("provider-model-efforts", index), "Reasoning effort(s)")
                    .control(text_field(&model.efforts).w(width::FIELD));
                group = group.child(Self::dropped(field, index, cx));
            }
        }
        group = group.child(
            Row::new("provider-add-model", "Add model")
                .control(text_field(&editor.new_model).w(width::FIELD)),
        );
        group
    }

    /// Fades a model's dropped-down field in under its row.
    fn dropped(field: Row, index: usize, cx: &App) -> AnyElement {
        motion::fade_in(
            div().child(field),
            ("provider-model-efforts", index),
            motion::STANDARD,
            cx,
        )
    }

    /// The icon-only chevron that drops a model's efforts down.
    fn model_caret(&self, index: usize, open: bool, cx: &Context<Self>) -> AnyElement {
        let caret = chevron(open, &self.palette);
        let label = if open { "Hide efforts" } else { "Show efforts" };
        inline_button(("model-caret", index), label, caret, cx)
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(editor) = &mut this.provider_setup.editor {
                    editor.expanded = (editor.expanded != Some(index)).then_some(index);
                    cx.notify();
                }
            }))
            .into_any_element()
    }

    fn remove_model_button(editor: &ProviderDraft, index: usize, cx: &Context<Self>) -> AnyElement {
        inline_button(("remove-model", index), "Remove model", "hi.trash", cx)
            .disabled(editor.models.len() == 1)
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(editor) = &mut this.provider_setup.editor {
                    editor.models.remove(index);
                    editor.expanded = None;
                    cx.notify();
                }
            }))
            .into_any_element()
    }

    /// Adds the typed model ID with the efforts of the last model.
    fn add_provider_model(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = &self.provider_setup.editor else {
            return;
        };
        let value = editor.new_model.read(cx).value();
        let efforts = editor
            .models
            .last()
            .map_or_default(|model| model.efforts.read(cx).value());
        if self.push_provider_model(value.trim(), &efforts, window, cx)
            && let Some(editor) = &self.provider_setup.editor
        {
            editor
                .new_model
                .update(cx, |input, cx| input.set_value("", window, cx));
        }
    }

    /// Appends model `id` with `efforts` unless it is empty or already listed.
    pub(super) fn push_provider_model(
        &mut self,
        id: &str,
        efforts: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let listed = self
            .provider_setup
            .editor
            .as_ref()
            .is_none_or(|editor| editor.models.iter().any(|model| model.id.as_ref() == id));
        if id.is_empty() || listed {
            return false;
        }
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("low, medium, high")
                .default_value(efforts.to_owned())
        });
        let changes = cx.subscribe(&input, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        input.update(cx, |input, cx| input.focus(window, cx));
        if let Some(editor) = &mut self.provider_setup.editor {
            editor.changes.push(changes);
            editor.expanded = Some(editor.models.len());
            editor.models.push(ModelEntry {
                id: id.to_owned().into(),
                efforts: input,
                default: None,
            });
        }
        cx.notify();
        true
    }

    /// Off and one mode is a switch; more modes pick from a segmented control.
    fn web_search_row(editor: &ProviderDraft, cx: &Context<Self>) -> Option<Row> {
        let options = &editor.definition.web_search;
        let current = editor.record.selection.web_search.id();
        let selected = options.iter().position(|option| *option.value == *current);
        let mut row = Row::new("provider-web-search", "Hosted web search");
        if let Some(option) = selected.and_then(|index| options.get(index)) {
            row = row.subtitle(Arc::clone(&option.description));
        }
        Some(match options.as_slice() {
            [] => return None,
            [only] => row.value(Arc::clone(&only.label)),
            [a, b] if &*a.value == "off" || &*b.value == "off" => {
                let on = if &*a.value == "off" { b } else { a };
                let (on_value, enabled) = (Arc::clone(&on.value), current != "off");
                let pick = cx.listener(move |this, checked: &bool, _, cx| {
                    let value = if *checked { on_value.as_ref() } else { "off" };
                    this.set_web_search(value, cx);
                });
                row.control(toggle("web-search", "Hosted web search", enabled, cx).on_click(pick))
            }
            _ => {
                let values: Vec<Arc<str>> = options.iter().map(|o| Arc::clone(&o.value)).collect();
                row.control(segmented(
                    "web-search",
                    options.iter().map(|option| Arc::clone(&option.label)),
                    selected,
                    cx.listener(move |this, index: &usize, _, cx| {
                        if let Some(value) = values.get(*index) {
                            this.set_web_search(value, cx);
                        }
                    }),
                ))
            }
        })
    }

    fn set_web_search(&mut self, value: &str, cx: &mut Context<Self>) {
        if let Some(editor) = &mut self.provider_setup.editor
            && let Some(mode) = WEB_SEARCH.into_iter().find(|mode| mode.id() == value)
        {
            editor.record.selection.web_search = mode;
            cx.notify();
        }
    }
}

/// How the setup's selected model receives tools discovered mid-turn: a custom endpoint
/// may override the catalog, then the model, then the provider default.
fn tool_discovery(editor: &ProviderDraft, cx: &App) -> ToolDiscoveryMode {
    let definition = &editor.definition;
    let base_url = editor.base_url.read(cx).value();
    let base_url = base_url.trim();
    let custom = !base_url.is_empty() && base_url != definition.default_base_url.as_str();
    if custom && let Some(mode) = definition.custom_endpoint_tool_discovery {
        return mode;
    }
    let model = editor
        .models
        .first()
        .map_or(editor.record.selection.model.as_str(), |model| {
            model.id.as_ref()
        });
    definition
        .models
        .iter()
        .find(|preset| preset.id == model)
        .map_or(definition.tool_discovery, |preset| preset.tool_discovery)
}

/// `current` while it is still offered, else the first effort; `None` without efforts.
fn kept_effort(efforts: &[String], current: Option<&str>) -> Option<String> {
    current
        .filter(|current| efforts.iter().any(|effort| effort == current))
        .or_else(|| efforts.first().map(String::as_str))
        .map(str::to_owned)
}

fn split_entries(input: &str) -> Vec<&str> {
    let mut entries = Vec::new();
    for value in input
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        if !entries.contains(&value) {
            entries.push(value);
        }
    }
    entries
}
