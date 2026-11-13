//! Chat scenarios: composer, pickers, pills and transcript fixtures. No message is sent;
//! the transcript scenarios ask the gateway for subagent and voice transcripts, which only read.

use super::*;
use crate::ui::chat::{ChatDetail, Scene};
use crate::ui::runtime::Link;
use pages::bots::AgentScope;

#[expect(
    clippy::expect_used,
    reason = "fixed scenario identities must be valid"
)]
pub(super) const SCENARIOS: &[Scenario] = &[
    ("decisions-pending", |this, window, cx| {
        decision_fixture(
            this,
            window,
            cx,
            mobius::protocol::FrontendBlockState::Pending,
        );
    }),
    ("decisions-resolved", |this, window, cx| {
        decision_fixture(
            this,
            window,
            cx,
            mobius::protocol::FrontendBlockState::Complete,
        );
    }),
    ("chat", open_chat),
    // The message rail with a tick under the pointer: its falloff and preview card.
    ("chat-rail-hover", |this, window, cx| {
        open_chat(this, window, cx);
        Workspace::when_chat_ready(window, cx, |this, _, cx| this.fixture_hover_rail(2, cx));
    }),
    // Pinned answers on the rail, the first one's card, and its Unpin action in view.
    ("chat-rail-pinned", |this, window, cx| {
        open_chat(this, window, cx);
        Workspace::when_chat_ready(window, cx, |this, _, cx| this.fixture_pin_answers(cx));
    }),
    // The toast: a tone glyph and the message, no title or close button.
    ("toast", |this, window, cx| {
        this.new_chat(window, cx);
        this.announce(Tone::Error, "The gateway disconnected.".into());
        this.next_toast(cx);
    }),
    ("chat-draft-idle", |this, window, cx| {
        this.new_chat(window, cx);
        window.focus(&this.focus, cx);
    }),
    ("chat-draft-focused", |this, window, cx| {
        this.new_chat(window, cx);
        this.focus_composer(window, cx);
    }),
    ("chat-draft-bot-picker", |this, window, cx| {
        this.new_chat(window, cx);
        click_probe(
            "chat-draft-bot".into(),
            height::REGULAR / 2.,
            MouseButton::Left,
            window,
            cx,
        );
    }),
    ("chat-typed", |this, window, cx| {
        this.new_chat(window, cx);
        this.fixture_type("Hello", window, cx);
        this.focus_composer(window, cx);
    }),
    ("chat-model-popover", |this, window, cx| {
        this.new_chat(window, cx);
        this.fixture_model_picker(false, cx);
    }),
    ("chat-model-list", |this, window, cx| {
        this.new_chat(window, cx);
        this.fixture_model_picker(true, cx);
    }),
    ("chat-model-long-name", |this, window, cx| {
        super::bots::media_fixture(this);
        if let Some(gateway) = &mut this.client.state.gateway {
            for provider in &mut gateway.providers {
                for model in &mut provider.models {
                    model.label = "deepseek/deepseek-v4.1-flash-1017".into();
                }
            }
            for model in &mut gateway.agent_catalog.models {
                if model.reasoning_effort.as_deref() == Some("high") {
                    model.variant_label = Some("max".into());
                }
            }
        }
        this.set_link(Link::Ready);
        this.new_chat(window, cx);
        this.fixture_model_picker(false, cx);
        this.shell.notices.clear();
        this.shell.toast = None;
    }),
    ("chat-provider-error", |this, window, cx| {
        this.client.gateway.take();
        this.client.state.gateway = None;
        super::documents::open_demo(
            this,
            window,
            cx,
            "fixture-provider-error",
            "Provider error",
            crate::gateway::TranscriptStore::default(),
        );
        this.set_link(Link::Ready);
        this.fixture_provider_issue(cx);
    }),
    ("perf-draft", |this, window, cx| {
        this.new_chat(window, cx);
        window.focus(&this.focus, cx);
    }),
    ("perf-type", |this, window, cx| {
        open_chat(this, window, cx);
        this.focus_composer(window, cx);
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(2))
                .await;
            for count in 1..=20 {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(300))
                    .await;
                let typed = this.update_in(cx, |this, window, cx| {
                    let text = "typing".repeat(4);
                    this.fixture_type(text.get(..count).unwrap_or_default(), window, cx);
                });
                report("perf-type", typed);
            }
        })
        .detach();
    }),
    ("chat-session-card", |this, window, cx| {
        open_chat(this, window, cx);
        Workspace::when_chat_ready(window, cx, |this, window, cx| {
            let Some(id) = session_id(this) else {
                return;
            };
            this.open_chat_detail(ChatDetail::Context(id), window, cx);
        });
    }),
    // The live chat's approvals and questions, open in the Attention panel.
    ("chat-attention-live", |this, window, cx| {
        open_chat(this, window, cx);
        Workspace::when_chat_ready(window, cx, |this, window, cx| {
            let Some(id) = session_id(this) else {
                return;
            };
            this.open_chat_detail(ChatDetail::Attention(id), window, cx);
        });
    }),
    ("chat-session-card-context", |this, window, cx| {
        open_chat(this, window, cx);
        this.chat_ui.session_sections.context = true;
        Workspace::when_chat_ready(window, cx, |this, window, cx| {
            let Some(id) = session_id(this) else {
                return;
            };
            this.open_chat_detail(ChatDetail::Context(id), window, cx);
        });
    }),
    // Every section open over the fixture's context breakdown and output files.
    ("chat-session-card-sections", session_card_sections),
    ("chat-session-card-short", |this, window, cx| {
        window.resize(size(px(1000.), px(600.)));
        this.fixture_section_order(&[]);
        undo_after_capture(|this| this.fixture_section_order(&[]));
        fixture(this, window, cx, |this, window, cx| {
            show_session_sections(this, cx);
            check_short_session_card(window, cx);
        });
    }),
    // The pointer beside Environment's title reveals its drag handle.
    ("chat-session-card-handle", |this, window, cx| {
        fixture(this, window, cx, |this, window, cx| {
            show_session_sections(this, cx);
            point_at_section("environment", None, window, cx);
        });
    }),
    // A saved order with an unknown id and a repeat; Environment and Context follow.
    ("chat-session-card-order", |this, window, cx| {
        this.fixture_section_order(&["outputs", "retired", "totals", "outputs"]);
        undo_after_capture(|this| this.fixture_section_order(&[]));
        session_card_sections(this, window, cx);
    }),
    // Session dragged by its handle over Environment: the lifted header and the drop line.
    ("chat-session-card-drag", |this, window, cx| {
        fixture(this, window, cx, |this, window, cx| {
            show_session_sections(this, cx);
            point_at_section("totals", Some("environment"), window, cx);
        });
    }),
    ("chat-session-card-beside-tab", |this, window, cx| {
        open_chat(this, window, cx);
        Workspace::when_chat_ready(window, cx, |this, window, cx| {
            let Some(id) = session_id(this) else {
                return;
            };
            this.open_chat_detail(ChatDetail::Context(id), window, cx);
            bot_tab(this, window, cx);
        });
    }),
    ("chat-bot-detail", |this, window, cx| {
        open_chat(this, window, cx);
        Workspace::when_chat_ready(window, cx, |this, window, cx| {
            bot_tab(this, window, cx);
        });
    }),
    ("chat-bot-tab-picker", |this, window, cx| {
        open_chat(this, window, cx);
        Workspace::when_chat_ready(window, cx, |this, window, cx| {
            if let Some(id) = bot_tab(this, window, cx) {
                this.snapshot_agent_model_picker(&Rc::new(AgentScope::Bot(id)), true, cx);
            }
        });
    }),
    ("chat-bot-tab-composer-picker", |this, window, cx| {
        open_chat(this, window, cx);
        Workspace::when_chat_ready(window, cx, |this, window, cx| {
            if bot_tab(this, window, cx).is_some() {
                this.fixture_model_picker(false, cx);
            }
        });
    }),
    ("chat-fixture-transcript", |this, window, cx| {
        fixture(this, window, cx, |this, _, cx| {
            this.apply_fixture(Scene::Transcript, cx);
        });
    }),
    ("chat-session-message", |this, window, cx| {
        fixture(this, window, cx, |this, _, cx| {
            this.fixture_session_messages(false, false, cx);
        });
    }),
    ("chat-session-project-message", |this, window, cx| {
        fixture(this, window, cx, |this, _, cx| {
            this.fixture_project_session_message(cx);
        });
    }),
    ("chat-session-steer", |this, window, cx| {
        fixture(this, window, cx, |this, _, cx| {
            this.fixture_session_messages(true, false, cx);
        });
    }),
    ("chat-session-steer-expanded", |this, window, cx| {
        fixture(this, window, cx, |this, _, cx| {
            this.fixture_session_messages(true, true, cx);
        });
    }),
    ("chat-fixture-running", |this, window, cx| {
        fixture(this, window, cx, |this, _, cx| {
            this.apply_fixture(Scene::Running, cx);
        });
    }),
    ("chat-fixture-attention", |this, window, cx| {
        fixture(this, window, cx, |this, window, cx| {
            this.apply_fixture(Scene::Attention, cx);
            let Some(session) = session_id(this) else {
                return;
            };
            this.open_chat_detail(ChatDetail::Attention(session), window, cx);
        });
    }),
    ("chat-fixture-approval", |this, window, cx| {
        fixture(this, window, cx, |this, _, cx| {
            this.apply_fixture(Scene::Approval, cx);
        });
    }),
    ("chat-fixture-picker", |this, window, cx| {
        fixture(this, window, cx, |this, _, cx| {
            this.apply_fixture(Scene::Picker, cx);
        });
    }),
    ("chat-fixture-steer", |this, window, cx| {
        fixture(this, window, cx, |this, _, cx| {
            this.apply_fixture(Scene::Steer, cx);
        });
    }),
    ("chat-fixture-patch", |this, window, cx| {
        fixture(this, window, cx, |this, _, cx| {
            this.apply_fixture(Scene::Patch, cx);
        });
    }),
    ("chat-fixture-fork", |this, window, cx| {
        fixture(this, window, cx, |this, _, cx| {
            this.apply_fixture(Scene::Fork, cx);
        });
    }),
    ("chat-fixture-turn-changes", |this, window, cx| {
        fixture(this, window, cx, |this, _, cx| {
            this.apply_fixture(Scene::Transcript, cx);
            let transcript = &this.client.state.transcript;
            let diff = transcript.rows().find_map(|row| match &row.kind {
                mobius_gateway::wire::TranscriptRowKind::Entry {
                    turn_changes: Some(changes),
                } => Some(
                    transcript
                        .diffs(&changes.diffs)
                        .filter_map(crate::gateway::TranscriptEntry::block)
                        .map(|block| block.text.as_str())
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                _ => None,
            });
            if let Some(diff) = diff {
                this.open_turn_changes(&diff, cx);
            }
        });
    }),
    ("chat-dictation", |this, window, cx| {
        fixture(this, window, cx, |this, _, cx| {
            this.apply_fixture(Scene::Dictation, cx);
        });
    }),
    ("chat-voice", |this, window, cx| {
        fixture(this, window, cx, |this, _, cx| {
            this.apply_fixture(Scene::Voice, cx);
        });
    }),
    ("chat-subagents", |this, window, cx| {
        open_chat(this, window, cx);
        Workspace::when_chat_ready(window, cx, |this, window, cx| {
            let Some(session) = session_id(this) else {
                return;
            };
            let key = "subagents:status".to_owned();
            this.open_chat_detail(ChatDetail::Widget { session, key }, window, cx);
        });
    }),
    ("chat-subagents-mixed", |this, window, cx| {
        fixture(this, window, cx, |this, window, cx| {
            this.apply_fixture(Scene::Transcript, cx);
            this.snapshot_mixed_subagents(cx);
            let Some(session) = session_id(this) else {
                return;
            };
            let key = "subagents:status".to_owned();
            this.open_chat_detail(ChatDetail::Widget { session, key }, window, cx);
        });
    }),
    ("chat-subagent-transcript", |this, window, cx| {
        open_chat(this, window, cx);
        Workspace::when_chat_ready(window, cx, |this, _, cx| {
            this.fixture_widget_command("subagents:status", cx);
        });
    }),
    ("chat-subagent-preview", |this, window, cx| {
        fixture(this, window, cx, |this, _, cx| {
            this.apply_fixture(Scene::Transcript, cx);
            let Some(session) = session_id(this) else {
                return;
            };
            if let Some(preview) = this
                .client
                .state
                .previews
                .get("fixture-subagent")
                .map(Rc::clone)
            {
                this.open_preview_tab(&session, preview, cx);
            }
        });
    }),
    ("chat-voice-transcript", |this, window, cx| {
        open_chat(this, window, cx);
        Workspace::when_chat_ready(window, cx, |this, _, cx| {
            this.fixture_widget_command("voice:voice", cx);
        });
    }),
    ("chat-reply", |this, window, cx| {
        open_chat(this, window, cx);
        Workspace::when_chat_ready(window, cx, |this, window, cx| {
            let last = this
                .client
                .state
                .transcript
                .entries()
                .rev()
                .find_map(|entry| {
                    let target = *entry.message_target()?;
                    match &entry.content {
                        mobius_gateway::wire::TranscriptItemContent::Assistant { text, .. } => {
                            Some((target, text.clone())) // rules-allow: the reply owns its quote while the transcript keeps the answer
                        }
                        _ => None,
                    }
                });
            if let Some((target, text)) = last {
                let author = this
                    .composer_bot()
                    .map_or_else(|| "the Bot".to_owned(), |bot| bot.name.to_string());
                this.begin_reply(target, text, author, window, cx);
            }
        });
    }),
    ("chat-changes", |this, window, cx| {
        open_chat(this, window, cx);
        Workspace::when_chat_ready(window, cx, |this, _, cx| {
            this.open_git_diff(mobius_gateway::wire::GitDiffScope::Unstaged, cx);
        });
    }),
    ("chat-reasoning-toggle", |this, window, cx| {
        fixture(this, window, cx, |this, window, cx| {
            this.apply_long_fixture(Some(6), window, cx);
        });
    }),
    ("chat-pills-scrolled", |this, window, cx| {
        fixture(this, window, cx, |this, window, cx| {
            this.apply_long_fixture(Some(9), window, cx);
        });
    }),
    ("chat-reasoning-toggle-tail", |this, window, cx| {
        fixture(this, window, cx, |this, window, cx| {
            this.apply_long_fixture(None, window, cx);
        });
    }),
    ("chat-reasoning-toggle-live", |this, window, cx| {
        open_chat(this, window, cx);
        Workspace::when_chat_ready(window, cx, |this, window, cx| {
            this.fixture_reveal_reasoning(None, window, cx);
        });
    }),
    ("chat-loading", |this, window, cx| {
        this.activate_tab(PageTab::Chat(
            SessionId::new("fixture-loading").expect("fixture session"),
        ));
        this.focus_composer(window, cx);
        window.focus(&this.focus, cx);
    }),
    ("chat-failed", |this, window, cx| {
        this.open_page(
            PageTab::Chat(SessionId::new("missing").expect("fixture session")),
            window,
            cx,
        );
        window.focus(&this.focus, cx);
    }),
];

