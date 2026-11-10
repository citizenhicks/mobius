//! Exercise the real transcript through mouse selection and unrelated redraws.

use super::*;
use crate::ui::chat::Scene;

pub(super) const SCENARIOS: &[Scenario] = &[("chat-selection", select_transcript)];

const PROSE: &str = "selection:assistant:done:FinalAnswer";
const EXPECTED: &str = "The pills row is tightened.";

#[expect(
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "a self-checking scenario: a failed step or missing probe is the failure report, as in tests"
)]
fn select_transcript(this: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    open_chat(this, window, cx);
    assert!(
        matches!(this.page_tabs.first(), Some(PageTab::Chat(_))),
        "chat-selection needs an existing chat; set MOBIUS_SNAPSHOT_SESSION"
    );
    cx.spawn_in(window, async move |this, cx| {
        let mut ready = false;
        for _ in 0..100 {
            wait(cx.background_executor(), 100).await;
            ready = this
                .update_in(cx, |this, _, cx| {
                    if !this.active_session_ready() {
                        return false;
                    }
                    this.apply_fixture(Scene::Transcript, cx);
                    true
                })
                .expect("selection fixture window");
            if ready {
                break;
            }
        }
        assert!(ready, "chat-selection could not open the fixture chat");
        cx.update(|window, cx| window.draw(cx).clear(cx))
            .expect("initial transcript frame");
        wait(cx.background_executor(), 32).await;
        let bounds = cx
            .update(|window, cx| {
                window.draw(cx).clear(cx);
                PROBES.with(|probes| probes.borrow()[PROSE])
            })
            .expect("selection text bounds");
        let start = point(bounds.left(), bounds.top() + px(10.));
        let end = point(bounds.right() - px(1.), start.y);
        cx.update(|window, cx| {
            move_pointer(start, None, window, cx);
            window.draw(cx).clear(cx);
            window.dispatch_event(
                PlatformInput::MouseDown(MouseDownEvent {
                    button: MouseButton::Left,
                    position: start,
                    modifiers: Modifiers::default(),
                    click_count: 1,
                    first_mouse: false,
                }),
                cx,
            );
            move_pointer(end, Some(MouseButton::Left), window, cx);
            window.dispatch_event(
                PlatformInput::MouseUp(MouseUpEvent {
                    button: MouseButton::Left,
                    position: end,
                    modifiers: Modifiers::default(),
                    click_count: 1,
                }),
                cx,
            );
        })
        .expect("select fixture text");
        for frame in 0..6 {
            this.update_in(cx, |_, _, cx| cx.notify())
                .expect("unrelated workspace update");
            cx.update(|window, cx| window.draw(cx).clear(cx))
                .expect("selection frame");
            // finish_frame runs after paint and must not sweep the selected text.
            wait(cx.background_executor(), 16).await;
            cx.update(|window, cx| {
                assert_eq!(selected_text(window, cx).trim(), EXPECTED, "frame {frame}");
                if frame == 1 || frame == 3 {
                    move_pointer(point(px(1.), px(1.)), None, window, cx);
                } else if frame == 2 {
                    move_pointer(end, None, window, cx);
                }
            })
            .expect("selection survives redraw and pointer movement");
        }
        cx.update(|window, cx| {
            let clipboard = cx.read_from_clipboard();
            window.dispatch_keystroke(Keystroke::parse("cmd-c").expect("copy shortcut"), cx);
            let copied = cx.read_from_clipboard().and_then(|item| item.text());
            cx.write_to_clipboard(
                clipboard.unwrap_or_else(|| ClipboardItem::new_string(String::new())),
            );
            assert_eq!(copied.as_deref(), Some(EXPECTED));
        })
        .expect("copy selected fixture text");
        eprintln!("chat-selection: selection and copy survived six unrelated frames");
    })
    .detach();
}

fn move_pointer(
    position: Point<Pixels>,
    pressed_button: Option<MouseButton>,
    window: &mut Window,
    cx: &mut App,
) {
    window.dispatch_event(
        PlatformInput::MouseMove(MouseMoveEvent {
            position,
            pressed_button,
            modifiers: Modifiers::default(),
        }),
        cx,
    );
}

fn selected_text(window: &mut Window, cx: &mut App) -> String {
    gpui_base::TextSelection::selected_text(window, cx)
}
