//! Cloud account sign-in through the providers configured in cloud.json. Swift
//! keeps the bearer in Keychain; a pairing grant joins the ordinary gateway pairing flow.

use super::*;
use crate::host::platform::{
    CloudAction, CloudAge, CloudErrorKind, CloudStage, CloudStatus, cloud_config,
};
use mobius_gateway::client::Endpoint;

/// Apple's subscription management; purchases stay in the iPhone and iPad app.
pub(in crate::ui) const SUBSCRIPTIONS: &str = "https://apps.apple.com/account/subscriptions";

/// What sets a plan apart, in words rather than machine specs.
fn plan_summary(plan: &str) -> &'static str {
    if plan == "Cloud Plus" {
        "A faster machine, more storage and 4× the AI usage"
    } else {
        "A private gateway for everyday work"
    }
}

/// A provider's name as its sign-in button reads.
fn provider_name(provider: &str) -> String {
    match provider {
        "apple" => "Apple".into(),
        "github" => "GitHub".into(),
        "google" => "Google".into(),
        other => {
            let mut chars = other.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(chars).collect()
            })
        }
    }
}

/// A service without an age gate can connect without an Apple age decision.
fn can_connect(status: &CloudStatus, age_gate: Option<u8>) -> bool {
    status.subscribed && (age_gate.is_none() || status.age == Some(CloudAge::Eligible))
}

/// What a Cloud step tells the user as it starts; reading the account is routine.
fn stage_toast(stage: CloudStage) -> Option<&'static str> {
    match stage {
        CloudStage::SigningIn => Some("Complete sign-in in the browser window."),
        // Pairing announces itself (pair_gateway).
        CloudStage::Checking | CloudStage::Connecting => None,
        CloudStage::Provisioning => {
            Some("Preparing your möbius Cloud gateway. A new gateway takes a minute or two.")
        }
    }
}

/// The saved-gateway key for an endpoint, so a grant matches what pairing stores.
fn saved_key(endpoint: &str) -> String {
    endpoint
        .parse::<Endpoint>()
        .map_or_else(|_| endpoint.to_owned(), |endpoint| endpoint.to_string())
}

impl Workspace {
    pub(in crate::ui) fn cloud_status(&self) -> &CloudStatus {
        &self.config.cloud
    }

    /// The connected gateway is the one möbius Cloud paired.
    pub(in crate::ui) fn on_cloud_gateway(&self) -> bool {
        self.config.cloud.gateway_endpoint.as_deref().is_some_and(|endpoint| {
            matches!(self.current_gateway(), GatewayRef::Saved(current) if current == saved_key(endpoint))
        })
    }

    pub(in crate::ui) fn cloud(&self, action: CloudAction) {
        // Snapshot runs show fixture state and never reach the Cloud service.
        if cfg!(feature = "snapshot") && std::env::var_os("MOBIUS_SNAPSHOT").is_some() {
            return;
        }
        self.native.cloud(action);
    }

    /// Progress and failures arrive as toasts, so the page never shifts under the pointer.
    pub(in crate::ui) fn set_cloud_status(&mut self, status: CloudStatus) {
        self.config.cloud_loaded = true;
        let previous = std::mem::replace(&mut self.config.cloud, status);
        let status = &self.config.cloud;
        let started = status.stage.filter(|stage| previous.stage != Some(*stage));
        let failed = status
            .error
            .as_ref()
            .filter(|error| previous.error.as_ref() != Some(*error));
        let failure = failed.map(|error| match status.error_kind {
            Some(CloudErrorKind::Session) => format!("Signed out of möbius Cloud: {error}"),
            _ => error.to_owned(),
        });
        if let Some(message) = started.and_then(stage_toast) {
            self.announce(Tone::Info, message.to_owned());
        }
        if let Some(failure) = failure {
            self.report(failure);
        }
    }

    pub(in crate::ui) fn pair_cloud_gateway(
        &mut self,
        endpoint: &str,
        code: String,
        cx: &mut Context<Self>,
    ) {
        self.pair_gateway(saved_key(endpoint), code, cx);
    }

    pub(in crate::ui) fn forget_cloud_gateway(&mut self, endpoint: &str, cx: &mut Context<Self>) {
        self.forget_saved_gateway(&saved_key(endpoint), cx);
    }

    pub(super) fn cloud_section(&self, cx: &Context<Self>) -> Section {
        let status = &self.config.cloud;
        // Until the account is read, rows the size of a signed-in account stand in.
        let loading = !self.config.cloud_loaded
            || (status.stage == Some(CloudStage::Checking)
                && status.name.is_none()
                && status.email.is_none()
                && !status.subscribed);
        let group = if loading {
            cloud_loading_rows()
        } else if status.signed_in {
            self.cloud_account_rows(status, cx)
        } else {
            self.cloud_offer_rows()
        };
        Section::new("möbius Cloud").child(group)
    }

