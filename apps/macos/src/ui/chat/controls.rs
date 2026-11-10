//! What sits around the composer surface: the pills row, the draft pickers, the context
//! strip and the per-Bot setting menus (ComposerView.swift, ComposerOptionsView.swift).

use super::*;
use crate::numeric::ToFloat;
use gpui_component::menu::{DropdownMenu, PopupMenu, PopupMenuItem};
use mobius::protocol::{FrontendSettingKind, FrontendSettingValue, FrontendTone};
use mobius_gateway::wire::HarnessConfig;
use std::borrow::Cow;
use std::rc::Rc;

/// SwiftUI's FileCard: a picture shown as itself in its own shape; other files a card
/// with their glyph, name and one line of detail.
pub(in crate::ui) fn file_card(
    id: impl Into<ElementId>,
    glyph: &'static str,
    name: impl Into<SharedString>,
    detail: impl Into<SharedString>,
    detail_color: Hsla,
    thumbnail: Option<&runtime::Thumbnail>,
    cx: &App,
) -> Stateful<Div> {
    let p = palette(cx);
    let size = tile_size(thumbnail);
    let tile = div()
        .id(id)
        .relative()
        .flex_shrink_0()
        .w(size.width)
        .h(size.height)
        .overflow_hidden()
        .tile_corners();
    // A picture is its own tile: no card, frame or letterbox around it.
    if let Some(thumbnail) = thumbnail {
        return tile
            .child(
                img(Arc::clone(&thumbnail.path))
                    .size_full()
                    .tile_corners()
                    .object_fit(ObjectFit::Cover),
            )
            .children(organic::reveal(p.raised, thumbnail.arrived, cx));
    }
    tile.bg(p.control)
        .border_1()
        .border_color(p.separator)
        .p(space::S)
        .flex()
        .flex_col()
        .items_center()
        .child(
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .child(icon(glyph, &p).size(glyph::ICON)),
        )
        .child(
            div()
                .w_full()
                .text_center()
                .truncate()
                .typography(Type::Label)
                .child(name.into()),
        )
        .child(
            div()
                .w_full()
                .text_center()
                .truncate()
                .typography(Type::Caption)
                .text_color(detail_color)
                .child(detail.into()),
        )
}

/// The box file tiles fit in (SwiftUI FileCard's 112 tall): a picture takes its own shape
/// within it, other files a 136 wide card.
const TILE_HEIGHT: f32 = 112.;
const TILE_WIDTH: f32 = 224.;

/// A tile's frame: a picture scaled to fit the box in its own proportions, never cropped
/// or padded; any other file the card's 136 × 112.
pub(in crate::ui) fn tile_size(thumbnail: Option<&runtime::Thumbnail>) -> Size<Pixels> {
    let Some(t) = thumbnail.filter(|t| t.width > 0 && t.height > 0) else {
        return size(px(136.), px(TILE_HEIGHT));
    };
    let (width, height) = (t.width.to_f32(), t.height.to_f32());
    let scale = (TILE_WIDTH / width).min(TILE_HEIGHT / height);
    size(px(width * scale), px(height * scale))
}

/// "PDF · 24 KB".
pub(in crate::ui) fn file_detail(name: &str, size: u64) -> String {
    let kind = std::path::Path::new(name)
        .extension()
        .and_then(|ext| ext.to_str())
        .map_or_else(|| "File".to_owned(), str::to_uppercase);
    format!("{kind} · {}", byte_count(size))
}

pub(in crate::ui) fn file_glyph(media_type: &str) -> &'static str {
    if media_type.starts_with("image/") {
        "hi.image01"
    } else {
        "hi.fileText"
    }
}

