//! The agent settings page and body, and the Identity and System prompt sections.

use super::settings::AgentScope;
use super::*;
use crate::ui::chat::ModelTarget;
use gpui_component::input::NumberInput;
use mobius_gateway::wire::ProviderTint;

/// The system prompt editor's height, or its minimum when it fills the page.
const PROMPT_HEIGHT: Pixels = px(240.);

/// Picker surfaces: a settings page, and the chat side tab.
pub(super) const PAGE: &str = "agent-page";
pub(super) const TAB: &str = "agent-tab";

impl Workspace {
    /// A Bot settings or agent defaults page: one section; status and Save are page actions.
    pub(in crate::ui) fn render_agent_page(
        &mut self,
        scope: AgentScope,
        section: AgentSection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.ensure_draft(&scope, window, cx);
        let subtitle = match &scope {
            AgentScope::Defaults => {
                "The prompt, model, and capabilities new möbius Bots start from.".into()
            }
            AgentScope::Bot(id) => self
                .client
                .state
                .bot(id)
                .map(|bot| format!("{} · @{}", bot.name, bot.handle))
                .unwrap_or_default(),
        };
        let header = PageHeader::new(section.label()).subtitle(subtitle);
        let fill = section == AgentSection::Prompt;
        page(
            SharedString::from(format!("agent:{}:{}", scope.key(), section.label())),
            header,
        )
        .when(fill, Page::fill)
        .child(self.agent_settings_sections(PAGE, &Rc::new(scope), &[section], window, cx))
        .into_any_element()
    }

    /// The agent settings as a self-contained body; its tab toolbar carries status and Save.
    pub(in crate::ui) fn agent_settings_body(
        &mut self,
        scope: AgentScope,
        sections: &[AgentSection],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.ensure_draft(&scope, window, cx);
        self.agent_settings_sections(TAB, &Rc::new(scope), sections, window, cx)
    }

    /// `surface` names where the sections show, so a picker opens only there; its pickers
    /// share `scope`.
    fn agent_settings_sections(
        &self,
        surface: &'static str,
        scope: &Rc<AgentScope>,
        sections: &[AgentSection],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if !self.bots.drafts.contains_key(scope.as_ref()) {
            return self.agent_unavailable(scope).into_any_element();
        }
        let fill = sections == [AgentSection::Prompt];
        let mut body = div()
            .flex()
            .flex_col()
            .gap(space::L)
            .when(fill, |body| body.flex_1().min_h_0());
        for section in sections {
            let content = match section {
                AgentSection::Identity => self.identity_section(scope, window, cx),
                AgentSection::Prompt => self.prompt_section(scope, fill),
                AgentSection::Model => self.model_section(surface, scope, cx),
                AgentSection::Capabilities => self.capabilities_section(surface, scope, cx),
            };
            body = body.child(if sections.len() > 1 {
                Section::new(section.label())
                    .child(content)
                    .into_any_element()
            } else {
                content.into_any_element()
            });
        }
        body.into_any_element()
    }

    fn agent_unavailable(&self, scope: &AgentScope) -> EmptyState {
        let detail = if !self.client.link.is_ready() {
            "Connect to a gateway first."
        } else if matches!(scope, AgentScope::Bot(_)) {
            "This Bot is no longer available."
        } else {
            "Configure a provider first."
        };
        let title = match scope {
            AgentScope::Defaults => "Agent defaults unavailable",
            AgentScope::Bot(_) => "Bot unavailable",
        };
        EmptyState::new("hi.slidersHorizontal", title).detail(detail)
    }