fn decision_fixture(
    this: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    state: mobius::protocol::FrontendBlockState,
) {
    use mobius::protocol::FrontendBlockState;
    this.client.gateway.take();
    super::documents::open_demo(
        this,
        window,
        cx,
        "decisions",
        "Testing Question Tool",
        crate::gateway::TranscriptStore::default(),
    );
    this.set_link(Link::Ready);
    let pending = state == FrontendBlockState::Pending;
    if let Some(session) = &mut this.client.state.session {
        session.active_turn_ids = if pending {
            vec!["fixture-live".into()]
        } else {
            Vec::new()
        };
        session.pending_approvals = if pending {
            vec![mobius::protocol::ExecApprovalRequestEvent {
                id: "fixture-approval".into(),
                turn_id: "fixture-live".into(),
                reason: String::new(),
                calls: vec![mobius::protocol::ApprovalCall {
                    call_id: "call_2".into(),
                    name: "exec".into(),
                    arguments: serde_json::json!({"command": "rustup toolchain install 1.98.1"}),
                    preview: None,
                }],
            }]
        } else {
            Vec::new()
        };
        session.widgets.clear();
        session.widgets.push(mobius_gateway::wire::SessionWidget {
            capability: "fixture".into(),
            item: Arc::new(decision_widget(pending)),
        });
    }
    this.client.state.transcript = decision_transcript(state);
    this.shell.notices.clear();
    this.shell.toast = None;
}

