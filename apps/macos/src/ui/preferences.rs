use super::backdrop::Background;
use super::primitives::FontSet;
use super::theme::{Accent, Appearance, Colors, Palette};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Preferences {
    pub appearance: Appearance,
    pub accent: Accent,
    pub font_set: FontSet,
    pub dark_colors: Colors,
    pub light_colors: Colors,
    /// The window background of each appearance.
    pub dark_background: Background,
    pub light_background: Background,
    pub sidebar_visible: bool,
    pub sidebar_width: Option<f32>,
    /// The chat last open on each gateway endpoint.
    pub last_sessions: BTreeMap<String, String>,
    /// Unsent composer text by draft key.
    pub drafts: BTreeMap<String, String>,
    /// Names the user gave gateways, by gateway label.
    pub gateway_names: BTreeMap<String, String>,
    /// Keystrokes the user chose, by shortcut id; defaults are not stored.
    pub shortcuts: BTreeMap<String, String>,
    /// One Bot pinned above Chats for each gateway endpoint.
    pub pinned_bots: BTreeMap<String, String>,
    /// Projects pinned in the Chats sidebar, by gateway endpoint.
    pub pinned_projects: BTreeMap<String, BTreeSet<String>>,
    /// Gateways whose Full Access reminder has already been shown.
    pub full_access_notices: BTreeSet<String>,
    /// The session card's section ids, top to bottom, for every chat.
    pub session_card_order: Vec<String>,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            appearance: Appearance::default(),
            accent: Accent::default(),
            font_set: FontSet::default(),
            dark_colors: Colors::default(),
            light_colors: Colors::default(),
            dark_background: Background::default(),
            light_background: Background::default(),
            sidebar_visible: true,
            sidebar_width: None,
            last_sessions: BTreeMap::new(),
            drafts: BTreeMap::new(),
            gateway_names: BTreeMap::new(),
            shortcuts: BTreeMap::new(),
            pinned_projects: BTreeMap::new(),
            pinned_bots: BTreeMap::new(),
            full_access_notices: BTreeSet::new(),
            session_card_order: Vec::new(),
        }
    }
}

impl Preferences {
    /// The non-empty unsent drafts, as kept across launches.
    pub fn kept_drafts(drafts: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        drafts
            .iter()
            .filter(|(_, text)| !text.trim().is_empty())
            .map(|(key, text)| (key.clone(), text.clone())) // rules-allow: preferences persist their own copy while the workspace keeps editing its drafts
            .collect()
    }

    /// The palette of the appearance on screen, with the chosen fonts installed.
    pub fn palette(&self, cx: &gpui::App) -> Palette {
        let is_dark = self.appearance.is_dark(matches!(
            cx.window_appearance(),
            gpui::WindowAppearance::Dark | gpui::WindowAppearance::VibrantDark
        ));
        let colors = if is_dark {
            self.dark_colors
        } else {
            self.light_colors
        };
        self.font_set.install();
        Palette::new(is_dark, self.accent, colors)
    }

    pub fn load() -> anyhow::Result<Self> {
        crate::gateway::preferences::load()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_font_pair_persists_without_changing_the_colour_profiles() {
        let value = Preferences {
            font_set: FontSet::Space,
            drafts: BTreeMap::from([("chat".into(), "Unsent draft".into())]),
            dark_colors: Colors {
                background: Some(0x18_18_18),
                ..Default::default()
            },
            light_colors: Colors {
                background: Some(0xfa_fa_fa),
                ..Default::default()
            },
            ..Default::default()
        };
        let decoded: Preferences =
            serde_json::from_slice(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(decoded.font_set, FontSet::Space);
        assert_eq!(decoded.dark_colors, value.dark_colors);
        assert_eq!(decoded.light_colors, value.light_colors);
        let mut unknown = serde_json::to_value(&value).unwrap();
        unknown["font_set"] = "future_font".into();
        let decoded: Preferences = serde_json::from_value(unknown).unwrap();
        assert_eq!(decoded.font_set, FontSet::System);
        assert_eq!(decoded.dark_colors, value.dark_colors);
        assert_eq!(decoded.light_colors, value.light_colors);
        assert_eq!(decoded.drafts, value.drafts);
    }

    #[test]
    fn all_non_empty_drafts_survive_a_preferences_roundtrip() {
        let mut preferences = Preferences::default();
        let drafts = (0..60)
            .map(|n| {
                (
                    format!("{n:02}"),
                    if n == 0 {
                        " ".into()
                    } else {
                        format!("draft {n}")
                    },
                )
            })
            .collect();
        preferences.drafts = Preferences::kept_drafts(&drafts);
        let restored: Preferences =
            serde_json::from_slice(&serde_json::to_vec(&preferences).unwrap()).unwrap();
        assert_eq!(restored.drafts.len(), 59);
        assert!(!restored.drafts.contains_key("00"));
        for (key, text) in drafts.iter().filter(|(_, text)| !text.trim().is_empty()) {
            assert_eq!(restored.drafts.get(key), Some(text));
        }
    }
}
