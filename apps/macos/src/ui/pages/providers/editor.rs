//! A provider's shared settings and its gateway-defined form.
use super::fields::{GroupTest, ModelGroup, input};
use super::*;
use mobius::backend::model::provider::BaseUrl;
use mobius_gateway::wire::{ProviderForm, ProviderModelKind};

pub(super) struct ProviderDraft {
    pub selection: ProviderConfig,
    tint: ProviderTint,
    operator_managed: bool,
    pub definition: ProviderStatus,
    pub is_new: bool,
    name: Entity<InputState>,
    base_url: Entity<InputState>,
    pub api_key: Entity<InputState>,
    pub groups: Vec<ModelGroup>,
    pub saving: Option<(Arc<str>, ProviderForm)>,
    _subscriptions: [Subscription; 3],
}

impl ProviderDraft {
    pub fn new(
        record: &ProviderInstance,
        definition: ProviderStatus,
        is_new: bool,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Self {
        let (name, n) = input(&record.label, None, false, window, cx);
        let (base_url, b) = input(
            record
                .selection
                .base_url
                .as_deref()
                .unwrap_or(&definition.default_base_url),
            None,
            false,
            window,
            cx,
        );
        let (api_key, a) = input("", None, true, window, cx);
        let groups = record
            .model_groups
            .iter()
            .map(|g| ModelGroup::new(g, window, cx))
            .collect();
        Self {
            selection: record.selection.clone(), // rules-allow: the editable connection outlives the gateway view
            tint: record.tint,
            operator_managed: record.operator_managed,
            definition,
            is_new,
            name,
            base_url,
            api_key,
            groups,
            saving: None,
            _subscriptions: [n, b, a],
        }
    }
    pub fn reload(
        &mut self,
        record: &ProviderInstance,
        submitted: ProviderConfig,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let groups = record
            .model_groups
            .iter()
            .map(|schema| {
                let mut group = ModelGroup::new(schema, window, cx);
                if let Some(old) = self
                    .groups
                    .iter_mut()
                    .find(|g| g.schema.kind == schema.kind)
                {
                    group.test = std::mem::replace(&mut old.test, GroupTest::Idle);
                    for (model, previous) in group.models.iter_mut().zip(&old.models) {
                        model.open = previous.open;
                    }
                }
                group
            })
            .collect();
        self.groups = groups;
        self.selection = if submitted == record.selection {
            submitted
        } else {
            record.selection.clone() // rules-allow: retain the gateway-normalized model selection when it differs from the submitted draft
        };
        self.tint = record.tint;
        self.operator_managed = record.operator_managed;
        self.saving = None;
    }
    pub(super) fn form(
        &self,
        cx: &App,
        include: impl Fn(&ModelGroup) -> bool,
    ) -> Result<ProviderForm, String> {
        let mut config = self.selection.clone(); // rules-allow: each form submission owns its copy of the draft identity
        let url = self.base_url.read(cx).value();
        if !self.operator_managed {
            config.base_url = (!url.trim().is_empty())
                .then(|| BaseUrl::new(url.trim()))
                .transpose()
                .map_err(|e| e.to_string())?;
        }
        let groups = self
            .groups
            .iter()
            .filter(|g| g.schema.supported && include(g))
            .map(|g| Ok((g.schema.kind, g.draft(cx)?)))
            .collect::<Result<_, String>>()?;
        Ok(ProviderForm {
            config,
            label: Some(self.name.read(cx).value().trim().to_owned()),
            tint: Some(self.tint),
            groups,
        })
    }
    pub fn can_register(&self, cx: &App) -> bool {
        !self.name.read(cx).value().trim().is_empty()
    }
    pub fn registration(&self, cx: &App) -> Result<Option<ClientMessage>, String> {
        if !self.can_register(cx) {
            return Ok(None);
        }
        Ok(Some(ClientMessage::SaveProviderForm {
            request_id: crate::gateway::request_id(),
            form: self.form(cx, |g| g.dirty(cx))?,
        }))
    }
    fn is_dirty(&self, live: &ProviderInstance, cx: &App) -> bool {
        self.name.read(cx).value().trim() != live.label
            || self.tint != live.tint
            || self.base_url.read(cx).value().trim()
                != live
                    .selection
                    .base_url
                    .as_deref()
                    .unwrap_or(&self.definition.default_base_url)
            || self.groups.iter().any(|group| group.dirty(cx))
    }
    /// The card and page header line: the service and how its credential stands.
    fn summary(&self, live: Option<&ProviderInstance>) -> String {
        let credential = match live {
            None => "New setup".to_owned(),
            Some(live) => match &live.credential_hint {
                Some(hint) => format!("Key ••••{hint}"),
                None if live.configured => "Configured on gateway".to_owned(),
                None => "Not configured".to_owned(),
            },
        };
        format!("{} · {credential}", self.definition.label)
    }
    fn group(&self, kind: ProviderModelKind) -> Option<&ModelGroup> {
        self.groups.iter().find(|group| group.schema.kind == kind)
    }
    pub fn test_response(&mut self, msg: &ServerMessage) {
        let (id, result) = match msg {
            ServerMessage::ProviderGroupTested { request_id } => (request_id, GroupTest::Passed),
            ServerMessage::Rejected {
                request_id,
                message,
                ..
            } => (request_id, GroupTest::Failed(Arc::clone(message))),
            _ => return,
        };
        if let Some(group) = self
            .groups
            .iter_mut()
            .find(|g| matches!(&g.test,GroupTest::Pending(pending) if pending==id))
        {
            group.test = result;
        }
    }
}
impl Workspace {
    pub(super) fn provider_group_mut(
        &mut self,
        kind: ProviderModelKind,
    ) -> Option<&mut ModelGroup> {
        self.provider_setup
            .editor
            .as_mut()?
            .groups
            .iter_mut()
            .find(|group| group.schema.kind == kind)
    }
    #[cfg(feature = "snapshot")]
    pub(in crate::ui) fn snapshot_provider_test(&self, kind: ProviderModelKind) -> &str {
        match self
            .provider_setup
            .editor
            .as_ref()
            .and_then(|e| e.group(kind))
            .map(|g| &g.test)
        {
            Some(GroupTest::Failed(_)) => "failed",
            Some(GroupTest::Passed) => "passed",
            Some(GroupTest::Pending(_)) => "pending",
            _ => "idle",
        }
    }
    /// Shows `kind`'s test as finished, without asking the gateway.
    #[cfg(feature = "snapshot")]
    pub(in crate::ui) fn snapshot_group_tested(
        &mut self,
        kind: ProviderModelKind,
        error: Option<&str>,
    ) {
        if let Some(group) = self.provider_group_mut(kind) {
            group.test = error.map_or(GroupTest::Passed, |error| GroupTest::Failed(error.into()));
        }
    }
    #[cfg(feature = "snapshot")]
    pub(in crate::ui) fn snapshot_provider_draft(&self, cx: &App) -> Option<ProviderForm> {
        self.provider_setup.editor.as_ref()?.form(cx, |_| true).ok()
    }
    /// Tests the group's unsaved form. The result shows in the titlebar and on the
    /// group's card; a failure's detail is a toast, so the page never moves.
    fn test_provider_group(&mut self, kind: ProviderModelKind, cx: &mut Context<Self>) {
        let Some(editor) = self.provider_setup.editor.as_ref() else {
            return;
        };
        let api_key = editor.api_key.read(cx).value();
        let api_key = (!api_key.trim().is_empty()).then(|| api_key.trim().to_owned());
        let form = editor.form(cx, |g| g.schema.kind == kind);
        let Some(group) = self.provider_group_mut(kind) else {
            return;
        };
        let form = match form {
            Ok(form) => form,
            Err(error) => {
                group.test = GroupTest::Failed(error.as_str().into());
                self.report(error);
                cx.notify();
                return;
            }
        };
        let request_id = crate::gateway::request_id();
        group.test = GroupTest::Pending(Arc::clone(&request_id));
        self.provider_setup
            .group_tests
            .insert(Arc::clone(&request_id));
        self.send(
            ClientMessage::TestProviderGroup {
                request_id,
                form,
                group: kind,
                api_key,
            },
            cx,
        );
    }
    /// The grid `id`'s cards open from: the saved setup's page, or the new one's.
    pub(in crate::ui) fn provider_grid(&self, id: &str) -> Option<PageTab> {
        let editor = self.provider_setup.editor.as_ref()?;
        (editor.selection.instance == id).then(|| {
            if editor.is_new {
                PageTab::ProviderDraft(editor.definition.provider.clone()) // rules-allow: the page tab owns its id
            } else {
                PageTab::Provider(editor.selection.instance.clone()) // rules-allow: the page tab owns its id
            }
        })
    }
    /// The setup's page: General and one card per model group.
    pub(super) fn provider_editor(&self, cx: &Context<Self>) -> AnyElement {
        let Some(editor) = &self.provider_setup.editor else {
            return self.provider_unavailable();
        };
        let id = &editor.selection.instance;
        let live = self.provider_record(id).filter(|_| !editor.is_new);
        let title = live.map_or(editor.definition.label.as_str(), |r| r.label.as_str());
        let status = live.map_or("New setup", |r| {
            if r.configured {
                "Configured on gateway"
            } else {
                "Not configured"
            }
        });
        let card = |part: ProviderPart, title: &str, glyph: &'static str, summary: String| {
            let page = PageTab::ProviderPart(id.clone(), part); // rules-allow: the page tab owns its id
            Card::new(page_key(&page), title.to_owned(), glyph, summary)
                .rects(&self.config.card_rects)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_page(page.clone(), window, cx); // rules-allow: the click handler is Fn and each open owns its page
                }))
        };
        let general = &editor.definition.general_section;
        let mut grid = CardGrid::new().card(card(
            ProviderPart::General,
            &general.label,
            symbol_icon_or(Some(&general.symbol), "hi.slidersHorizontal"),
            editor.summary(live),
        ));
        for group in &editor.groups {
            let schema = &group.schema;
            let mark = match group.test {
                GroupTest::Passed => Some(CardMark::Tested { passed: true }),
                GroupTest::Failed(_) => Some(CardMark::Tested { passed: false }),
                GroupTest::Idle | GroupTest::Pending(_) => None,
            };
            grid = grid.card(
                card(
                    ProviderPart::Models(schema.kind),
                    &schema.label,
                    symbol_icon_or(Some(&schema.symbol), "hi.slidersHorizontal"),
                    group.summary(),
                )
                .mark(mark)
                .disabled(!schema.supported),
            );
        }
        page(
            SharedString::from(format!("provider:{id}")),
            PageHeader::new(title).subtitle(format!("{} · {status}", editor.definition.label)),
        )
        .child(grid)
        .into_any_element()
    }
    /// One card's page of the setup `id`, its editor built from the live record on
    /// first sight.
    pub(in crate::ui) fn render_provider_part(
        &mut self,
        id: &str,
        part: ProviderPart,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self.provider_grid(id).is_none() && !self.open_provider_editor(id, window, cx) {
            return self.provider_unavailable();
        }
        let Some(editor) = &self.provider_setup.editor else {
            return self.provider_unavailable();
        };
        let live = self.provider_record(id).filter(|_| !editor.is_new);
        match part {
            ProviderPart::General => {
                let general = Self::provider_setup_group(editor, cx);
                let general = self
                    .provider_credential(general, editor, live, cx)
                    .child(Self::provider_colour_row(editor, cx));
                page(
                    SharedString::from(format!("provider:{id}:general")),
                    PageHeader::new(editor.definition.general_section.label.as_str())
                        .subtitle(editor.summary(live)),
                )
                .children(self.device_code_group())
                .child(general)
                .into_any_element()
            }
            ProviderPart::Models(kind) => {
                let Some(group) = editor.group(kind) else {
                    return self.provider_unavailable();
                };
                let schema = &group.schema;
                let mut page = page(
                    SharedString::from(format!("provider:{id}:{kind:?}")),
                    PageHeader::new(schema.label.as_str()).subtitle(group.summary()),
                );
                let fields = group.field_rows(cx);
                if !fields.is_empty() {
                    page =
                        page.child(Section::new("Endpoint").child(Group::new().children(fields)));
                }
                let mut models = Group::new().children(group.model_rows(cx));
                if !schema.supported {
                    models = models.child(Row::empty(
                        "provider-models-unsupported",
                        "Not supported by this provider",
                    ));
                } else if group.models.is_empty() && schema.editable {
                    models = models.child(Row::empty(
                        "provider-models-empty",
                        "No models yet. Add one from the titlebar.",
                    ));
                } else if group.models.is_empty() {
                    models = models.child(Row::empty("provider-models-empty", "No models"));
                }
                let mut section = Section::new("Models");
                if let Some(scope) = &schema.test_description {
                    section = section.description(scope.to_string());
                }
                page.child(section.child(models)).into_any_element()
            }
        }
    }
    /// A card page's titlebar: a group's test status, then Add model and Test, or Remove
    /// provider on General; Save last on both.
    pub(in crate::ui::pages) fn provider_part_actions(
        &self,
        id: &str,
        part: ProviderPart,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(editor) = self
            .provider_setup
            .editor
            .as_ref()
            .filter(|editor| editor.selection.instance == id)
        else {
            return Vec::new();
        };
        let ready = self.client.link.is_ready();
        let live = self.provider_record(id).filter(|_| !editor.is_new);
        let key_entered = !editor.api_key.read(cx).value().trim().is_empty();
        let dirty = live.is_none_or(|live| editor.is_dirty(live, cx));
        let can_save = ready && editor.can_register(cx) && (dirty || key_entered);
        let mut status = None;
        let mut actions = Vec::new();
        match part {
            ProviderPart::General if live.is_some() => actions.push(
                PageAction::new(
                    "remove-provider",
                    "Remove provider…",
                    "hi.trash",
                    |this, window, cx| this.remove_provider_dialog(window, cx),
                )
                .tone(ActionTone::Destructive)
                .disabled(!ready),
            ),
            ProviderPart::General => {}
            ProviderPart::Models(kind) => {
                let Some(group) = editor.group(kind).filter(|group| group.schema.supported) else {
                    return Vec::new();
                };
                status = self.test_status(&group.test, cx);
                if group.schema.editable {
                    actions.push(PageAction::new(
                        format!("provider:{kind:?}:add-model"),
                        "Add model",
                        "hi.plus",
                        move |this, window, cx| {
                            if let Some(group) = this.provider_group_mut(kind) {
                                group.add("", window, cx);
                                cx.notify();
                            }
                        },
                    ));
                }
                if group.schema.test_description.is_some() {
                    actions.push(
                        PageAction::new(
                            format!("test:{kind:?}"),
                            "Test",
                            "hi.playFill",
                            move |this, _, cx| this.test_provider_group(kind, cx),
                        )
                        .disabled(!ready || matches!(group.test, GroupTest::Pending(_))),
                    );
                }
            }
        }
        actions.push(
            PageAction::new("provider-save", "Save", "hi.check", |this, window, cx| {
                this.save_provider(window, cx);
            })
            .tone(ActionTone::Primary)
            .disabled(!can_save),
        );
        let mut bar: Vec<AnyElement> = status.into_iter().collect();
        bar.extend(self.titlebar_actions(actions, cx));
        bar
    }
    /// A test's state to the left of the page's actions: a spinner while it runs, then a
    /// dot and a word, the failure's detail in its tooltip.
    fn test_status(&self, test: &GroupTest, cx: &Context<Self>) -> Option<AnyElement> {
        let p = self.palette;
        let (mark, word, detail) = match test {
            GroupTest::Idle => return None,
            GroupTest::Pending(_) => (
                Spinner::new(glyph::INLINE)
                    .color(p.muted)
                    .into_any_element(),
                "Testing",
                None,
            ),
            GroupTest::Passed => (status_dot(p.signal).into_any_element(), "Passed", None),
            GroupTest::Failed(error) => (
                status_dot(p.danger).into_any_element(),
                "Failed",
                Some(SharedString::from(Arc::clone(error))),
            ),
        };
        let status = div()
            .id("provider-test-status")
            .flex()
            .items_center()
            .flex_shrink_0()
            .gap(space::XS)
            .mr(space::XS)
            .typography(Type::Caption)
            .text_color(p.muted)
            .whitespace_nowrap()
            .child(mark)
            .child(word)
            .when_some(detail, |status, detail| {
                status.tooltip(text_tooltip(detail))
            });
        Some(motion::fade_in(status, word, motion::QUICK, cx))
    }

    /// Name and service side by side, then the endpoint.
    fn provider_setup_group(editor: &ProviderDraft, cx: &Context<Self>) -> Group {
        let glyph = symbol_icon_or(Some(&editor.definition.symbol), "hi.hardDrives");
        let service = div()
            .h(height::REGULAR)
            .flex()
            .items_center()
            .gap(space::S)
            .typography(Type::Body)
            .child(icon(glyph, &palette(cx)).size(glyph::LEAD))
            .child(SharedString::from(editor.definition.label.as_str()));
        let name = text_field(&editor.name);
        let base = text_field(&editor.base_url).disabled(editor.operator_managed);
        #[cfg(feature = "snapshot")]
        let name = div()
            .relative()
            .child(name)
            .child(crate::ui::snapshot::probe("provider-name".into()));
        #[cfg(feature = "snapshot")]
        let base = div()
            .relative()
            .child(base)
            .child(crate::ui::snapshot::probe("provider-base".into()));
        let half = || div().flex_1().min_w_0();
        Group::new()
            .child(
                div()
                    .flex()
                    .child(
                        half().child(
                            StackedField::new("Name")
                                .help("Shown in model pickers and usage.")
                                .control(name),
                        ),
                    )
                    // The two fields' own insets meet here; this leaves one gap between them.
                    .child(
                        half()
                            .ml(-(space::L + space::S))
                            .child(StackedField::new("Provider").control(service)),
                    ),
            )
            .child(StackedField::new("Base URL").control(base))
    }

    fn provider_colour_row(editor: &ProviderDraft, cx: &Context<Self>) -> Row {
        Row::new("provider-colour", "Colour").control(swatch_picker(
            "provider-tint",
            Accent::from(editor.tint),
            cx.listener(|this, accent: &Accent, _, cx| {
                if let Some(editor) = &mut this.provider_setup.editor {
                    editor.tint = (*accent).into();
                    cx.notify();
                }
            }),
        ))
    }
}