impl Workspace {
    /// The new chat's original folder and Bot choices, stacked above the composer.
    pub(super) fn draft_pickers(&self, cx: &Context<Self>) -> AnyElement {
        let p = self.palette;
        let path = SharedString::from(self.draft_workspace.display().to_string());
        let unavailable_reason = if !self.client.link.is_ready() {
            Some("Connect to a gateway to start a chat.")
        } else if self.queued_draft.is_some() {
            Some("Wait for your message to send.")
        } else {
            None
        };
        let folders = self.recent_folders();
        let folder_view = cx.entity().downgrade();
        let folder_path = SharedString::clone(&path);
        let view = cx.entity().downgrade();
        let bot_name = self
            .draft_bot()
            .map_or("Choose Bot", |bot| bot.name.as_ref());
        let bot_glyph = self.draft_bot().map_or_else(
            || icon("bot.face", &p).size(glyph::INLINE).into_any_element(),
            |bot| {
                BotFace::new(Accent::from(bot.tint).color(), bot.shape, glyph::INLINE)
                    .into_any_element()
            },
        );
        let bot = menu_row("draft-bot", bot_glyph, bot_name.to_owned(), &p)
            .accessibility_label(format!("Choose Bot: {bot_name}"));
        #[cfg(feature = "snapshot")]
        let bot = bot.child(crate::ui::snapshot::probe("chat-draft-bot".into()));
        let folder = menu_row(
            "draft-workspace",
            icon("hi.folder", &p).size(glyph::INLINE),
            folder_name(&path),
            &p,
        )
        .accessibility_label(format!("Folder: {path}"))
        .tooltip(path);
        if let Some(reason) = unavailable_reason {
            return div()
                .flex()
                .flex_col()
                .items_start()
                .child(unavailable(
                    "draft-workspace-unavailable",
                    folder,
                    reason,
                    cx,
                ))
                .child(unavailable("draft-bot-unavailable", bot, reason, cx))
                .into_any_element();
        }
        let bot = bot
            .dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, _, cx| {
                let Some(workspace) = view.upgrade() else {
                    return menu;
                };
                let workspace = workspace.read(cx);
                let Some(gateway) = &workspace.client.state.gateway else {
                    return menu;
                };
                bot_menu(
                    menu,
                    &gateway.bots,
                    workspace.draft_bot().map(|bot| bot.id.as_ref()),
                    &view,
                    cx,
                )
            })
            .on_open_change(cx.listener(|this, open, _, cx| this.overlay_changed(*open, cx)));
        div()
            .flex()
            .flex_col()
            .items_start()
            .child(
                folder
                    .dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, _, cx| {
                        folder_menu(menu, &folders, &folder_path, &folder_view, cx)
                    })
                    .on_open_change(
                        cx.listener(|this, open, _, cx| this.overlay_changed(*open, cx)),
                    ),
            )
            .child(bot)
            .into_any_element()
    }

    /// Up to eight distinct folders, most recently used first.
    pub(super) fn recent_folders(&self) -> Vec<String> {
        let mut sessions: Vec<_> = self.client.state.sessions.iter().collect();
        sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
        let mut folders: Vec<String> = Vec::new();
        for label in sessions
            .iter()
            .filter_map(|session| session.session_context.workspace_label.as_ref())
        {
            if !folders.contains(label) && folders.len() < 8 {
                folders.push(label.clone()); // rules-allow: callers keep the folders past this borrow, in menus and the draft
            }
        }
        folders
    }

    /// Picks a folder on the gateway's machine: the native picker for the local gateway,
    /// a path dialog for a remote one. Shell's "Attach folder…" uses it too.
    pub(in crate::ui) fn choose_gateway_folder(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
        on_pick: impl Fn(&mut Self, std::path::PathBuf, &mut Context<Self>) + 'static,
    ) {
        let epoch = self.client.epoch;
        if !self.client.local_gateway {
            self.edit_text(
                TextEdit::new(format!("Folder on {}", self.gateway_name()), String::new()),
                window,
                cx,
                move |this, path, _, cx| {
                    if this.client.epoch == epoch && !path.trim().is_empty() {
                        on_pick(this, path.trim().into(), cx);
                    }
                },
            );
            return;
        }
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose folder".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(mut paths))) = paths.await
                && let Some(path) = paths.pop()
                && let Err(error) = this.update(cx, |this, cx| {
                    if this.client.epoch == epoch {
                        on_pick(this, path, cx);
                    }
                })
            {
                tracing::debug!(%error, "the workspace closed before the chosen folder arrived");
            }
        })
        .detach();
    }

    fn set_draft_folder(&mut self, path: std::path::PathBuf, cx: &mut Context<Self>) {
        if self.draft_selected() {
            self.draft_workspace = path;
            cx.notify();
        }
    }

    /// Attachment tiles above the editor of the expanded card (ComposerAttachmentsView):
    /// one scrolling row, since stacked tiles would push the text field away.
    pub(super) fn composer_context(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let attachments = self.draft_attachments();
        if attachments.is_empty() {
            return None;
        }
        Some(
            div()
                .id("composer-attachments")
                .px(space::S)
                .pt(space::S)
                .flex()
                .items_center()
                .gap(space::S)
                .overflow_x_scroll()
                .children(
                    attachments
                        .iter()
                        .map(|attachment| self.composer_attachment(attachment, cx)),
                )
                .into_any_element(),
        )
    }

    /// ComposerAttachmentRow: the tile dims under a spinner while it uploads, and its
    /// remove button sits in the corner, on a scrim over a thumbnail.
    fn composer_attachment(
        &self,
        attachment: &runtime::DraftAttachment,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let p = self.palette;
        let id = Arc::clone(&attachment.id);
        let removing = attachment.removing();
        let busy = removing || (attachment.file.is_none() && attachment.error.is_none());
        let (detail, color) = match (&attachment.error, &attachment.file) {
            _ if removing => ("Removing…".into(), p.muted),
            (Some(error), _) => (error.clone(), p.danger), // rules-allow: the tile owns its text while the attachment keeps its error
            (None, Some(file)) => (file_detail(&file.name, file.size), p.muted),
            (None, None) => ("Uploading…".into(), p.muted),
        };
        // A failed tile shows its reason, which a thumbnail would cover.
        let thumbnail = attachment
            .error
            .is_none()
            .then(|| self.draft_thumbnail(&attachment.id))
            .flatten();
        let media_type = attachment.file.as_ref().map_or_else(
            || {
                Cow::Owned(
                    mime_guess::from_path(&attachment.name)
                        .first_or_octet_stream()
                        .to_string(),
                )
            },
            |file| Cow::Borrowed(file.media_type.as_str()),
        );
        let remove = mini_button(
            SharedString::from(format!("remove-{id}")),
            format!("Remove {}", attachment.name),
            "hi.x",
            cx,
        )
        .disabled(removing)
        .when(thumbnail.is_some(), |button| {
            button
                .pill_corners()
                .bg(black().opacity(alpha::DIM))
                .text_color(white())
        })
        .on_click(cx.listener(move |this, _, _, cx| this.remove_attachment(&id, cx)));
        file_card(
            SharedString::from(format!("attachment-{}", attachment.id)),
            file_glyph(&media_type),
            attachment.name.clone(), // rules-allow: the tile owns its name while the draft keeps the attachment
            detail,
            color,
            thumbnail.as_ref(),
            cx,
        )
        .when(busy, |tile| {
            tile.child(
                div()
                    .absolute()
                    .inset_0()
                    .tile_corners()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(p.card.opacity(alpha::DIM))
                    .child(Spinner::new(glyph::ICON).color(p.text)),
            )
        })
        .child(
            div()
                .absolute()
                .top(space::XS)
                .right(space::XS)
                .child(remove),
        )
    }

    /// One menu per composer setting of the selected Bot (ComposerSettingMenu), with the
    /// setting's label; the session card lists them.
    pub(super) fn composer_setting_menus(&self, cx: &Context<Self>) -> Vec<(String, AnyElement)> {
        let (Some(gateway), Some(bot)) = (&self.client.state.gateway, self.composer_bot()) else {
            return Vec::new();
        };
        let Some(composition) = bot.config.config.mobius() else {
            return Vec::new();
        };
        let middleware = &composition.middleware;
        let mut menus = Vec::new();
        for feature in &gateway.agent_catalog.middleware_features {
            let enabled = feature.required || middleware.enabled(&feature.id);
            for setting in feature.settings.iter().filter(|setting| setting.composer) {
                let FrontendSettingKind::Select { options, .. } = &setting.kind else {
                    continue;
                };
                let selected = match middleware.setting(&feature.id, &setting.id) {
                    Some(FrontendSettingValue::String(value)) => Some(value.clone()), // rules-allow: the 'static menu builder owns its copy of the selected value
                    _ => None,
                };
                let option = options
                    .iter()
                    .find(|o| Some(&*o.value) == selected.as_deref());
                let label = option.map_or_else(
                    || SharedString::from(&setting.label),
                    |o| SharedString::from(&o.label),
                );
                let (bot_id, feature_id, setting_id): (Arc<str>, Rc<str>, Rc<str>) = (
                    Arc::clone(&bot.id),
                    feature.id.as_str().into(),
                    setting.id.as_str().into(),
                );
                let options = options.clone(); // rules-allow: the 'static menu builder owns its copy of the options
                let view = cx.entity().downgrade();
                let id = SharedString::from(format!("setting:{}:{}", feature.id, setting.id));
                let button = super::session_info::card_menu_trigger(id, label, &self.palette);
                let reason = if self.client.link.is_ready() {
                    (!enabled).then(|| {
                        format!(
                            "Turn on {} for this Bot to change this setting.",
                            feature.label
                        )
                    })
                } else {
                    Some("Connect to a gateway to change this setting.".to_owned())
                };
                let setting_label = setting.label.clone(); // rules-allow: the session card owns each menu's label
                if let Some(reason) = reason {
                    let id = format!("setting-unavailable:{}:{}", feature.id, setting.id);
                    menus.push((setting_label, unavailable(id, button, reason, cx)));
                    continue;
                }
                menus.push((
                    setting_label,
                    button
                        .dropdown_menu_with_anchor(Anchor::TopRight, move |mut menu, _, _| {
                            for option in &options {
                                let value = Arc::clone(&option.value);
                                let destructive = option.tone == FrontendTone::Error
                                    && Some(&*value) != selected.as_deref();
                                let (bot, feature, setting) = (
                                    Arc::clone(&bot_id),
                                    Rc::clone(&feature_id),
                                    Rc::clone(&setting_id),
                                );
                                let view = WeakEntity::clone(&view);
                                let label = if destructive {
                                    SharedString::from(format!("Enable {}", option.label))
                                } else {
                                    SharedString::from(&option.label)
                                };
                                menu = menu.item(
                                    PopupMenuItem::new(label)
                                        .checked(Some(&*value) == selected.as_deref())
                                        .on_click(move |_, _, cx| {
                                            if let Err(error) = view.update(cx, |this, cx| {
                                                this.set_bot_setting(
                                                    &bot, &feature, &setting, &value, cx,
                                                );
                                            }) {
                                                tracing::debug!(%error, "the workspace closed before the setting was chosen");
                                            }
                                        }),
                                );
                            }
                            menu
                        })
                        .on_open_change(
                            cx.listener(|this, open, _, cx| this.overlay_changed(*open, cx)),
                        )
                        .into_any_element(),
                ));
            }
        }
        menus
    }

    fn set_bot_setting(
        &mut self,
        bot_id: &str,
        feature: &str,
        setting: &str,
        value: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(bot) = self.client.state.bot(bot_id) else {
            return;
        };
        let Some(composition) = bot.config.config.mobius() else {
            return;
        };
        // The first write copies the composition the Bot keeps.
        let mut config = Arc::clone(composition);
        Arc::make_mut(&mut config).middleware.set_setting(
            feature,
            setting,
            Some(FrontendSettingValue::String(value.to_owned())),
        );
        let message = update_bot_message(bot, HarnessConfig::Mobius(config));
        self.send(message, cx);
    }
}

