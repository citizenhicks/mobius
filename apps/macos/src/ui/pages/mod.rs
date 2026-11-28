mod background;
pub(super) mod bots;
mod cloud;
pub(in crate::ui) use cloud::SUBSCRIPTIONS;
mod events;
mod extensions;
mod gateway;
pub(super) mod harness;
pub(super) mod memory;
pub(super) mod providers;
mod settings;
mod shortcuts;
mod usage;
mod widgets;

use super::*;
use crate::gateway::Connection;
use gpui_component::menu::DropdownMenu as _;
use gpui_component::{Disableable, WindowExt as _};
use mobius_gateway::wire::ClientMessage;
use std::rc::Rc;

/// Config package UI state; new fields belong here rather than on `Workspace`.
#[derive(Default)]
pub(in crate::ui) struct ConfigUi {
    routing: Option<gateway::RoutingInputs>,
    usage_aggregation: usage::Aggregation,
    /// The endpoint being paired, whose page shows the pairing progress.
    pairing_tick: Option<Task<()>>,
    /// Event id to the revision last read; kept for this session.
    event_reads: BTreeMap<String, String>,
    /// The native Cloud account state, as Swift last reported it.
    cloud: crate::host::platform::CloudStatus,
    /// Whether any Cloud status has arrived; until then the account shows as loading.
    cloud_loaded: bool,
    /// Names the user gave gateways, by gateway label.
    gateway_names: BTreeMap<String, String>,
    shortcuts: shortcuts::ShortcutsUi,
    pub(in crate::ui) color_edits: [Option<settings::ColorEdit>; 2],
    pub(in crate::ui) backdrop: crate::ui::backdrop::BackdropUi,
    /// Where the card grid on screen last painted each card, by `page_key`.
    card_rects: CardRects,
    /// The page the last frame showed, and whether it arrived by a card's zoom.
    shown_page: Option<(PageTab, bool)>,
    zoom: Option<PageZoom>,
}

/// A page in flight between its card and its place.
struct PageZoom {
    flight: CardZoom,
    grid: PageTab,
    page: PageTab,
    /// Renders once more when the flight lands.
    _landing: Task<()>,
}

/// A page's identity in the body, and its card's in a grid.
fn page_key(page: &PageTab) -> SharedString {
    format!("page:{page:?}").into()
}

/// One of a page's own actions in the titlebar.
struct PageAction {
    /// The button's id; the snapshot harness finds the button by it.
    id: SharedString,
    label: &'static str,
    glyph: &'static str,
    tone: ActionTone,
    disabled: bool,
    run: Rc<ActionRun>,
}

/// What a page action does when its button or menu item is clicked.
type ActionRun = dyn Fn(&mut Workspace, &mut Window, &mut Context<Workspace>);

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActionTone {
    Plain,
    Destructive,
    Primary,
}

impl PageAction {
    fn new(
        id: impl Into<SharedString>,
        label: &'static str,
        glyph: &'static str,
        run: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            label,
            glyph,
            tone: ActionTone::Plain,
            disabled: false,
            run: Rc::new(run),
        }
    }

    fn tone(mut self, tone: ActionTone) -> Self {
        self.tone = tone;
        self
    }

    fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

/// The most buttons a page shows in the titlebar; with more, the rest wait in a menu.
const TITLEBAR_ACTIONS: usize = 4;

/// `actions` as the titlebar's buttons and its More menu: everything fits up to
/// `TITLEBAR_ACTIONS`; past it the last three that are not destructive stay, and the menu
/// takes the earlier ones, then the destructive ones.
fn overflow(actions: Vec<PageAction>) -> (Vec<PageAction>, Vec<PageAction>) {
    if actions.len() <= TITLEBAR_ACTIONS {
        return (actions, Vec::new());
    }
    let (destructive, rest): (Vec<_>, Vec<_>) = actions
        .into_iter()
        .partition(|action| action.tone == ActionTone::Destructive);
    let moved = rest.len().saturating_sub(TITLEBAR_ACTIONS - 1);
    let mut rest = rest.into_iter();
    let more = rest.by_ref().take(moved).chain(destructive).collect();
    (rest.collect(), more)
}

impl ConfigUi {
    pub(in crate::ui) fn restored(preferences: &mut super::preferences::Preferences) -> Self {
        Self {
            gateway_names: std::mem::take(&mut preferences.gateway_names),
            backdrop: crate::ui::backdrop::BackdropUi::restored(preferences),
            ..Self::default()
        }
    }
}

