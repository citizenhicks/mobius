mod assets;
mod host;
mod ui;

use host::{browser, platform};
use mobius_client::{self as gateway, numeric};

use gpui::{
    App, AppContext, Bounds, Focusable, Styled, TitlebarOptions, WindowBounds, WindowOptions,
    point, px, size, transparent_black,
};
use gpui_component::Root;

fn main() -> anyhow::Result<()> {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    if arguments.first().is_some_and(|arg| arg == "__serve") {
        let filter = std::env::var("RUST_LOG")
            .unwrap_or_else(|_| "off,mobius=info,mobius_gateway=info".into());
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::try_new(filter)?)
            .with_writer(std::io::stderr)
            .try_init()
            .map_err(std::io::Error::other)?;
        tokio::runtime::Runtime::new()?.block_on(gateway::run_embedded(arguments))?;
        return Ok(());
    }
    // One möbius per user owns the local gateway. The snapshot harness, which only reads, and
    // an instance with its own gateway state may run beside it.
    let isolated =
        cfg!(feature = "snapshot") || std::env::var_os("MOBIUS_GATEWAY_STATE_DIR").is_some();
    if !isolated && platform::hand_off_launch() {
        return Ok(());
    }
    gateway::set_admission(platform::gateway_endpoint);
    platform::configure_text();
    let app = gpui_platform::application().with_assets(assets::Assets);
    // A Dock click or a second launch handed over by `hand_off_launch` lands here, also
    // while möbius runs from the menu bar only.
    app.on_reopen(|cx| {
        platform::restore();
        if cx.windows().is_empty() {
            open_workspace(cx);
        }
        cx.activate(true);
    });
    app.run(|cx| {
        gpui_component::init(cx);
        // Remote images in interface documents load through GPUI's HTTP client.
        match host::http::ImageClient::new() {
            Ok(client) => cx.set_http_client(std::sync::Arc::new(client)),
            #[expect(
                clippy::print_stderr,
                reason = "the app installs no log subscriber outside `__serve`; stderr is where this is seen"
            )]
            Err(error) => eprintln!("Remote images are unavailable: {error}"),
        }
        ui::install_menus(cx);
        platform::install_status_item(include_bytes!("../assets/icons/MobiusMenuBar.svg"));
        cx.on_app_quit(|_| {
            browser::shutdown();
            async {}
        })
        .detach();
        open_workspace(cx);
        cx.activate(true);
    });
    Ok(())
}

#[expect(
    clippy::print_stderr,
    reason = "the app installs no log subscriber outside `__serve`; stderr is where a failed window is seen"
)]
fn open_workspace(cx: &mut App) {
    let bounds = Bounds::centered(None, size(px(1600.), px(1000.)), cx);
    if let Err(error) = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(860.), px(600.))),
            titlebar: Some(TitlebarOptions {
                title: Some("Möbius".into()),
                appears_transparent: true,
                traffic_light_position: Some(point(px(16.), px(16.))),
            }),
            // The titlebar band drags and zooms itself, so clicks on its buttons never zoom.
            app_owns_titlebar_drag: true,
            ..Default::default()
        },
        |window, cx| {
            let view = cx.new(|cx| ui::Workspace::new(window, cx));
            view.read(cx).focus_handle(cx).focus(window, cx);
            // The workspace paints its own frame, so a window background can show through.
            cx.new(|cx| Root::new(view, window, cx).bg(transparent_black()))
        },
    ) {
        eprintln!("Could not open Möbius: {error}");
        cx.quit();
    }
}
