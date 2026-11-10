//! The Credential section: live status, the API key or device sign-in and its code.

use super::*;

impl Workspace {
    pub(super) fn provider_credential(
        &self,
        editor: &ProviderDraft,
        live: Option<&ProviderInstance>,
        cx: &Context<Self>,
    ) -> Section {
        let p = self.palette;
        let label = &editor.definition.label;
        let section = Section::new("Credential").children(self.device_code_group());
        let mut group = Group::new();
        if let Some(live) = live {
            let attention = self.provider_usage_error(&live.selection.instance);
            let (text, color) = match attention {
                Some(_) => ("Needs attention", p.warning),
                None if live.configured => ("Configured on gateway", p.signal),
                None => ("Not configured", p.warning),
            };
            let mut status =
                Row::new("provider-status", "Status").control(status_label(color, text, cx));
            if let Some(error) = attention {
                status = status.subtitle(error.to_owned());
            }
            group = group.child(status);
        }
        if editor.definition.auth == ProviderAuthKind::DeviceCode {
            let busy = matches!(
                self.provider_setup.action,
                ProviderAction::StartingLogin(_) | ProviderAction::DeviceCode { .. }
            );
            let sign_in = button("provider-sign-in", "Sign in")
                .icon(icon("hi.signIn", &palette(cx)))
                .disabled(!self.client.link.is_ready() || busy || !editor.can_register(cx))
                .on_click(cx.listener(|this, _, _, cx| this.start_provider_login(cx)));
            group = group.child(
                Row::new("provider-account", "Account")
                    .subtitle(format!(
                        "Sign in with your {label} account in your browser."
                    ))
                    .control(sign_in),
            );
        } else {
            if let Some(hint) = live.and_then(|live| live.credential_hint.as_deref()) {
                group = group
                    .child(Row::new("provider-saved-key", "Saved key").mono(format!("••••{hint}")));
            }
            group = group.child(
                StackedField::new("API key")
                    .info("Sent once to the gateway and never returned to this app. Sending a new one replaces the stored key for this setup.")
                    .control(text_field(&editor.api_key)),
            );
        }
        section.child(group)
    }

    /// The device code to enter while a device sign-in waits for it.
    fn device_code_group(&self) -> Option<AnyElement> {
        match &self.provider_setup.action {
            ProviderAction::DeviceCode { label, url, code } => {
                Some(self.device_code(label, url, code))
            }
            _ => None,
        }
    }

    fn device_code(&self, label: &str, url: &str, code: &str) -> AnyElement {
        let (url, copied) = (url.to_owned(), code.to_owned());
        let open = button("open-verification", "Open verification page")
            .primary()
            .on_click(move |_, _, cx| cx.open_url(&url));
        let copy = button("copy-device-code", "Copy code").on_click(move |_, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(copied.clone())); // rules-allow: each click hands the clipboard its own copy of the code
        });
        Group::new()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(space::S)
                    .p(space::L)
                    .child(
                        div()
                            .typography(Type::Headline)
                            .child(format!("Finish {label} sign-in")),
                    )
                    .child(
                        div()
                            .typography(Type::Caption)
                            .text_color(self.palette.muted)
                            .child("Open the verification page and enter this code."),
                    )
                    .child(code_box(code, &self.palette))
                    .child(button_row().justify_start().child(open).child(copy)),
            )
            .into_any_element()
    }
}