    /// Sign out, or Pair with a code then one sign-in button per configured provider.
    pub(super) fn account_actions(&self, cx: &Context<Self>) -> Vec<AnyElement> {
        let status = &self.config.cloud;
        let pair = toolbar_button("pair-cloud", "Pair with a code", "hi.link", cx)
            .on_click(cx.listener(|this, _, window, cx| this.pair_gateway_dialog(window, cx)))
            .into_any_element();
        if status.signed_in {
            let sign_out = toolbar_button("cloud-sign-out", "Sign out", "hi.lockOpen", cx)
                .on_click(cx.listener(|this, _, _, _| this.cloud(CloudAction::SignOut)))
                .into_any_element();
            vec![sign_out, pair]
        } else {
            let mut actions = vec![pair];
            for (index, provider) in
                (0u8..).zip(cloud_config().map_or(&[][..], |cloud| &cloud.providers))
            {
                let id = ElementId::Name(format!("cloud-sign-in-{provider}").into());
                let button = if provider == "apple" {
                    apple_toolbar_button(id, cx)
                } else {
                    let label = format!("Sign in with {}", provider_name(provider));
                    toolbar_button(id, label, "hi.userFocus", cx)
                };
                let action = CloudAction::SignIn(index);
                actions.push(
                    button
                        .disabled(status.stage.is_some())
                        .on_click(cx.listener(move |this, _, _, _| this.cloud(action)))
                        .into_any_element(),
                );
            }
            actions
        }
    }

    fn cloud_offer_rows(&self) -> Group {
        Group::new()
            .footer(match cloud_config().and_then(|cloud| cloud.age_gate) {
                Some(age) => format!("For adults aged {age} and over: after you sign in, Apple asks whether to share your age range. Bots are AI agents and can make mistakes."),
                None => "Bots are AI agents and can make mistakes.".to_owned(),
            })
            .child(
                Row::new("cloud", "Your private gateway, managed by möbius")
                    .leading(Leading::glyph("hi.cloudServer"))
                    .subtitle("Provisioned, secured and maintained for your account.")
                    .external()
                    .on_click(|_, _, cx| {
                        if let Some(site) = super::settings::site() {
                            cx.open_url(site);
                        }
                    }),
            )
            .child(
                Row::new("cloud-plans", "Plans")
                    .subtitle("Cloud for everyday work, or Cloud Plus for a faster machine, more storage and 4× the AI usage."),
            )
            .child(
                Row::new("cloud-account", "Account")
                    .subtitle(if cloud_config().is_some() {
                        "Sign in to connect this Mac."
                    } else {
                        "möbius Cloud isn’t configured for this build."
                    })
                    .status(
                        self.palette.muted,
                        if self.config.cloud.stage.is_some() {
                            "Signing in…"
                        } else {
                            "Signed out"
                        },
                    ),
            )
    }

    fn cloud_account_rows(&self, status: &CloudStatus, cx: &Context<Self>) -> Group {
        let signed_in = status.provider.as_deref().map_or_else(
            || "Signed in".to_owned(),
            |provider| format!("Signed in with {}", provider_name(provider)),
        );
        let (title, subtitle) = match (status.name.as_deref(), status.email.as_deref()) {
            (Some(name), Some(email)) => (name, email),
            (None, Some(email)) => (email, signed_in.as_str()),
            (Some(name), None) => (name, signed_in.as_str()),
            (None, None) => ("Cloud Account", signed_in.as_str()),
        };
        let group = Group::new()
            .child(
                Row::new("cloud-account", title)
                    .leading(Leading::glyph("hi.cloudServer"))
                    .subtitle(subtitle),
            )
            .children(self.cloud_age_row(status, cx))
            .child(self.cloud_plan_row(status));
        // Apply the configured age gate while keeping the subscription requirement.
        if can_connect(status, cloud_config().and_then(|cloud| cloud.age_gate)) {
            group.child(self.cloud_gateway_row(status, cx))
        } else {
            group
        }
    }

    /// The plan and its state; while subscribed, the row opens its App Store management.
    fn cloud_plan_row(&self, status: &CloudStatus) -> Row {
        let row = Row::new("cloud-plan", "Plan");
        if !status.subscribed {
            let subtitle = if let Some(gate) = cloud_config()
                .and_then(|cloud| cloud.age_gate)
                .filter(|_| status.age == Some(CloudAge::Underage))
            {
                format!("Subscriptions require age {gate} or over.")
            } else {
                "Subscribe in the möbius app on iPhone or iPad.".to_owned()
            };
            return row
                .subtitle(subtitle)
                .status(self.palette.muted, "No subscription");
        }
        let plan = status.plan.as_deref().unwrap_or("Cloud");
        let active = status.subscription.as_deref().is_none_or(|s| s == "Active");
        let tone = if active {
            self.palette.signal
        } else {
            self.palette.warning
        };
        row.subtitle(format!(
            "{}. Manage it in the App Store.",
            plan_summary(plan)
        ))
        .status(
            tone,
            format!(
                "{plan} · {}",
                status.subscription.as_deref().unwrap_or("Active")
            ),
        )
        .external()
        .on_click(|_, _, cx| cx.open_url(SUBSCRIPTIONS))
    }