/// A menu trigger drawn as a list row: glyph, label and a down chevron (MobiusMenuLabel).
pub(super) fn menu_row(
    id: impl Into<ElementId>,
    glyph: impl IntoElement,
    label: impl Into<SharedString>,
    p: &Palette,
) -> Button {
    Button::new(id)
        .ghost()
        .h(height::REGULAR)
        .px(space::S)
        .child(
            div()
                .flex()
                .items_center()
                .gap(space::S)
                .typography(Type::Body)
                .text_color(p.text)
                .child(glyph)
                .child(label.into())
                .child(chevron(false, p)),
        )
}

/// Chooses the pending Bot for a new chat without changing any session's owner.
pub(in crate::ui) fn bot_menu(
    mut menu: PopupMenu,
    bots: &[mobius_gateway::wire::BotRecord],
    current: Option<&str>,
    view: &WeakEntity<Workspace>,
    _cx: &App,
) -> PopupMenu {
    for bot in bots {
        let id = Arc::clone(&bot.id);
        let view = WeakEntity::clone(view);
        menu = menu.item(
            BotFace::new(Accent::from(bot.tint).color(), bot.shape, glyph::INLINE)
                .menu_item(Arc::clone(&bot.name))
                .checked(current == Some(bot.id.as_ref()))
                .on_click(move |_, _, cx| {
                    if let Err(error) = view.update(cx, |this, cx| {
                        this.draft_bot = Some(Arc::clone(&id));
                        cx.notify();
                    }) {
                        tracing::debug!(%error, "the workspace closed before the Bot was chosen");
                    }
                }),
        );
    }
    menu
}

pub(super) fn folder_name(path: &str) -> String {
    match path {
        "." => "Default folder".into(),
        path => crate::ui::navigation::workspace_name(path).to_owned(),
    }
}

pub(super) fn folder_menu(
    mut menu: PopupMenu,
    folders: &[String],
    current: &str,
    view: &WeakEntity<Workspace>,
    cx: &App,
) -> PopupMenu {
    let mut entries: Vec<String> = folders.to_vec();
    if !entries.iter().any(|folder| folder == ".") {
        entries.push(".".into());
    }
    for folder in entries {
        let (label, checked) = (folder_name(&folder), folder == current);
        let run = move |this: &mut Workspace, _: &mut Window, cx: &mut Context<Workspace>| {
            this.set_draft_folder(folder.clone().into(), cx); // rules-allow: every click hands the draft its own path
        };
        menu =
            menu.item(workspace_item(label, "hi.folder", view, run, &palette(cx)).checked(checked));
    }
    menu.separator().item(workspace_item(
        "Choose folder…",
        "hi.plus",
        view,
        |this, window, cx| this.choose_gateway_folder(window, cx, Workspace::set_draft_folder),
        &palette(cx),
    ))
}