fn decision_transcript(
    state: mobius::protocol::FrontendBlockState,
) -> crate::gateway::TranscriptStore {
    use mobius::protocol::{
        FrontendBlockRole, FrontendBlockState, MessageAuthor, MessageDelivery, MessageEvent,
        ModelStepContentPhase,
    };
    let pending = state == FrontendBlockState::Pending;
    let mut script = crate::gateway::fixture::Script::default();
    script.turn("fixture-live", true);
    let mut warning = mobius::protocol::FrontendBlock {
        id: Some("fixture-live/warning".into()),
        group: Some("fixture-live".into()),
        role: FrontendBlockRole::Activity,
        title: "Agent".into(),
        text: "UserPromptSubmit hook exited with status 127: node: not found".into(),
        ..Default::default()
    };
    script.block("fixture", warning);
    script.message(
        "opening",
        MessageEvent {
            author: MessageAuthor::User,
            delivery: MessageDelivery::Turn,
            text: "Run the tests and tell me what broke.".into(),
            attachments: Vec::new(),
            reply: None,
            message_target: None,
        },
    );
    let question = "open";
    warning = mobius::protocol::FrontendBlock {
        id: Some(format!("fixture-live/{question}")),
        group: Some("fixture-live".into()),
        role: FrontendBlockRole::Tool,
        title: "ask_user".into(),
        text: "Which suite should I run?".into(),
        ..Default::default()
    };
    script.block("fixture", warning);
    script.block(
        "sandbox",
        mobius::protocol::FrontendBlock {
            id: Some("fixture-live/approval:fixture-approval".into()),
            group: Some("fixture-live".into()),
            state,
            role: FrontendBlockRole::Approval,
            title: if state == FrontendBlockState::Pending {
                "Approval required"
            } else {
                "Allowed once"
            }
            .into(),
            text: "exec cargo test --locked".into(),
            ..Default::default()
        },
    );
    script.text(
        "fixture-live",
        "answer",
        ModelStepContentPhase::FinalAnswer,
        "The build passes. Before I start the long run, which suite do you want?",
    );
    if !pending {
        script.turn("fixture-live", false);
    }
    let mut page = script.page();
    for row in &mut page.rows {
        if let mobius_gateway::wire::TranscriptRowKind::Worked {
            started_at_ms,
            duration_ms: None,
            ..
        } = &mut Arc::make_mut(row).kind
        {
            *started_at_ms = chrono::Utc::now().timestamp_millis() - 48_000;
        }
    }
    crate::gateway::TranscriptStore::from(page)
}

