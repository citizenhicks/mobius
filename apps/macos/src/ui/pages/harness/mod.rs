//! Harnesses, the runtimes a Bot runs on. This release runs only möbius Bots; other harness
//! records remain visible without offering unavailable actions.

mod rows;

pub(in crate::ui) use rows::{Choice, option_menu};

use super::*;
use mobius_gateway::wire::HarnessKind;

/// The möbius middleware that turns memory on or off; a Bot's Memory section shows it, not its
/// Capabilities.
pub(in crate::ui) const MEMORY: &str = "memory";

/// The harness's glyph in lists and menus; the rail draws möbius as its two-layer logo.
pub(in crate::ui) const fn glyph(kind: HarnessKind) -> &'static str {
    match kind {
        HarnessKind::Mobius => "bot.face",
        HarnessKind::Codex => "hi.chatGpt",
        HarnessKind::Claude => "hi.claude",
    }
}

/// The harness's name, also while no gateway describes it.
pub(in crate::ui) const fn name(kind: HarnessKind) -> &'static str {
    match kind {
        HarnessKind::Mobius => "möbius",
        HarnessKind::Codex => "Codex",
        HarnessKind::Claude => "Claude",
    }
}

/// This release runs Bots only on the möbius harness.
pub(in crate::ui) const fn available(kind: HarnessKind) -> bool {
    matches!(kind, HarnessKind::Mobius)
}

/// Why a Bot on another harness cannot run in this release.
pub(in crate::ui) fn unavailable(kind: HarnessKind) -> Option<String> {
    (!available(kind)).then(|| format!("{} isn’t available on this gateway", name(kind)))
}

/// The möbius rail entry's groups, each disclosing its rows while one of its pages is open.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::ui) enum MobiusGroup {
    Providers,
    Extensions,
    Defaults,
}

impl MobiusGroup {
    const ALL: [Self; 3] = [Self::Providers, Self::Extensions, Self::Defaults];

    /// The group whose rows `page` belongs to.
    pub(in crate::ui) const fn of(page: &PageTab) -> Option<Self> {
        match page {
            PageTab::Providers | PageTab::Provider(_) | PageTab::ProviderDraft(_) => {
                Some(Self::Providers)
            }
            PageTab::Extensions | PageTab::Extension(_) => Some(Self::Extensions),
            PageTab::AgentDefaults(_) => Some(Self::Defaults),
            _ => None,
        }
    }

    /// The group's disclosure key (`disclosed_item`).
    pub(in crate::ui) const fn key(self) -> &'static str {
        match self {
            Self::Providers => "mobius:providers",
            Self::Extensions => "mobius:extensions",
            Self::Defaults => "mobius:defaults",
        }
    }

    const fn title(self) -> &'static str {
        match self {
            Self::Providers => "Providers",
            Self::Extensions => "Extensions",
            Self::Defaults => "Agent defaults",
        }
    }

    const fn glyph(self) -> &'static str {
        match self {
            Self::Providers => "hi.plugsConnected",
            Self::Extensions => "hi.squaresFour",
            Self::Defaults => "hi.slidersHorizontal",
        }
    }
}

impl Workspace {
    /// Providers, Extensions and Agent defaults; the open page's group shows its rows.
    pub(in crate::ui) fn mobius_list(&self, cx: &Context<Self>) -> AnyElement {
        let mut list = crate::ui::navigation::sidebar_list();
        for group in MobiusGroup::ALL {
            let key = group.key();
            let row = self
                .page_row(
                    key,
                    group.title(),
                    Leading::glyph(group.glyph()),
                    self.group_page(group),
                    cx,
                )
                .selected(false)
                .caret(self.disclosure_shown(key) > 0.5);
            list = list
                .child(row)
                .children(self.sidebar_disclosure(key, || match group {
                    MobiusGroup::Providers => self.provider_rows(cx),
                    MobiusGroup::Extensions => self.extension_rows(cx),
                    MobiusGroup::Defaults => self.defaults_rows(cx),
                }));
        }
        list.into_any_element()
    }

    /// Where a group's row leads: the first provider, the extensions or the defaults prompt.
    fn group_page(&self, group: MobiusGroup) -> PageTab {
        match group {
            MobiusGroup::Providers => self
                .client
                .state
                .gateway
                .as_ref()
                .and_then(|gateway| gateway.provider_instances.first())
                .map_or(PageTab::Providers, |provider| {
                    PageTab::Provider(provider.selection.instance.clone()) // rules-allow: the page tab owns the id
                }),
            MobiusGroup::Extensions => PageTab::Extensions,
            MobiusGroup::Defaults => PageTab::AgentDefaults(AgentSection::Prompt),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_release_does_not_enable_cli_harnesses() {
        assert!(available(HarnessKind::Mobius));
        assert!(!available(HarnessKind::Codex));
        assert!(!available(HarnessKind::Claude));
    }
}
