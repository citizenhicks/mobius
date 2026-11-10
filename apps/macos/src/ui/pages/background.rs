//! Settings → Appearance → Background: the window background of the appearance on screen.

use super::*;
use crate::numeric::Saturating;
use crate::ui::backdrop::{Dim, Opacity, Sliders, Source, Texture};
use gpui_component::slider::{Slider, SliderEvent, SliderState, SliderValue};
use std::path::Path;

/// The slider step, in percent.
const STEP: f32 = 5.;

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

fn percent(value: SliderValue) -> u8 {
    value.end().round().clamp(0., 100.).saturating_u8()
}

impl Workspace {
    pub(super) fn background_section(&mut self, cx: &mut Context<Self>) -> Section {
        self.ensure_background_sliders(cx);
        let p = self.palette;
        let is_dark = p.is_dark();
        let background = self.background();
        let source = background.source;
        let custom = background.image.as_deref().map(file_name);
        let label = match (source, custom.as_deref()) {
            (Source::Custom, Some(name)) => name.to_owned(),
            _ => source.label().to_owned(),
        };
        let mut items: Vec<_> = Source::CHOICES
            .into_iter()
            .map(|choice| {
                MenuChoice::new(
                    choice.label(),
                    choice == source,
                    cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.set_background_source(choice, cx);
                    }),
                )
            })
            .collect();
        if let Some(name) = custom {
            items.push(MenuChoice::new(
                name,
                source == Source::Custom,
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.set_background_source(Source::Custom, cx);
                }),
            ));
        }
        items.push(MenuChoice::new(
            "Choose image…",
            false,
            cx.listener(|_, _: &ClickEvent, window, cx| {
                Self::choose_background_image(window, cx);
            }),
        ));
        let theme = if is_dark { "dark" } else { "light" };
        let description = match source {
            Source::None => format!("A solid window in the {theme} theme."),
            Source::Transparent => "The desktop shows through, blurred.".to_owned(),
            Source::Custom => background
                .image
                .as_deref()
                .map_or_else(String::new, |path| path.display().to_string()),
            scene => scene.credit().unwrap_or_default(),
        };
        let reduced = self.config.backdrop.reduce_transparency && source != Source::None;
        let picker = menu_select("background-picker", label, None, items, cx);
        let mut row = Row::new("background-source", "Background").control(picker);
        row = if reduced {
            row.status(p.warning, "Reduce transparency is on")
                .subtitle("macOS keeps the window solid.")
        } else {
            row.subtitle(description)
        };
        let mut group = Group::new().child(row);
        if !reduced && let Some(sliders) = &self.config.backdrop.sliders {
            let picture = background.shows_picture();
            if picture {
                let dim = background.dim(is_dark).percent();
                let effect = if is_dark {
                    "Darkens the picture behind the interface."
                } else {
                    "Fades the picture toward the page."
                };
                group = group.child(
                    Row::new("background-dim", "Dim")
                        .subtitle(format!("{dim}% · {effect}"))
                        .control(Slider::new(&sliders.dim).w(width::STEPPER)),
                );
            }
            if source != Source::None {
                let opacity = background.window_opacity().percent();
                group = group.child(
                    Row::new("background-opacity", "Opacity")
                        .subtitle(format!(
                            "{opacity}% · How much of the desktop the window covers."
                        ))
                        .control(Slider::new(&sliders.opacity).w(width::STEPPER)),
                );
            }
            if picture {
                group = group.child(self.frost_row(cx));
            }
            if picture || source == Source::Transparent {
                group = group.child(self.texture_row(cx));
            }
        }
        Section::new("Background").child(group)
    }

    fn frost_row(&self, cx: &Context<Self>) -> Row {
        let frosted = self.background().frosted;
        Row::new("background-frost", "Frosted panels")
            .subtitle(if frosted {
                "Cards and the composer blur the picture behind them."
            } else {
                "Cards show the picture through their tint."
            })
            .control(
                toggle("background-frost-toggle", "Frosted panels", frosted, cx).on_click(
                    cx.listener(|this, on: &bool, _, cx| {
                        this.background_mut().frosted = *on;
                        this.save_backgrounds(cx);
                        this.refresh_appearance(cx);
                    }),
                ),
            )
    }

    fn texture_row(&self, cx: &Context<Self>) -> Row {
        let texture = self.background().texture;
        let items = Texture::CHOICES
            .into_iter()
            .map(|choice| {
                MenuChoice::new(
                    choice.label(),
                    choice == texture,
                    cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.background_mut().texture = choice;
                        this.save_backgrounds(cx);
                        this.refresh_appearance(cx);
                    }),
                )
            })
            .collect();
        Row::new("background-texture", "Texture")
            .subtitle(match texture {
                Texture::Off => "The background shows without a texture.",
                Texture::Dots => "The background shows as a fine dot matrix.",
                Texture::Lines => "Fine lines over the page fade into frost toward the composer.",
            })
            .control(menu_select(
                "background-texture-picker",
                texture.label(),
                None,
                items,
                cx,
            ))
    }

    /// Makes the Dim and Opacity sliders for the appearance and source on screen; they
    /// apply while dragged and save on release.
    fn ensure_background_sliders(&mut self, cx: &mut Context<Self>) {
        let is_dark = self.palette.is_dark();
        let background = self.background();
        let made_for = (is_dark, background.source == Source::Transparent);
        if self
            .config
            .backdrop
            .sliders
            .as_ref()
            .is_some_and(|sliders| sliders.made_for == made_for)
        {
            return;
        }
        let (dim, opacity) = (
            f32::from(background.dim(is_dark).percent()),
            f32::from(background.window_opacity().percent()),
        );
        let dim = cx.new(|_| {
            SliderState::new()
                .min(0.)
                .max(f32::from(Dim::MAX))
                .step(STEP)
                .default_value(dim)
        });
        let opacity = cx.new(|_| {
            SliderState::new()
                .min(f32::from(Opacity::MIN))
                .max(f32::from(Opacity::MAX))
                .step(STEP)
                .default_value(opacity)
        });
        let subscriptions = [
            cx.subscribe(&dim, |this, _, event: &SliderEvent, cx| {
                let (SliderEvent::Change(value) | SliderEvent::Release(value)) = event;
                this.background_mut().dim = Some(Dim::from(percent(*value)));
                this.slid(matches!(event, SliderEvent::Release(_)), cx);
            }),
            cx.subscribe(&opacity, |this, _, event: &SliderEvent, cx| {
                let (SliderEvent::Change(value) | SliderEvent::Release(value)) = event;
                this.background_mut()
                    .set_window_opacity(Opacity::from(percent(*value)));
                this.slid(matches!(event, SliderEvent::Release(_)), cx);
            }),
        ];
        self.config.backdrop.sliders = Some(Sliders {
            made_for,
            dim,
            opacity,
            _subscriptions: subscriptions,
        });
    }

    fn slid(&mut self, released: bool, cx: &mut Context<Self>) {
        if released {
            self.save_backgrounds(cx);
        }
        self.refresh_appearance(cx);
    }
}
