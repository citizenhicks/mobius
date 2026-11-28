//! Native controls bind opaque gateway field IDs to an unsaved provider group.
use super::*;
use gpui_component::input::InputEvent;
use mobius::protocol::{FrontendSettingKind, FrontendSettingValue};
use mobius_gateway::wire::{
    ProviderField, ProviderGroupDraft, ProviderModelDraft, ProviderModelGroup, ProviderModelKind,
};

pub(super) fn input(
    value: &str,
    kind: Option<ProviderModelKind>,
    masked: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> (Entity<InputState>, Subscription) {
    let field = cx.new(|cx| {
        InputState::new(window, cx)
            .masked(masked)
            .default_value(value.to_owned())
    });
    let subscription = cx.subscribe(&field, move |this, _, event: &InputEvent, cx| {
        if matches!(event, InputEvent::Change)
            && let Some(editor) = this.provider_setup.editor.as_mut()
        {
            for group in &mut editor.groups {
                if kind.is_none_or(|kind| kind == group.schema.kind) {
                    group.test = GroupTest::Idle;
                }
            }
            cx.notify();
        }
    });
    (field, subscription)
}

pub(super) enum GroupTest {
    Idle,
    Pending(Arc<str>),
    Passed,
    Failed(Arc<str>),
}
pub(super) struct Field {
    schema: ProviderField,
    input: Entity<InputState>,
    _subscription: Subscription,
}
fn field_text(value: &FrontendSettingValue) -> String {
    match value {
        FrontendSettingValue::String(text) => text.clone(), // rules-allow: native fields own the text they display
        FrontendSettingValue::Integer(number) => number.to_string(),
    }
}
impl Field {
    fn new(
        schema: &ProviderField,
        kind: ProviderModelKind,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Self {
        let text = field_text(&schema.value);
        let (input, subscription) = input(&text, Some(kind), false, window, cx);
        Self {
            schema: schema.clone(), // rules-allow: the draft retains its own copy of the gateway schema
            input,
            _subscription: subscription,
        }
    }
    fn value(&self, cx: &App) -> Result<FrontendSettingValue, String> {
        let value = self.input.read(cx).value();
        match &self.schema.setting.kind {
            FrontendSettingKind::Integer { .. } => value
                .trim()
                .parse()
                .map(FrontendSettingValue::Integer)
                .map_err(|_invalid| {
                    format!("Enter a whole number for {}.", self.schema.setting.label)
                }),
            _ => Ok(FrontendSettingValue::String(value.to_string())),
        }
    }
    fn row(&self, key: &str, siblings: &[Self], group: &[Self], cx: &Context<Workspace>) -> Row {
        let setting = &self.schema.setting;
        let row = Row::new(
            SharedString::from(format!("{key}:{}", setting.id)),
            setting.label.as_str(),
        );
        let row = if setting.description.is_empty() {
            row
        } else {
            row.subtitle(setting.description.as_str())
        };
        self.shown(key, siblings, group, cx).trailing(row)
    }
    /// The field's value, or the control that edits it.
    fn shown(
        &self,
        key: &str,
        siblings: &[Self],
        group: &[Self],
        cx: &Context<Workspace>,
    ) -> Shown {
        let setting = &self.schema.setting;
        let id = SharedString::from(format!("{key}:{}", setting.id));
        let value = self.input.read(cx).value();
        if let FrontendSettingKind::Select {
            options,
            unset_label,
        } = &setting.kind
        {
            let choices = unset_label
                .iter()
                .map(|label| ("", label.as_str()))
                .chain(options.iter().map(|o| (o.value.as_ref(), o.label.as_ref())));
            let selected = options
                .iter()
                .find(|o| o.value.as_ref() == value.as_str())
                .map_or_else(
                    || unset_label.as_deref().unwrap_or(value.as_str()),
                    |o| o.label.as_ref(),
                );
            if !self.schema.editable {
                return Shown::Value(selected.to_owned().into());
            }
            let choices = choices
                .map(|(v, label)| {
                    let field = Entity::clone(&self.input);
                    let updates: Vec<_> = self
                        .schema
                        .on_change
                        .get(v)
                        .into_iter()
                        .flatten()
                        .filter_map(|(id, value)| {
                            let target = siblings.iter().find(|f| f.schema.setting.id == *id)?;
                            Some((
                                Entity::clone(&target.input),
                                SharedString::from(field_text(value)),
                            ))
                        })
                        .collect();
                    let v = SharedString::from(v);
                    MenuChoice::new(
                        label,
                        v == value,
                        cx.listener(move |_, _, window, cx| {
                            for (target, value) in &updates {
                                target.update(cx, |input, cx| {
                                    input.set_value(SharedString::clone(value), window, cx);
                                });
                            }
                            field.update(cx, |input, cx| {
                                input.set_value(SharedString::clone(&v), window, cx);
                            });
                        }),
                    )
                })
                .collect();
            return Shown::Control(menu_select(id, selected, None, choices, cx).into_any_element());
        }
        if !self.schema.editable {
            if let Some(display) = self.schema.display_override(|key, expected| {
                group
                    .iter()
                    .find(|f| f.schema.setting.id == key)
                    .is_some_and(|f| f.input.read(cx).value().as_str() == expected)
            }) {
                return Shown::Value(field_text(display).into());
            }
            return Shown::Value(value);
        }
        let control = div().relative().child(
            text_field(&self.input)
                .aria_label(setting.label.as_str())
                .w(width::FIELD),
        );
        #[cfg(feature = "snapshot")]
        let control = control.child(crate::ui::snapshot::probe(id.to_string()));
        Shown::Control(control.into_any_element())
    }
}
/// What a field puts in its row's trailing slot.
enum Shown {
    Value(SharedString),
    Control(AnyElement),
}
impl Shown {
    fn trailing(self, row: Row) -> Row {
        match self {
            Self::Value(value) => row.value(value),
            Self::Control(control) => row.control(control),
        }
    }
}
fn values(fields: &[Field], cx: &App) -> Result<BTreeMap<String, FrontendSettingValue>, String> {
    fields
        .iter()
        .map(|f| Ok((f.schema.setting.id.clone(), f.value(cx)?))) // rules-allow: submitted field maps own their opaque keys
        .collect()
}

pub(super) struct ModelRow {
    id: Entity<InputState>,
    _change: Subscription,
    template: Option<usize>,
    fields: Vec<Field>,
    /// Added here and not saved yet, so its id is still typed in.
    fresh: bool,
    pub open: bool,
}
impl ModelRow {
    fn new(
        id: &str,
        schema: &ProviderModelGroup,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Self {
        let kind = schema.kind;
        let (template, fields) = Self::template(id, schema);
        let input = cx.new(|cx| InputState::new(window, cx).default_value(id.to_owned()));
        let change = cx.subscribe_in(
            &input,
            window,
            move |this, input, event: &InputEvent, window, cx| {
                if !matches!(event, InputEvent::Change) {
                    return;
                }
                if let Some(group) = this.provider_group_mut(kind)
                    && let Some(model) = group.models.iter_mut().find(|model| model.id == *input)
                {
                    group.test = GroupTest::Idle;
                    let name = input.read(cx).value();
                    let (template, fields) = Self::template(name.trim(), &group.schema);
                    if model.template != template {
                        model.template = template;
                        model.fields = fields
                            .iter()
                            .map(|schema| {
                                let field = Field::new(schema, kind, window, cx);
                                if schema.editable
                                    && let Some(old) = model.fields.iter().find(|old| {
                                        old.schema.setting.id == schema.setting.id
                                            && old.schema.editable
                                            && old.value(cx).as_ref() != Ok(&old.schema.value)
                                    })
                                {
                                    let value = old.input.read(cx).value();
                                    field
                                        .input
                                        .update(cx, |input, cx| input.set_value(value, window, cx));
                                }
                                field
                            })
                            .collect();
                    }
                    cx.notify();
                }
            },
        );
        Self {
            id: input,
            _change: change,
            template,
            fields: fields
                .iter()
                .map(|field| Field::new(field, kind, window, cx))
                .collect(),
            fresh: false,
            open: false,
        }
    }
    fn template<'a>(
        id: &str,
        group: &'a ProviderModelGroup,
    ) -> (Option<usize>, &'a [ProviderField]) {
        group
            .models
            .iter()
            .chain(&group.model_presets)
            .enumerate()
            .find(|(_, m)| m.id == id)
            .map_or((None, group.new_model_fields.as_slice()), |(index, m)| {
                (Some(index), m.fields.as_slice())
            })
    }
}
pub(super) struct ModelGroup {
    pub schema: ProviderModelGroup,
    pub models: Vec<ModelRow>,
    fields: Vec<Field>,
    pub test: GroupTest,
}
impl ModelGroup {
    pub(super) fn new(
        schema: &ProviderModelGroup,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Self {
        let kind = schema.kind;
        Self {
            fields: schema
                .fields
                .iter()
                .map(|field| Field::new(field, kind, window, cx))
                .collect(),
            models: schema
                .models
                .iter()
                .map(|m| ModelRow::new(&m.id, schema, window, cx))
                .collect(),
            schema: schema.clone(), // rules-allow: the draft retains its own copy of the gateway schema
            test: GroupTest::Idle,
        }
    }
    pub fn add(&mut self, id: &str, window: &mut Window, cx: &mut Context<Workspace>) {
        let mut model = ModelRow::new(id, &self.schema, window, cx);
        model.fresh = true;
        model.open = true;
        model.id.update(cx, |input, cx| input.focus(window, cx));
        self.models.push(model);
        self.test = GroupTest::Idle;
    }
    pub(super) fn draft(&self, cx: &App) -> Result<ProviderGroupDraft, String> {
        Ok(ProviderGroupDraft {
            values: values(&self.fields, cx)?,
            models: self
                .models
                .iter()
                .map(|model| {
                    Ok(ProviderModelDraft {
                        id: model.id.read(cx).value().trim().to_owned(),
                        values: values(&model.fields, cx)?,
                    })
                })
                .collect::<Result<_, String>>()?,
        })
    }
    pub fn dirty(&self, cx: &App) -> bool {
        let changed = |field: &Field| field.value(cx).as_ref() != Ok(&field.schema.value);
        self.fields.iter().any(changed)
            || self.models.len() != self.schema.models.len()
            || self
                .models
                .iter()
                .zip(&self.schema.models)
                .any(|(model, stored)| {
                    model.id.read(cx).value().as_str() != stored.id
                        || model.fields.iter().any(changed)
                })
    }
    /// "3 models", as the group's card and page header say it.
    pub fn summary(&self) -> String {
        match self.models.len() {
            _ if !self.schema.supported => "Not supported".into(),
            0 => "Not set up".into(),
            1 => "1 model".into(),
            count => format!("{count} models"),
        }
    }
    /// The group's own fields, one row each.
    pub fn field_rows(&self, cx: &Context<Workspace>) -> Vec<Row> {
        let key = format!("provider:{:?}", self.schema.kind);
        self.fields
            .iter()
            .map(|field| field.row(&key, &self.fields, &self.fields, cx))
            .collect()
    }
    /// One row per model: its id, its one setting or a caret onto several, and Remove.
    pub fn model_rows(&self, cx: &Context<Workspace>) -> Vec<Row> {
        let kind = self.schema.kind;
        let mut rows = Vec::new();
        for (index, model) in self.models.iter().enumerate() {
            let key = format!("provider:{kind:?}:model:{index}");
            let id = SharedString::from(key.as_str());
            let mut row = if model.fresh {
                Row::new(id, "New model")
                    .control(text_field(&model.id).aria_label("Model ID").w(width::FIELD))
            } else {
                Row::new(id, model.id.read(cx).value()).mono_title()
            };
            match model.fields.as_slice() {
                [] => {}
                [field] => {
                    row = field
                        .shown(&key, &model.fields, &self.fields, cx)
                        .trailing(row);
                }
                _ => {
                    let caret = inline_button(
                        SharedString::from(format!("{key}:settings")),
                        "Model settings",
                        chevron(model.open, &palette(cx)),
                        cx,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(model) = this
                            .provider_group_mut(kind)
                            .and_then(|g| g.models.get_mut(index))
                        {
                            model.open = !model.open;
                            cx.notify();
                        }
                    }));
                    #[cfg(feature = "snapshot")]
                    let caret = caret.child(crate::ui::snapshot::probe(format!("{key}:settings")));
                    row = row.control(caret);
                }
            }
            if self.schema.editable {
                let remove = inline_button(
                    SharedString::from(format!("{key}:remove")),
                    "Remove model",
                    "hi.trash",
                    cx,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(group) = this.provider_group_mut(kind) {
                        group.models.remove(index);
                        group.test = GroupTest::Idle;
                        cx.notify();
                    }
                }));
                #[cfg(feature = "snapshot")]
                let remove = remove.child(crate::ui::snapshot::probe(format!("{key}:remove")));
                row = row.control(remove);
            }
            rows.push(row);
            if model.open && model.fields.len() > 1 {
                rows.extend(
                    model
                        .fields
                        .iter()
                        .map(|field| field.row(&key, &model.fields, &self.fields, cx)),
                );
            }
        }
        rows
    }
}