/// A pairing or device code on the inset fill.
pub(super) fn code_box(code: &str, p: &Palette) -> Div {
    div()
        .px(space::S)
        .py(space::S)
        .control_corners()
        .bg(p.inset)
        .typography(Type::CodeDisplay)
        .child(code.to_owned())
}

impl Workspace {
    /// The page's own actions for the titlebar's trailing edge, icon-only: destructive
    /// first and the primary action last, as the chat header orders its buttons.
    pub(in crate::ui) fn page_actions(
        &self,
        page: &PageTab,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        match page {
            PageTab::Empty(crate::ui::navigation::Section::Bots) => self
                .first_bot_id()
                .map_or_default(|id| self.bot_page_actions(&id, cx)),
            PageTab::Bot(id) => self.bot_page_actions(id, cx),
            PageTab::BotSettings(id, _) => {
                self.agent_status_actions(&bots::AgentScope::Bot(Arc::clone(id)), cx)
            }
            PageTab::AgentDefaults(_) => self.agent_status_actions(&bots::AgentScope::Defaults, cx),
            PageTab::ProviderPart(id, part) => self.provider_part_actions(id, *part, cx),
            PageTab::Extensions => self
                .first_extension_id()
                .map_or_default(|id| self.extension_actions(&id, cx)),
            PageTab::Extension(id) => self.extension_actions(id, cx),
            PageTab::Gateway(page) => self.gateway_actions(page, cx),
            PageTab::Settings(SettingsPage::Account) => self.account_actions(cx),
            PageTab::Settings(SettingsPage::Shortcuts) => Self::shortcut_actions(cx),
            PageTab::Event(id) => self.event_actions(id, cx),
            PageTab::Contribution(key) if key == memory::PAGE && self.memory.is_open() => {
                self.memory_actions(cx)
            }
            PageTab::Contribution(key) => self.contribution_actions(key, cx),
            PageTab::Draft
            | PageTab::Chat(_)
            | PageTab::Content(_)
            | PageTab::Empty(_)
            | PageTab::Providers
            | PageTab::Provider(_)
            | PageTab::ProviderDraft(_)
            | PageTab::Settings(_) => Vec::new(),
        }
    }

    /// `actions` as titlebar buttons, destructive first in the danger tint and the
    /// primary last. Past `TITLEBAR_ACTIONS`, the destructive ones and the earliest of
    /// the rest move into a trailing More menu, so three stay beside it.
    fn titlebar_actions(&self, actions: Vec<PageAction>, cx: &Context<Self>) -> Vec<AnyElement> {
        let p = self.palette;
        let (shown, more) = overflow(actions);
        let mut buttons: Vec<AnyElement> = shown
            .into_iter()
            .map(|action| {
                let run = action.run;
                let button = toolbar_button(
                    SharedString::clone(&action.id),
                    action.label,
                    action.glyph,
                    cx,
                )
                .disabled(action.disabled)
                .when(!action.disabled, |button| match action.tone {
                    ActionTone::Plain => button,
                    ActionTone::Destructive => button.text_color(p.danger),
                    ActionTone::Primary => button.text_color(p.accent),
                })
                .on_click(cx.listener(move |this, _, window, cx| run(this, window, cx)));
                #[cfg(feature = "snapshot")]
                let button = button.child(crate::ui::snapshot::probe(action.id.to_string()));
                button.into_any_element()
            })
            .collect();
        if !more.is_empty() {
            let view = cx.entity().downgrade();
            buttons.push(
                toolbar_button("page-more", "More", "hi.dotsThree", cx)
                    .dropdown_menu_with_anchor(Anchor::TopRight, move |mut menu, _, cx| {
                        let p = palette(cx);
                        for action in &more {
                            let run = Rc::clone(&action.run);
                            menu = menu.item(
                                workspace_item(
                                    action.label,
                                    action.glyph,
                                    &view,
                                    move |this, window, cx| run(this, window, cx),
                                    &p,
                                )
                                .disabled(action.disabled),
                            );
                        }
                        menu
                    })
                    .on_open_change(
                        cx.listener(|this, open, _, cx| this.overlay_changed(*open, cx)),
                    )
                    .into_any_element(),
            );
        }
        buttons
    }

    /// The grid a card's page zooms out of and back into.
    fn card_grid(&self, page: &PageTab) -> Option<PageTab> {
        match page {
            PageTab::Settings(_) | PageTab::Gateway(GatewayPage::Machine(_)) => {
                Some(PageTab::Empty(crate::ui::navigation::Section::Settings))
            }
            PageTab::ProviderPart(id, _) => self.provider_grid(id),
            _ => None,
        }
    }

