mod backdrop;
mod chat;
mod chrome;
mod diff_document;
mod layout;
mod navigation;
mod pages;
mod panel;
mod preferences;
mod primitives;
mod region;
mod router;
mod runtime;
#[cfg(feature = "snapshot")]
mod snapshot;
mod storage;
pub mod theme;
mod transcript;
pub use navigation::install_menus;
pub(crate) use primitives::FontSet;

use crate::gateway::{Gateway, GatewayState};
use gpui::prelude::*;
use gpui::{
    Action, Anchor, AnyElement, App, AppContext, Background, Bounds, BoxShadow, ClickEvent,
    ClipboardEntry, ClipboardItem, Corners, DefiniteLength, DevicePixels, DispatchPhase, Div,
    ElementId, Entity, ExternalPaths, FocusHandle, Focusable, FontWeight, Global, HighlightStyle,
    Hsla, Img, ImgResourceLoader, KeyBinding, KeyBindingContextPredicate, KeyDownEvent, Keystroke,
    Menu, MenuItem, Modifiers, ModifiersChangedEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, NavigationDirection, ObjectFit, OsAction, PathPromptOptions, Pixels, Point,
    Radians, RenderImage, Resource, Result, Rgba, Role, ScrollEvent, ScrollHandle, ScrollStrategy,
    SharedString, Size, Stateful, StyleRefinement, StyledText, Subscription, Task, TextRun,
    WeakEntity, Window, WindowBackgroundAppearance, WindowControlArea, actions, black, canvas,
    container_query, div, fill, font, hsla, img, linear_color_stop, linear_gradient, point, px,
    relative, rgb, size, transparent_black, white,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{InputEvent, InputState, TextareaState};
use gpui_component::{ResizableState, Sizable, WindowExt};
use mobius::SessionId;
#[cfg(feature = "snapshot")]
use primitives::shape_picker;
use primitives::{
    BotFace, BotFaceView, BotMood, EditKind, EmptyState, Group, Handler, Leading, MenuChoice,
    Notice, Page, PageHeader, RailGlyph, Row, Section, Shape, SidebarRow, Spinner, StackedField,
    TextEdit, ToggleExt, Tone, Type, Typography, accent_tile_picker, ago, alpha,
    apple_toolbar_button, back_icon, button, button_row, byte_count, chat_menu_trigger, chevron,
    clock_duration, compact_count, compact_duration, composer_button, counted, dialog_actions,
    dialog_body, face, field, filled_glyph, first_line, forward_icon, gauge_button, glyph,
    hairline, heading, height, icon, inline_button, inline_code_ring, inset, known_symbol_icon,
    labelled, leading_item, lively_face, loading_rows, markdown_style, menu_select, mini_button,
    motion, organic, page, palette, percent, pill, pill_quad, preview_glyph, primary_button,
    radius, rail_button, rail_section_button, row_actions_inset, row_button, segmented,
    settings_button, show_more_row, sidebar_header, sidebar_label, sidebar_note, single_line,
    space, status_dot, status_label, swatch_picker, symbol_icon_or, text_field, text_tooltip,
    textarea, toggle, toolbar_button, unavailable, width, workspace_item,
};
use router::{
    AgentSection, GatewayPage, GatewayRef, PageTab, SettingsPage, activate_page, host, is_cloud,
    selected_after_close,
};
use runtime::LinkTone;
use std::collections::BTreeMap;
use std::sync::Arc;
use theme::{Accent, Appearance, Palette};

/// One companion-pane document (spec §1.10); `panel` renders each kind.
#[derive(Clone)]
enum TabContent {
    /// A gateway request in flight.
    Loading {
        request: Arc<str>,
        /// The gateway session for this request; local reads have none.
        session_id: Option<SessionId>,
    },
    Failed(String),
    Code(panel::CodeDoc),
    Diff(panel::DiffDoc),
    Files(Box<panel::FilesDoc>),
    /// A gateway-rendered subagent or voice transcript.
    Preview {
        session_id: SessionId,
        preview: std::rc::Rc<crate::gateway::Preview>,
    },
    Routine(std::rc::Rc<panel::RoutineDoc>),
    /// The new-tab launcher: tools and this chat's transcripts.
    Launcher,
    /// A local file shown by QuickLook.
    File(panel::FileDoc),
    /// A login shell in the chat's folder.
    Terminal(panel::TerminalDoc),
    /// The chat's agent browser, streamed from a local gateway; its connection lives in
    /// `panel::ContentState`.
    AgentBrowser,
    /// A page in the app's own browser; its Chromium page lives in `panel::ContentState`.
    Browser,
    /// The remote gateway's desktop, rendered by stock noVNC in its own CEF page.
    Desktop,
    /// A chat's Bot settings or widget content, opened from its pill.
    Detail {
        detail: chat::ChatDetail,
        glyph: &'static str,
    },
}

#[derive(Clone)]
struct ContentTab {
    id: Arc<str>,
    title: SharedString,
    content: TabContent,
}

actions!(
    workspace,
    [
        #[derive(Eq)]
        NewChat,
        #[derive(Eq)]
        OpenFile,
        #[derive(Eq)]
        ToggleSide,
        #[derive(Eq)]
        Search,
        #[derive(Eq)]
        Back,
        #[derive(Eq)]
        Forward
    ]
);

pub struct Workspace {
    native: crate::host::platform::Native,
    dictating: bool,
    voice_active: bool,
    voice_connected: bool,
    voice_muted: bool,
    focus: FocusHandle,
    dictation_draft: runtime::DictationDraft,
    /// When the dictation waveform last gained a sample, and the sample total then.
    dictation_tape: std::cell::Cell<Option<(usize, std::time::Instant)>>,
    runtime: runtime::RuntimeState,
    provider_setup: pages::providers::ProviderSetup,
    downloads: Option<crate::gateway::local::PrivateDir>,
    client: crate::gateway::Client,
    regions: region::Regions,
    /// Explicit revision for direct fork/fixture edits; gateway updates use region revisions.
    gateway_revision: u64,
    storage: storage::StorageUi,
    page_tabs: Vec<PageTab>,
    active_page: usize,
    navigation_section: navigation::Section,
    navigation_visible: bool,
    navigation_catalog: navigation::CatalogState,
    history: navigation::History,
    sidebar_resize: Entity<ResizableState>,
    content_resize: Entity<ResizableState>,
    split_content: bool,
    /// The sidebar and side pane slides in progress.
    motion: layout::PanelMotion,
    draft_workspace: std::path::PathBuf,
    draft_bot: Option<Arc<str>>,
    queued_draft: Option<(Arc<str>, runtime::DraftIntent)>,
    appearance: Appearance,
    accent: Accent,
    dark_colors: theme::Colors,
    light_colors: theme::Colors,
    palette: Palette,
    composer: Entity<TextareaState>,
    model_picker: Option<chat::ModelPicker>,
    endpoint: Entity<InputState>,
    pairing_code: Entity<InputState>,
    drafts: BTreeMap<String, String>,
    /// Preference edits on their way to the disk, and the task writing them.
    preference_writes: Arc<crate::gateway::preferences::Writer<preferences::Preferences>>,
    preference_drain: Option<Task<()>>,
    transcript_scrolls: BTreeMap<String, transcript::TranscriptScroll>,
    expanded: std::collections::BTreeSet<String>,
    shell: navigation::ShellState,
    chat_ui: chat::ChatUi,
    content: panel::ContentState,
    bots: pages::bots::BotsUi,
    memory: pages::memory::MemoryUi,
    config: pages::ConfigUi,
    _subscriptions: Vec<Subscription>,
    _events: Task<()>,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let composer = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("just say what you need")
                .auto_grow(1, 8)
                .submit_on_enter(true)
        });
        let sidebar_resize = cx.new(|_| ResizableState::default());
        let content_resize = cx.new(|_| ResizableState::default());
        let endpoint = cx.new(|cx| InputState::new(window, cx).placeholder("Gateway endpoint"));
        let pairing_code =
            cx.new(|cx| InputState::new(window, cx).placeholder("One-time pairing code"));
        let subscriptions = vec![
            Self::persist_on_exit(window, cx),
            cx.subscribe_in(&composer, window, |this, _, event, window, cx| {
                this.composer_event(event, window, cx);
            }),
            Self::intercept_mention_keys(cx),
            cx.observe_window_appearance(window, |this, _, cx| this.refresh_appearance(cx)),
            cx.observe_window_activation(window, |_, _, cx| cx.notify()),
            cx.observe(&sidebar_resize, |_, _, cx| cx.notify()),
            cx.observe(&content_resize, |_, _, cx| cx.notify()),
        ];
        let mut client = crate::gateway::Client::start();
        let native = crate::host::platform::Native::new(cx);
        let events = Self::event_task(
            native.wakeups(),
            client.gateway.as_mut().and_then(Gateway::take_updates),
            window,
            cx,
        );
        let mut preferences = preferences::Preferences::load().unwrap_or_default();
        let regions = region::Regions::new(cx);
        #[cfg(feature = "snapshot")]
        Self::start_snapshots(window, cx);
        cx.defer_in(window, |this, _, cx| this.refresh_appearance(cx));
        Self {
            native,
            dictating: false,
            voice_active: false,
            voice_connected: false,
            voice_muted: false,
            focus: cx.focus_handle(),
            dictation_draft: Default::default(),
            dictation_tape: Default::default(),
            runtime: Default::default(),
            provider_setup: Default::default(),
            downloads: crate::gateway::local::private_dir(),
            client,
            regions,
            gateway_revision: 0,
            storage: Default::default(),
            page_tabs: vec![PageTab::Draft],
            active_page: 0,
            navigation_section: navigation::Section::Chats,
            navigation_visible: preferences.sidebar_visible,
            navigation_catalog: navigation::CatalogState::restored(&mut preferences),
            history: Default::default(),
            sidebar_resize,
            content_resize,
            split_content: true,
            motion: Default::default(),
            draft_workspace: runtime::UNSELECTED_FOLDER.into(),
            draft_bot: None,
            queued_draft: None,
            appearance: preferences.appearance,
            accent: preferences.accent,
            dark_colors: preferences.dark_colors,
            light_colors: preferences.light_colors,
            palette: preferences.palette(cx).install(cx),
            composer,
            model_picker: None,
            endpoint,
            pairing_code,
            shell: navigation::ShellState::restored(&mut preferences),
            config: pages::ConfigUi::restored(&mut preferences),
            drafts: preferences.drafts,
            preference_writes: Default::default(),
            preference_drain: None,
            transcript_scrolls: BTreeMap::new(),
            expanded: Default::default(),
            chat_ui: chat::ChatUi::restored(&preferences.session_card_order),
            content: Default::default(),
            bots: Default::default(),
            memory: Default::default(),
            _subscriptions: subscriptions,
            _events: events,
        }
    }
}