    /// Cloud's age decision. Apple's sheet appears after sign-in while none is recorded;
    /// a decline or failure keeps the sign-in and offers another try.
    fn cloud_age_row(&self, status: &CloudStatus, cx: &Context<Self>) -> Option<Row> {
        let p = self.palette;
        let gate = cloud_config()?.age_gate?;
        let age = status.age?;
        let (subtitle, tone, label) = match age {
            CloudAge::Eligible => (
                "Recorded on your möbius Cloud account.".to_owned(),
                p.signal,
                format!("{gate}+"),
            ),
            CloudAge::Underage => (
                format!(
                    "möbius Cloud requires age {gate} or over. The age range shared by your Apple Account is under {gate}."
                ),
                p.danger,
                format!("Under {gate}"),
            ),
            CloudAge::Checking => (
                "Apple asks whether to share your age range with möbius.".to_owned(),
                p.muted,
                "Checking".to_owned(),
            ),
            CloudAge::Declined => (
                format!(
                    "You chose not to share your age range. möbius Cloud requires age {gate} or over."
                ),
                p.warning,
                "Not shared".to_owned(),
            ),
            CloudAge::Failed => (
                "Your age range couldn’t be confirmed.".to_owned(),
                p.warning,
                "Not confirmed".to_owned(),
            ),
        };
        let row = Row::new("cloud-age", "Age range")
            .subtitle(subtitle)
            .status(tone, label);
        let retry = match age {
            CloudAge::Declined => "Share age range",
            CloudAge::Failed => "Retry",
            _ => return Some(row),
        };
        Some(
            row.control(
                settings_button("cloud-age-retry", retry)
                    .disabled(status.stage.is_some())
                    .on_click(cx.listener(|this, _, _, _| this.cloud(CloudAction::ConfirmAge))),
            ),
        )
    }

    fn cloud_gateway_row(&self, status: &CloudStatus, cx: &Context<Self>) -> Row {
        let p = self.palette;
        let (subtitle, tone, label) = match status.gateway.as_deref() {
            Some("ready") => ("Ready to pair with this Mac.", p.signal, "Ready"),
            Some("waiting") => (
                "möbius is provisioning your gateway.",
                p.warning,
                "Preparing",
            ),
            Some("expired") => (
                "An active Cloud subscription is required.",
                p.danger,
                "Paused",
            ),
            Some(_) => (
                "möbius Cloud could not provision your gateway.",
                p.danger,
                "Error",
            ),
            None => ("Checking your gateway.", p.muted, "Checking"),
        };
        let paired = status.gateway_endpoint.as_deref().map(saved_key);
        let connected = self.on_cloud_gateway();
        let saved = paired
            .as_ref()
            .is_some_and(|endpoint| self.saved_gateways().any(|saved| saved == endpoint));
        let (subtitle, tone, label) = match status.stage {
            Some(CloudStage::Provisioning) => (subtitle, p.warning, "Preparing"),
            Some(CloudStage::Connecting) => (subtitle, p.warning, "Connecting"),
            _ if connected => (
                "This Mac is connected to your gateway.",
                p.signal,
                "Connected",
            ),
            _ if saved => ("Paired with this Mac.", p.signal, "Paired"),
            _ => (subtitle, tone, label),
        };
        let row = Row::new("cloud-gateway", "Cloud gateway")
            .subtitle(subtitle)
            .status(tone, label);
        if connected {
            return row;
        }
        row.control(
            settings_button("cloud-connect", "Connect")
                .primary()
                .disabled(status.stage.is_some() || status.gateway.as_deref() == Some("expired"))
                .on_click(cx.listener(|this, _, _, _| this.cloud(CloudAction::Connect))),
        )
    }
}

/// The account's rows while it loads (SettingsLoadingRows "Loading Cloud account").
fn cloud_loading_rows() -> Group {
    Group::new()
        .child(
            Row::new("cloud-account", "Cloud Account")
                .leading(Leading::glyph("hi.cloudServer"))
                .subtitle("Signed in")
                .placeholder(),
        )
        .child(
            Row::new("cloud-age", "Age range")
                .subtitle("Recorded on your möbius Cloud account.")
                .placeholder(),
        )
        .child(
            Row::new("cloud-plan", "Plan")
                .subtitle("A private gateway for everyday work. Manage it in the App Store.")
                .placeholder(),
        )
        .child(
            Row::new("cloud-gateway", "Cloud gateway")
                .subtitle("Ready to pair with this Mac.")
                .placeholder(),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_visibility_follows_the_services_age_gate_and_subscription() {
        let mut status = CloudStatus {
            subscribed: true,
            ..CloudStatus::default()
        };
        assert!(
            can_connect(&status, None),
            "an ungated service must offer Connect"
        );
        assert!(
            !can_connect(&status, Some(18)),
            "a gated service requires an age decision"
        );
        status.age = Some(CloudAge::Eligible);
        assert!(
            can_connect(&status, Some(18)),
            "an eligible subscriber can connect"
        );
        status.subscribed = false;
        assert!(
            !can_connect(&status, None),
            "a subscription is still required without an age gate"
        );
    }
}
