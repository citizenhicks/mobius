//! Host credentials on the connected gateway: GitHub and SSH identities.

use super::*;
use gpui_component::WindowExt;
use std::cell::Cell;
use std::rc::Rc;

const GITHUB: &str = "https://github.com";

impl Workspace {
    pub(in crate::ui) fn refresh_host_credentials(&mut self, cx: &mut Context<Self>) {
        self.send(
            ClientMessage::ProbeGitCredential {
                request_id: crate::gateway::request_id(),
                target: GITHUB.into(),
            },
            cx,
        );
        self.send(
            ClientMessage::ListSshIdentities {
                request_id: crate::gateway::request_id(),
            },
            cx,
        );
    }

    pub(super) fn credential_rows(&self, cx: &Context<Self>) -> [SidebarRow; 2] {
        let mut github = self.page_row(
            "credential:github",
            "GitHub",
            Leading::Gutter,
            PageTab::Gateway(GatewayPage::GitHub),
            cx,
        );
        if matches!(self.client.state.git_credential, Some((true, _))) {
            github = github.status(icon("hi.checkCircle", &palette(cx)).size(glyph::MARK));
        }
        let mut ssh = self.page_row(
            "credential:ssh",
            "SSH identities",
            Leading::Gutter,
            PageTab::Gateway(GatewayPage::Ssh),
            cx,
        );
        if let Some(identities) = &self.client.state.ssh_identities {
            ssh = ssh.meta(identities.len().to_string());
        }
        [github, ssh]
    }

    fn github_summary(&self) -> String {
        match &self.client.state.git_credential {
            _ if !self.client.link.is_ready() => "Connect to check this host.".into(),
            None => "Checking this host…".into(),
            Some((true, Some(user))) => format!("Credential found on this host · {user}"),
            Some((true, None)) => "Credential found on this host.".into(),
            Some((false, _)) => "No credential found.".into(),
        }
    }

    pub(super) fn github_page(&self, cx: &Context<Self>) -> AnyElement {
        let available = matches!(self.client.state.git_credential, Some((true, _)));
        let mut row =
            Row::new("github-credential", "GitHub credential").subtitle(self.github_summary());
        row = if available {
            row.control(icon("hi.checkCircle", &palette(cx)).size(glyph::LEAD))
        } else {
            row.control(
                button("github-set-up", "Set up…")
                    .disabled(!self.client.link.is_ready())
                    .on_click(
                        cx.listener(|this, _, window, cx| this.git_credential_dialog(window, cx)),
                    ),
            )
        };
        let mut group = Group::new()
            .footer("HTTPS credentials are stored by the gateway host’s Git credential helper.")
            .child(row)
            .child(Row::new("github-host", "Host").value("github.com"));
        if let Some((_, Some(user))) = &self.client.state.git_credential {
            group = group.child(Row::new("github-user", "Username").value(user));
        }
        let status = if available {
            "Available"
        } else {
            "Not configured"
        };
        page(
            "gateway-github",
            PageHeader::new("GitHub").subtitle(format!("github.com · {status}")),
        )
        .child(group)
        .into_any_element()
    }

    pub(super) fn ssh_page(&self, cx: &Context<Self>) -> AnyElement {
        let mut group = Group::new().footer(
            "Add the public key to GitHub or another SSH remote. The private key stays on the gateway host.",
        );
        let count = match &self.client.state.ssh_identities {
            None => {
                group = group.child(Row::empty("ssh-loading", "Checking this host…"));
                None
            }
            Some(identities) if identities.is_empty() => {
                group = group.child(Row::empty("ssh-empty", "No public identities found."));
                Some(0)
            }
            Some(identities) => {
                for identity in identities {
                    group = group.child(
                        Row::new(
                            SharedString::from(format!("ssh:{}", identity.fingerprint)),
                            &identity.label,
                        )
                        .leading(Leading::glyph("hi.fingerprint"))
                        .subtitle(&identity.algorithm)
                        .mono(&identity.fingerprint),
                    );
                }
                Some(identities.len())
            }
        };
        if let Some(key) = &self.client.state.ssh_public_key {
            group = group.child(
                StackedField::new("Public key").control(
                    div()
                        .flex()
                        .items_start()
                        .gap(space::S)
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .typography(Type::Mono)
                                .text_color(self.palette.muted)
                                .child(SharedString::from(key)),
                        )
                        .child(machine::copy_button(
                            "copy-public-key",
                            "Copy public key",
                            key,
                            cx,
                        )),
                ),
            );
        }
        let subtitle = match count {
            None => "Gateway host".to_owned(),
            Some(1) => "Gateway host · 1 identity".to_owned(),
            Some(count) => format!("Gateway host · {count} identities"),
        };
        page(
            "gateway-ssh",
            PageHeader::new("SSH identities").subtitle(subtitle),
        )
        .child(group)
        .into_any_element()
    }

    pub(super) fn create_ssh_identity_button(&self, cx: &Context<Self>) -> AnyElement {
        toolbar_button("create-ssh-identity", "Create identity", "hi.plus", cx)
            .disabled(!self.client.link.is_ready())
            .on_click(cx.listener(|this, _, _, cx| {
                this.send(
                    ClientMessage::GenerateSshIdentity {
                        request_id: crate::gateway::request_id(),
                    },
                    cx,
                );
            }))
            .into_any_element()
    }

    pub(in crate::ui) fn git_credential_dialog(&self, window: &mut Window, cx: &mut Context<Self>) {
        let username = cx.new(|cx| InputState::new(window, cx).placeholder("GitHub username"));
        let token = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Personal access token")
                .masked(true)
        });
        let view = cx.entity().downgrade();
        let epoch = self.client.epoch;
        let (muted, danger) = (self.palette.muted, self.palette.danger);
        let missing = Rc::new(Cell::new(false));
        window.open_dialog(cx, move |dialog, _, _| {
            let submit = {
                let (username, token, view, missing) =
                    (Entity::clone(&username), Entity::clone(&token), WeakEntity::clone(&view), Rc::clone(&missing));
                move |window: &mut Window, cx: &mut App| {
                    let user = username.read(cx).value().trim().to_owned();
                    let secret = token.read(cx).value().to_string();
                    if user.is_empty() || secret.is_empty() {
                        missing.set(true);
                        window.refresh();
                        return false;
                    }
                    view.update(cx, |this, cx| {
                        this.submit_gateway_dialog(epoch, cx, |this, cx| {
                            this.send(
                                ClientMessage::ApproveGitCredential {
                                    request_id: crate::gateway::request_id(),
                                    target: GITHUB.into(),
                                    username: user,
                                    token: secret,
                                },
                                cx,
                            );
                        })
                    })
                    .unwrap_or(false)
                }
            };
            dialog
                .title(heading("GitHub credentials"))
                .w(width::DIALOG)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(space::L)
                        .child(
                            div()
                                .typography(Type::Caption)
                                .text_color(muted)
                                .child("Sent once to the host’s configured Git helper. möbius does not store or read it back."),
                        )
                        .child(labelled("Username", text_field(&username).aria_label("Username"), muted))
                        .child(labelled("Personal access token", text_field(&token).aria_label("Personal access token"), muted))
                        .when(missing.get(), |body| {
                            body.child(
                                div()
                                    .typography(Type::Caption)
                                    .text_color(danger)
                                    .child("Enter a username and a token to continue."),
                            )
                        }),
                )
                .map(|dialog| dialog_actions(dialog, "Save", true, submit))
        });
    }
}