fn decision_widget(pending: bool) -> mobius::protocol::FrontendWidget {
    let mut widget = mobius::protocol::FrontendWidget::new(
        "attention",
        mobius::protocol::FrontendSlot::Attention,
        "1",
    );
    widget.content = Some(mobius::protocol::FrontendWidgetContent::ActionList {
        title: "Questions".into(),
        actions: Vec::new(),
        items: vec![mobius::protocol::FrontendActionListItem {
            id: "open".into(),
            text: "Which suite should I run?".into(),
            outcome: (!pending).then(|| "Only the Wire tests, then stop".into()),
            state: if pending {
                mobius::protocol::FrontendListItemState::Pending
            } else {
                mobius::protocol::FrontendListItemState::Completed
            },
            actions: if pending {
                ["Unit tests", "Full simulator suite"]
                    .into_iter()
                    .enumerate()
                    .map(|(i, label)| mobius::protocol::FrontendAction {
                        id: i.to_string(),
                        label: label.into(),
                        symbol: mobius::protocol::FrontendSymbol::Question,
                        tone: mobius::protocol::FrontendTone::Neutral,
                        input_from_label: true,
                        editor: None,
                        op: mobius::protocol::Op::CapabilityCommand {
                            capability: "fixture".into(),
                            command: "answer".into(),
                            arguments: "open".into(),
                            input: None,
                            target: None,
                        },
                    })
                    .collect()
            } else {
                Vec::new()
            },
        }],
    });
    widget
}

