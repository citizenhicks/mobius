//! möbius Cloud on the Account page (MobiusCloudOffer.swift). Swift signs in with Apple and
//! keeps the bearer in Keychain; a pairing grant joins the ordinary gateway pairing flow.

use super::*;
use crate::host::platform::{CloudAction, CloudAge, CloudErrorKind, CloudStage, CloudStatus};
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

/// What a Cloud step tells the user as it starts; reading the account is routine.
fn stage_toast(stage: CloudStage) -> Option<&'static str> {
    match stage {
        CloudStage::SigningIn => Some("Complete Apple sign-in in the browser window."),
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

    /// Sign out, or Pair with a code then Sign in with Apple.
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
            let sign_in = apple_toolbar_button("cloud-sign-in", cx)
                .disabled(status.stage.is_some())
                .on_click(cx.listener(|this, _, _, _| this.cloud(CloudAction::SignIn)))
                .into_any_element();
            vec![pair, sign_in]
        }
    }

    fn cloud_offer_rows(&self) -> Group {
        Group::new()
            .footer("For adults aged 18 and over: after you sign in, Apple asks whether to share your age range. Bots are AI agents and can make mistakes.")
            .child(
                Row::new("cloud", "Your private gateway, managed by möbius")
                    .leading(Leading::glyph("hi.cloudServer"))
                    .subtitle("Provisioned, secured and maintained for your account.")
                    .external()
                    .on_click(|_, _, cx| cx.open_url(super::settings::SITE)),
            )
            .child(
                Row::new("cloud-plans", "Plans")
                    .subtitle("Cloud for everyday work, or Cloud Plus for a faster machine, more storage and 4× the AI usage."),
            )
            .child(
                Row::new("cloud-account", "Account")
                    .subtitle("Sign in with Apple to connect this Mac.")
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
        let (title, subtitle) = match (status.name.as_deref(), status.email.as_deref()) {
            (Some(name), Some(email)) => (name, email),
            (None, Some(email)) => (email, "Signed in with Apple"),
            (Some(name), None) => (name, "Signed in with Apple"),
            (None, None) => ("Apple Account", "Signed in with Apple"),
        };
        let group = Group::new()
            .child(
                Row::new("cloud-account", title)
                    .leading(Leading::glyph("hi.cloudServer"))
                    .subtitle(subtitle),
            )
            .children(self.cloud_age_row(status, cx))
            .child(self.cloud_plan_row(status));
        // Connecting needs both an adult age decision and a subscription.
        if status.subscribed && status.age == Some(CloudAge::Eligible) {
            group.child(self.cloud_gateway_row(status, cx))
        } else {
            group
        }
    }

    /// The plan and its state; while subscribed, the row opens its App Store management.
    fn cloud_plan_row(&self, status: &CloudStatus) -> Row {
        let row = Row::new("cloud-plan", "Plan");
        if !status.subscribed {
            let subtitle = if status.age == Some(CloudAge::Underage) {
                "Subscriptions are for adults aged 18 and over."
            } else {
                "Subscribe in the möbius app on iPhone or iPad."
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
        let age = status.age?;
        let (subtitle, tone, label) = match age {
            CloudAge::Eligible => ("Recorded on your möbius Cloud account.", p.signal, "18+"),
            CloudAge::Underage => (
                "möbius Cloud is only for adults aged 18 and over. The age range shared by your Apple Account is under 18.",
                p.danger,
                "Under 18",
            ),
            CloudAge::Checking => (
                "Apple asks whether to share your age range with möbius.",
                p.muted,
                "Checking",
            ),
            CloudAge::Declined => (
                "You chose not to share your age range. möbius Cloud is for adults aged 18 and over.",
                p.warning,
                "Not shared",
            ),
            CloudAge::Failed => (
                "Your age range couldn’t be confirmed.",
                p.warning,
                "Not confirmed",
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
            Row::new("cloud-account", "Apple Account")
                .leading(Leading::glyph("hi.cloudServer"))
                .subtitle("Signed in with Apple")
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
