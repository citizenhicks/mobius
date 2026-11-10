//! Extensions: each installed package's detail, and the install dialog (ExtensionsView.swift).

mod install;

use super::bots::extension_metadata;
use super::*;
use mobius_gateway::wire::{ExtensionHookRecord, ExtensionRecord};

const UNINSTALL_COPY: &str = "The gateway will uninstall it without changing saved chat selections. Chats that reference it continue with the extension disabled. Per-workspace .mobius/extensions data is retained.";

/// A label above machine text worth reading character by character.
fn mono_field(label: &'static str, value: impl Into<SharedString>, muted: Hsla) -> StackedField {
    StackedField::new(label).control(
        div()
            .typography(Type::Mono)
            .text_color(muted)
            .child(value.into()),
    )
}

impl Workspace {
    /// The Extensions landing: the first extension by name, or the empty state.
    pub(in crate::ui) fn render_extensions(&self, cx: &Context<Self>) -> AnyElement {
        let first = self.first_extension_id();
        if let Some(id) = first {
            return self.render_extension(&id);
        }
        let install = button("install-first-extension", "Install extension")
            .primary()
            .disabled(!self.client.link.is_ready())
            .on_click(cx.listener(|this, _, window, cx| this.install_extension_dialog(window, cx)));
        page("extensions", PageHeader::new("Extensions"))
            .child(
                EmptyState::new("hi.squaresFour", "No extensions installed")
                    .detail("Portable plugins this gateway can use in a chat.")
                    .action(install),
            )
            .into_any_element()
    }

    pub(in crate::ui) fn render_extension(&self, id: &str) -> AnyElement {
        let Some(extension) = self.extension(id) else {
            return page("extension", PageHeader::new("Extension unavailable"))
                .child(
                    EmptyState::new("hi.squaresFour", "Extension unavailable")
                        .detail("It is no longer installed on this gateway."),
                )
                .into_any_element();
        };
        let p = self.palette;
        let mut body = page(
            SharedString::from(format!("extension:{id}")),
            PageHeader::new(Arc::clone(&extension.name)).subtitle(extension_metadata(extension)),
        );
        if !extension.description.is_empty() {
            body = body.child(
                Section::new("Description").child(Group::new().child(Row::new(
                    "extension-description",
                    Arc::clone(&extension.description),
                ))),
            );
        }
        body = body.child(Section::new("Package").child(self.package_group(extension)));
        if !extension.skills.is_empty() {
            body = body.child(Section::new("Skills").child(Group::new().children(
                extension.skills.iter().enumerate().map(|(index, skill)| {
                    Row::new(("skill", index), skill.as_str()).leading(Leading::glyph("hi.sparkle"))
                }),
            )));
        }
        if !extension.hooks.is_empty() {
            let needs_trust = !extension.hooks_trusted;
            let footer = if needs_trust {
                "These shell commands run on the gateway when their matching events fire. They stay disabled until you trust them. Trust applies only to the digest above, so an update disables them again."
            } else {
                "Trusted for the digest above. An update to a different snapshot disables them until you trust it."
            };
            let mut section = Section::new("Executable hooks");
            for (index, hook) in extension.hooks.iter().enumerate() {
                section = section.child(hook_group(index, hook, p.muted));
            }
            body = body.child(
                section.child(
                    div()
                        .px(space::XXS)
                        .typography(Type::Caption)
                        .text_color(if needs_trust { p.warning } else { p.muted })
                        .child(footer),
                ),
            );
        }
        body.into_any_element()
    }

    fn package_group(&self, extension: &ExtensionRecord) -> Group {
        let muted = self.palette.muted;
        let kind = extension_metadata(extension);
        let mut group = Group::new().child(
            Row::new("package-kind", "Kind")
                .value(kind.split(" · ").next().unwrap_or_default().to_owned()),
        );
        let optional = [
            ("Version", extension.version.as_ref()),
            ("Source", Some(&extension.source)),
            ("Ref", extension.reference.as_ref()),
            ("Path", extension.subdirectory.as_ref()),
        ];
        for (label, value) in optional {
            if let Some(value) = value {
                group = group.child(Row::new(label, label).value(Arc::clone(value)));
            }
        }
        group
            .child(mono_field(
                "Revision",
                Arc::clone(&extension.resolved_revision),
                muted,
            ))
            .child(mono_field("Digest", Arc::clone(&extension.digest), muted))
    }

