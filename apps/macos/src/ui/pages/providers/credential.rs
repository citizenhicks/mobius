//! The Credential section: live status, the API key or device sign-in and its code.

use super::*;

impl Workspace {
    pub(super) fn provider_credential(
        &self,
        group: Group,
        editor: &ProviderDraft,
        live: Option<&ProviderInstance>,
        cx: &Context<Self>,
    ) -> Group {
        let p = self.palette;
        let label = &editor.definition.label;
        let mut group = group;
        // The header says how the credential stands; only a usage failure needs its own row.
        if let Some(error) =
            live.and_then(|live| self.provider_usage_error(&live.selection.instance))
        {
            group = group.child(
                Row::new("provider-status", "Status")
                    .subtitle(error.to_owned())
                    .control(status_label(p.warning, "Needs attention", cx)),
            );
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
            let saved = live.is_some_and(|live| live.credential_hint.is_some());
            let key = text_field(&editor.api_key);
            #[cfg(feature = "snapshot")]
            let key = div()
                .relative()
                .child(key)
                .child(crate::ui::snapshot::probe("provider-key".into()));
            group = group.child(
                StackedField::new("API key")
                    .info("Sent once to the gateway and never returned to this app. Sending a new one replaces the stored key for this setup.")
                    .help(if saved {
                        "Saved on the gateway. Type to replace it."
                    } else {
                        "Sent to the gateway when you save."
                    })
                    .control(key),
            );
        }
        group
    }

    /// The device code to enter while a device sign-in waits for it.
    pub(super) fn device_code_group(&self) -> Option<AnyElement> {
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