/// Scenarios whose probed reasoning line the harness clicks open and closed.
pub(super) const CLICKS: &[&str] = &[
    "chat-reasoning-toggle",
    "chat-reasoning-toggle-tail",
    "chat-reasoning-toggle-live",
];

/// Opens the chat's Bot settings in a side tab beside the composer.
fn bot_tab(
    this: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Arc<str>> {
    let id = Arc::clone(&this.composer_bot()?.id);
    this.open_chat_detail(ChatDetail::Bot(Arc::clone(&id)), window, cx);
    Some(id)
}

/// Opens the live chat, then lays a fixture over it once it is ready. Nothing is sent
/// beyond the read-only open.
fn fixture(
    this: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    apply: fn(&mut Workspace, &mut Window, &mut Context<Workspace>),
) {
    if this.client.gateway.is_none() || this.client.state.gateway.is_none() {
        this.client.gateway.take();
        this.client.state.gateway = None;
        super::documents::open_demo(
            this,
            window,
            cx,
            "fixture-chat",
            "Interface preview",
            crate::gateway::TranscriptStore::default(),
        );
        this.set_link(Link::Ready);
        apply(this, window, cx);
        return;
    }
    open_chat(this, window, cx);
    window.focus(&this.focus, cx);
    Workspace::when_chat_ready(window, cx, apply);
}

/// Every session card section open over the fixture's context breakdown and output files.
fn session_card_sections(this: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    fixture(this, window, cx, |this, _, cx| {
        show_session_sections(this, cx);
    });
}

fn show_session_sections(this: &mut Workspace, cx: &mut Context<Workspace>) {
    this.apply_fixture(Scene::Transcript, cx);
    this.fixture_chat_files();
    this.chat_ui.session_sections.context = true;
    let Some(id) = session_id(this) else {
        return;
    };
    this.fixture_show_session_card(&id, cx);
}

/// The last section remains reachable in the minimum-height window and moves by keyboard.
fn check_short_session_card(window: &Window, cx: &Context<Workspace>) {
    cx.spawn_in(window, async move |_, cx| {
        let bounds =
            |id: &str| PROBES.with(|probes| probes.borrow().get(id).copied().unwrap_or_default());
        wait(cx.background_executor(), 600).await;
        // The probe is a child, so its recorded origin moves with the scroll offset.
        let mut viewport = Bounds::default();
        let scrolled = cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            viewport = bounds("session-card-scroll");
            let totals = bounds("session-card-header:totals");
            assert!(
                totals.bottom() > viewport.bottom(),
                "fixture must overflow: viewport {viewport:?}, totals {totals:?}"
            );
            window.dispatch_event(
                PlatformInput::Scroll(ScrollEvent {
                    position: point(viewport.right() - space::XS, viewport.center().y),
                    delta: ScrollDelta::Pixels(point(px(0.), -px(1000.))),
                    modifiers: Modifiers::default(),
                    touch_phase: TouchPhase::Moved,
                    source: ScrollSource::Pointer,
                }),
                cx,
            );
        });
        report("scroll the short session card", scrolled);
        wait(cx.background_executor(), 100).await;
        let moved = cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            let totals = bounds("session-card-header:totals");
            assert!(
                totals.top() >= viewport.top() && totals.bottom() <= viewport.bottom(),
                "scrolling must reveal the last section: viewport {viewport:?}, totals {totals:?}"
            );
            click(bounds("session-card-handle:totals").center(), window, cx);
            window.dispatch_event(
                PlatformInput::KeyDown(gpui::KeyDownEvent {
                    keystroke: Keystroke {
                        key: "up".into(),
                        ..Keystroke::default()
                    },
                    is_held: false,
                    prefer_character_input: false,
                }),
                cx,
            );
        });
        report("move the last section by keyboard", moved);
        wait(cx.background_executor(), 300).await;
        let checked = cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            assert!(
                bounds("session-card-header:totals").top()
                    < bounds("session-card-header:outputs").top(),
                "the focused section must move up"
            );
        });
        report("check keyboard section order", checked);
    })
    .detach();
}

