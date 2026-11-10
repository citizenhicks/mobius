//! The sidebar's gateway switcher: connection status, machine name and the gateway menu.

use super::*;
use gpui_component::button::{ButtonCustomVariant, ButtonVariant};
use gpui_component::menu::DropdownMenu;

impl Workspace {
    pub(super) fn gateway_switcher(&self, cx: &Context<Self>) -> AnyElement {
        let p = self.palette;
        let loading = self.client.link.is_loading();
        let status = if loading {
            Spinner::new(glyph::INLINE)
                .color(p.muted)
                .into_any_element()
        } else {
            status_dot(self.client.link.tone().color(&p)).into_any_element()
        };
        // The mark fades between dot and ring rather than popping, keyed by which it shows.
        let status = motion::fade_in(
            div().flex().items_center().child(status),
            SharedString::from(format!("gateway-status:{loading}")),
            motion::QUICK,
            cx,
        );
        let view = cx.entity().downgrade();
        let current = self.current_gateway();
        let saved: Vec<_> = self
            .saved_gateways()
            .map(|endpoint| {
                let gateway = GatewayRef::Saved(endpoint.to_owned());
                let name = self.gateway_display_name(&gateway).into_owned();
                (gateway, name)
            })
            .collect();
        Button::new("gateway-switcher")
            .with_variant(ButtonVariant::Custom(
                ButtonCustomVariant::new(cx)
                    .color(transparent_black())
                    .foreground(p.muted)
                    .hover(p.hover)
                    .active(p.pressed),
            ))
            .h(height::MINI)
            .px(space::S)
            .tooltip(self.link_label())
            .accessibility_label(format!("Gateway: {}", self.link_label()))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::XS)
                    .min_w_0()
                    .typography(Type::Caption)
                    .child(status)
                    .child(div().min_w_0().truncate().child(self.gateway_name()))
                    .child(chevron(false, &palette(cx))),
            )
            .dropdown_menu_with_anchor(Anchor::TopLeft, move |menu, _, cx| {
                gateway_menu(menu, &view, &current, &saved, cx)
            })
            .on_open_change(cx.listener(|this, open, _, cx| this.overlay_changed(*open, cx)))
            .into_any_element()
    }
}

fn gateway_menu(
    mut menu: gpui_component::menu::PopupMenu,
    view: &WeakEntity<Workspace>,
    current: &GatewayRef,
    saved: &[(GatewayRef, String)],
    cx: &App,
) -> gpui_component::menu::PopupMenu {
    let local = (GatewayRef::Local, "This Mac".to_owned());
    let p = palette(cx);
    for (gateway, label) in std::iter::once(&local).chain(saved) {
        let connection = Workspace::connection(gateway);
        let run = move |this: &mut Workspace, _: &mut Window, cx: &mut Context<Workspace>| {
            this.connect(connection.clone(), cx); // rules-allow: the click handler is Fn and each connect owns its connection
        };
        menu = menu.item(
            workspace_item(SharedString::from(label), gateway.glyph(), view, run, &p)
                .checked(gateway == current),
        );
    }
    menu.separator()
        .item(workspace_item(
            "Pair gateway…",
            "hi.plus",
            view,
            |this, window, cx| this.pair_gateway_dialog(window, cx),
            &p,
        ))
        .item(workspace_item(
            "Gateway settings",
            "hi.gear",
            view,
            |this, window, cx| {
                let page = GatewayPage::Machine(this.current_gateway());
                this.open_page(PageTab::Gateway(page), window, cx);
            },
            &p,
        ))
}