    fn extension(&self, id: &str) -> Option<&ExtensionRecord> {
        self.client
            .state
            .gateway
            .as_ref()?
            .agent_catalog
            .extensions
            .iter()
            .find(|extension| *extension.id == *id)
    }

    /// The extension the Extensions landing shows: the first by name.
    pub(in crate::ui) fn first_extension_id(&self) -> Option<String> {
        self.client.state.gateway.as_ref().and_then(|gateway| {
            gateway
                .agent_catalog
                .extensions
                .iter()
                .min_by_key(|extension| extension.name.to_lowercase())
                .map(|extension| extension.id.to_string())
        })
    }

    /// Uninstall…, Update, then Trust or Untrust hooks when the package has hooks.
    pub(super) fn extension_actions(&self, id: &str, cx: &Context<Self>) -> Vec<AnyElement> {
        let Some(extension) = self.extension(id) else {
            return Vec::new();
        };
        let ready = self.client.link.is_ready();
        let (uninstall, update) = (Arc::clone(&extension.id), Arc::clone(&extension.id));
        let name = Arc::clone(&extension.name);
        let mut actions = vec![
            toolbar_button("extension-uninstall", "Uninstall…", "hi.trash", cx)
                .disabled(!ready)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.uninstall_extension(&uninstall, &name, window, cx);
                }))
                .into_any_element(),
            toolbar_button("extension-update", "Update", "hi.refresh03", cx)
                .disabled(!ready)
                .on_click(cx.listener(move |this, _, _, cx| {
                    let request_id = crate::gateway::request_id();
                    let message = ClientMessage::UpdateExtension {
                        request_id,
                        id: Arc::clone(&update),
                    };
                    this.send(message, cx);
                }))
                .into_any_element(),
        ];
        if !extension.hooks.is_empty() {
            let trust = !extension.hooks_trusted;
            let (id, digest) = (Arc::clone(&extension.id), Arc::clone(&extension.digest));
            let (label, glyph) = if trust {
                ("Trust hooks", "hi.shieldCheck")
            } else {
                ("Untrust hooks", "hi.shieldOff")
            };
            actions.push(
                toolbar_button("extension-trust", label, glyph, cx)
                    .when(trust && ready, |button| {
                        button.text_color(self.palette.accent)
                    })
                    .disabled(!ready)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let request_id = crate::gateway::request_id();
                        let (id, expected_digest) = (Arc::clone(&id), Arc::clone(&digest));
                        let message = ClientMessage::TrustExtensionHooks {
                            request_id,
                            id,
                            expected_digest,
                            trusted: trust,
                        };
                        this.send(message, cx);
                    }))
                    .into_any_element(),
            );
        }
        actions
    }

    fn uninstall_extension(
        &self,
        id: &Arc<str>,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = Arc::clone(id);
        self.confirm_destructive(
            format!("Uninstall {name}?"),
            UNINSTALL_COPY,
            "Uninstall",
            window,
            cx,
            move |this, window, cx| {
                let request_id = crate::gateway::request_id();
                let message = ClientMessage::UninstallExtension {
                    request_id,
                    id: Arc::clone(&id),
                };
                this.send(message, cx);
                this.open_page(PageTab::Extensions, window, cx);
            },
        );
    }
}

/// One hook's event, matcher, timeout and command; values are quoted and escaped because
/// they come from a cloned repository.
fn hook_group(index: usize, hook: &ExtensionHookRecord, muted: Hsla) -> Group {
    let matcher = hook
        .matcher
        .as_ref()
        .map_or_else(|| "Any".to_owned(), |matcher| format!("{matcher:?}"));
    Group::new()
        .child(Row::new(("hook-event", index), "Event").mono(format!("{:?}", hook.event)))
        .child(Row::new(("hook-matcher", index), "Matcher").mono(matcher))
        .child(
            Row::new(("hook-timeout", index), "Timeout").mono(format!("{}s", hook.timeout_seconds)),
        )
        .child(mono_field("Command", format!("{:?}", hook.command), muted))
}