impl Workspace {
    /// Runs before key bindings, so the `@` menu's keys never also send the message.
    fn intercept_mention_keys(cx: &mut Context<Self>) -> Subscription {
        let view = cx.entity().downgrade();
        cx.intercept_keystrokes(move |event, window, cx| {
            if let Err(error) = view.update(cx, |this, cx| {
                if this.mention_key(&event.keystroke, window, cx) {
                    cx.stop_propagation();
                }
            }) {
                tracing::debug!(%error, "the workspace closed before a keystroke reached it");
            }
        })
    }

    fn composer_event(&mut self, event: &InputEvent, window: &mut Window, cx: &mut Context<Self>) {
        cx.notify();
        if matches!(event, InputEvent::Change) {
            self.stop_dictation_if_edited(cx);
            self.track_mention(cx);
        }
        if matches!(event, InputEvent::Focus | InputEvent::Change) {
            self.prewarm_title();
        }
        if matches!(event, InputEvent::PressEnter { shift: false, .. }) {
            self.send_message(window, cx);
        }
    }

    fn event_task(
        mut native: tokio::sync::watch::Receiver<u64>,
        mut updates: Option<tokio::sync::mpsc::Receiver<crate::gateway::Update>>,
        window: &Window,
        cx: &Context<Self>,
    ) -> Task<()> {
        cx.spawn_in(window, async move |this, cx| {
            if let Err(error) = this.update_in(cx, Self::poll) {
                tracing::debug!(%error, "the workspace closed before its first poll");
            }
            loop {
                let update = tokio::select! {
                    _ = native.changed() => None,
                    update = async {
                        match &mut updates {
                            Some(updates) => updates.recv().await,
                            None => std::future::pending().await,
                        }
                    } => {
                        if update.is_none() { updates = None; }
                        update
                    }
                };
                let mut batch = Vec::new();
                if let Some(update) = update {
                    batch.push(update);
                }
                if let Some(updates) = &mut updates {
                    while batch.len() < 256 {
                        let Ok(update) = updates.try_recv() else {
                            break;
                        };
                        batch.push(update);
                    }
                }
                if this
                    .update_in(cx, |this, window, cx| {
                        let changed = !batch.is_empty();
                        for update in batch {
                            this.gateway_update(update, window, cx);
                        }
                        this.poll(window, cx);
                        if changed {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.refresh_removable_files();
        if !self.shell.notices.is_empty() {
            crate::host::platform::wake();
        }
        self.chat_ui.visible = window.is_window_active();
        self.ensure_session_ticker(cx);
        let viewer = self
            .active_content()
            .filter(|_| window.is_window_active())
            .map(|tab| Arc::clone(&tab.id));
        self.sync_agent_browser_visibility(viewer.as_deref());
        let visible = viewer.as_deref() == Some("computer")
            && !window.has_active_dialog(cx)
            && !self.expanded.contains("native-overlay");
        self.sync_desktop_visibility(visible);
        let p = self.palette;
        if window.has_active_dialog(cx)
            || self.expanded.contains("native-overlay")
            || !self
                .active_content()
                .is_some_and(|tab| tab.content.shows_native())
        {
            self.native.hide_panel();
        }
        #[cfg(feature = "snapshot")]
        snapshot::count_render(0);
        self.track_panel_motion(window, cx);
        self.track_disclosure(window, cx);
        let body = self.render_page(window, cx);
        // Fade the new section in without shifting its layout; re-renders keep the same id.
        let page = SharedString::from(format!("page:{:?}", self.page_tabs.get(self.active_page)));
        let body = motion::fade_in(
            div().size_full().min_h_0().flex().flex_col().child(body),
            page,
            motion::QUICK,
            cx,
        );
        let body = self.workspace_body(body, cx);
        self.sync_window_background(window);
        div()
            .relative()
            .size_full()
            .key_context("Workspace")
            .track_focus(&self.focus)
            .map(|root| Self::shell_actions(root, cx))
            .map(|root| Self::update_actions(root, cx))
            .flex()
            .flex_col()
            .font_family(FontSet::current().regular())
            .font_weight(FontWeight::NORMAL)
            .text_size(Type::Body.size())
            .text_color(p.text)
            .child(backdrop::layer(&p, cx))
            .child(self.chrome(window, cx))
            .child(body)
            .children(self.toast_layer(cx))
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        FocusHandle::clone(&self.focus)
    }
}