    /// The flight between `from` and `to` when one is the other's card grid.
    fn page_zoom(&self, from: &PageTab, to: &PageTab, cx: &Context<Self>) -> Option<PageZoom> {
        let (opening, grid, page) = if self.card_grid(to).as_ref() == Some(from) {
            (true, from, to)
        } else if self.card_grid(from).as_ref() == Some(to) {
            (false, to, from)
        } else {
            return None;
        };
        let landing = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(CardZoom::duration()).await;
            if let Err(error) = this.update(cx, |_, cx| cx.notify()) {
                tracing::trace!(%error, "a card's zoom landed after its window closed");
            }
        });
        Some(PageZoom {
            flight: CardZoom {
                card: page_key(page),
                opening,
                since: motion::now(),
            },
            grid: grid.clone(), // rules-allow: the flight renders both pages while the route keeps its own
            page: page.clone(), // rules-allow: the flight renders both pages while the route keeps its own
            _landing: landing,
        })
    }

    /// The page: zooming out of or into its card, fading in on any other change of
    /// section without shifting its layout. Re-renders keep the same id.
    pub(in crate::ui) fn page_body(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let page = self.page_tabs.get(self.active_page).cloned(); // rules-allow: each page renders through the workspace mutably
        let Some(page) = page else {
            return self.render_page(window, cx);
        };
        let zoomed = match self.config.shown_page.take() {
            Some((shown, arrived)) if shown == page => arrived,
            shown => {
                self.config.zoom = shown
                    .filter(|_| !cx.reduce_motion())
                    .and_then(|(shown, _)| self.page_zoom(&shown, &page, cx));
                self.config.zoom.is_some()
            }
        };
        if self
            .config
            .zoom
            .as_ref()
            .is_some_and(|zoom| zoom.flight.landed())
        {
            self.config.zoom = None;
        }
        let key = page_key(&page);
        let frame = |body: AnyElement| div().size_full().min_h_0().flex().flex_col().child(body);
        let body = self.render_route(page.clone(), window, cx); // rules-allow: the body renders while the frame remembers which page it showed
        self.config.shown_page = Some((page, zoomed));
        let Some(zoom) = self.config.zoom.take() else {
            // A page that flew in is already there; under the fade's id it keeps its state.
            return if zoomed {
                frame(body).id(key).into_any_element()
            } else {
                motion::fade_in(frame(body), key, motion::QUICK, cx)
            };
        };
        let (other, other_key) = if zoom.flight.opening {
            (&zoom.grid, page_key(&zoom.grid))
        } else {
            (&zoom.page, page_key(&zoom.page))
        };
        let other = self.render_route(other.clone(), window, cx); // rules-allow: the flight keeps its page while it renders
        let (other, body) = (frame(other).id(other_key), frame(body).id(key));
        let (grid, card) = if zoom.flight.opening {
            (other, body)
        } else {
            (body, other)
        };
        let layers = zoom.flight.layers(
            grid.into_any_element(),
            card.into_any_element(),
            &self.config.card_rects,
            cx,
        );
        self.config.zoom = Some(zoom);
        layers.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{ActionTone, PageAction, overflow};

    #[test]
    fn a_fifth_action_moves_the_destructive_and_earliest_ones_into_the_menu() {
        let action = |id: &'static str, tone| PageAction::new(id, id, "", |_, _, _| {}).tone(tone);
        let ids = |actions: &[PageAction]| -> Vec<String> {
            actions.iter().map(|action| action.id.to_string()).collect()
        };
        let four = ["forget", "rename", "reconnect", "code"].map(|id| {
            let destructive = id == "forget";
            action(
                id,
                if destructive {
                    ActionTone::Destructive
                } else {
                    ActionTone::Plain
                },
            )
        });
        let (shown, more) = overflow(four.into());
        assert_eq!(ids(&shown), ["forget", "rename", "reconnect", "code"]);
        assert!(more.is_empty(), "four actions fit the titlebar");

        let five = vec![
            action("forget", ActionTone::Destructive),
            action("repair", ActionTone::Plain),
            action("rename", ActionTone::Plain),
            action("reconnect", ActionTone::Plain),
            action("code", ActionTone::Primary),
        ];
        let (shown, more) = overflow(five);
        assert_eq!(ids(&shown), ["rename", "reconnect", "code"]);
        assert_eq!(ids(&more), ["repair", "forget"]);
    }
}