    fn identity_section(
        &self,
        scope: &AgentScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let section = div();
        let (Some(draft), AgentScope::Bot(id)) = (self.bots.drafts.get(scope), scope) else {
            return section;
        };
        let tint = Accent::from(draft.tint);
        let face = lively_face(
            format!("identity:{id}"),
            tint.color(),
            draft.shape,
            BotMood::Idle,
            face::FORM,
            window,
            cx,
        );
        let handle = self
            .client
            .state
            .bot(id)
            .map(|bot| format!("@{}", bot.handle))
            .unwrap_or_default();
        let harness = self.scope_harness(scope).unwrap_or(HarnessKind::Mobius);
        let mut harness_row = Row::new("identity-harness", "Harness")
            .subtitle("Chosen when the Bot was created; it can’t change.")
            .value(crate::ui::pages::harness::name(harness));
        if !crate::ui::pages::harness::available(harness) {
            harness_row = harness_row.status(self.palette.warning, "Not available on this gateway");
        }
        // Each picker's 'static listener needs a scope of its own.
        let (color_scope, shape_scope) = (AgentScope::clone(scope), AgentScope::clone(scope));
        let group = Group::new()
            .footer(
                "Bots are AI agents. They can make mistakes. Review important answers and actions.",
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::L)
                    .px(space::L)
                    .py(space::S)
                    .child(face)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(space::XXS)
                            .child(
                                div()
                                    .typography(Type::Headline)
                                    .truncate()
                                    .child(draft.name.read(cx).value().to_string()),
                            )
                            .child(
                                div()
                                    .typography(Type::Caption)
                                    .text_color(self.palette.muted)
                                    .child(handle),
                            ),
                    ),
            )
            .child(Row::new("identity-color", "Color").control(swatch_picker(
                "identity-swatches",
                tint,
                cx.listener(move |this, accent: &Accent, _, cx| {
                    let tint = ProviderTint::from(*accent);
                    this.update_draft(&color_scope, |draft| draft.tint = tint, cx);
                }),
            )))
            .child(Row::new("identity-shape", "Shape").control(shape_picker(
                "identity-shapes",
                draft.shape,
                tint.color(),
                cx.listener(move |this, shape: &BotShape, _, cx| {
                    this.update_draft(&shape_scope, |draft| draft.shape = *shape, cx);
                }),
            )))
            .child(
                Row::new("identity-name", "Name").control(
                    div()
                        .w(width::FIELD)
                        .child(text_field(&draft.name).aria_label("Bot name")),
                ),
            )
            .child(harness_row);
        // Only a möbius Bot's description is part of its identity.
        section.child(if harness == HarnessKind::Mobius {
            group.child(
                StackedField::new("Operational description")
                    .control(textarea(&draft.description).aria_label("Operational description")),
            )
        } else {
            group
        })
    }

    /// The prompt editor; on its own page it takes the page's spare height.
    fn prompt_section(&self, scope: &AgentScope, fill: bool) -> Div {
        let section = div()
            .flex()
            .flex_col()
            .when(fill, |section| section.flex_1().min_h_0());
        let Some(draft) = self.bots.drafts.get(scope) else {
            return section;
        };
        let editor = div()
            .px(space::S)
            .py(space::XS)
            .map(|editor| {
                if fill {
                    editor.flex_1().min_h(PROMPT_HEIGHT)
                } else {
                    editor.h(PROMPT_HEIGHT)
                }
            })
            .child(
                textarea(&draft.prompt)
                    .aria_label("System prompt")
                    .appearance(false)
                    .h_full(),
            );
        section.child(Group::new().fill(fill).child(editor))
    }

    fn model_section(
        &self,
        surface: &'static str,
        scope: &Rc<AgentScope>,
        cx: &Context<Self>,
    ) -> Div {
        let section = div();
        let Some(draft) = self.bots.drafts.get(scope.as_ref()) else {
            return section;
        };
        let detail = match scope.as_ref() {
            AgentScope::Defaults => {
                "Sets the provider, model, and reasoning inherited by new Bots."
            }
            AgentScope::Bot(_) => "Sets the provider, model, and reasoning used by this Bot.",
        };
        let picker = self.model_popover(surface, ModelTarget::Draft(Rc::clone(scope)), true, cx);
        let group = Group::new().child(
            Row::new("agent-model", "Model")
                .subtitle(detail)
                .control(picker),
        );
        section.child(
            group.child(
                Row::new("agent-steps", "Maximum model steps")
                    .subtitle(
                        "Maximum primary model rounds allowed in one run before möbius stops it.",
                    )
                    .control(NumberInput::new(&draft.steps).w(width::STEPPER)),
            ),
        )
    }
}