/// Once the fixture's card has slid in, rests the pointer on section `key`'s handle, or presses it
/// and drags over `drop_on`'s header, leaving the drag held for the capture.
fn point_at_section(
    key: &'static str,
    drop_on: Option<&'static str>,
    window: &Window,
    cx: &Context<Workspace>,
) {
    cx.spawn_in(window, async move |_, cx| {
        // The fixture has just landed: let the card slide in and Context open first.
        wait(cx.background_executor(), 600).await;
        let pointed = cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            let header = |key: &str| {
                let name = format!("session-card-header:{key}");
                let bounds = PROBES.with(|probes| probes.borrow().get(&name).copied());
                if bounds.is_none() {
                    eprintln!("snapshot: no probe {name}");
                }
                bounds
            };
            let Some(handle) = PROBES.with(|probes| {
                probes
                    .borrow()
                    .get(&format!("session-card-handle:{key}"))
                    .copied()
            }) else {
                return;
            };
            let handle = handle.center();
            let modifiers = Modifiers::default();
            let move_to = |position, pressed_button, window: &mut Window, cx: &mut App| {
                let moved = MouseMoveEvent {
                    position,
                    pressed_button,
                    modifiers,
                };
                window.dispatch_event(PlatformInput::MouseMove(moved), cx);
                window.draw(cx).clear(cx);
            };
            move_to(handle, None, window, cx);
            let Some(target) = drop_on.and_then(header) else {
                return;
            };
            let down = MouseDownEvent {
                button: MouseButton::Left,
                position: handle,
                modifiers,
                click_count: 1,
                first_mouse: false,
            };
            window.dispatch_event(PlatformInput::MouseDown(down), cx);
            // GPUI anchors the preview where the drag passes its threshold, as a hand
            // would just inside the handle.
            move_to(
                handle - point(px(3.), px(0.)),
                Some(MouseButton::Left),
                window,
                cx,
            );
            let to = point(handle.x - px(40.), target.center().y);
            for step in 1..=4u8 {
                let along = f32::from(step) / 4.;
                let at = point(
                    handle.x + (to.x - handle.x) * along,
                    handle.y + (to.y - handle.y) * along,
                );
                move_to(at, Some(MouseButton::Left), window, cx);
            }
        });
        report("point", pointed);
    })
    .detach();
}
