//! Both buttons dispatch into the real composer. Only the microphone samples and upstream provider are fixtures.
use super::probes::{PROBES, click, wait};
use crate::ui::Workspace;
use crate::ui::router::{PageTab, ProviderPart};
use gpui::{AsyncWindowContext, WeakEntity};
use mobius_gateway::wire::ProviderModelKind;
use std::path::Path;
use std::time::{Duration, Instant};

fn press(name: &str, cx: &mut AsyncWindowContext) -> gpui::Result<()> {
    let bounds = PROBES
        .with(|items| items.borrow().get(name).copied())
        .ok_or_else(|| anyhow::anyhow!("missing control {name}"))?;
    eprintln!("Clicked {name} at {:?}", bounds.center());
    cx.update(|window, cx| {
        click(bounds.center(), window, cx);
        window.draw(cx).clear(cx);
    })
}

/// Clicks `part`'s card on setup `instance`'s grid and waits out the zoom into its page.
#[expect(
    clippy::future_not_send,
    reason = "native controls live on the GPUI foreground executor"
)]
async fn open_card(
    instance: &str,
    part: ProviderPart,
    cx: &mut AsyncWindowContext,
) -> gpui::Result<()> {
    let page = PageTab::ProviderPart(instance.to_owned(), part);
    press(&format!("card:page:{page:?}"), cx)?;
    settle(cx).await
}

/// Goes back to the setup's grid, as the titlebar's Back does.
#[expect(
    clippy::future_not_send,
    reason = "native controls live on the GPUI foreground executor"
)]
async fn back_to_grid(
    view: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
) -> gpui::Result<()> {
    view.update_in(cx, |this, window, cx| {
        this.navigate_history(false, window, cx);
    })?;
    settle(cx).await
}

/// A card's zoom takes no clicks; the next step waits for it to land.
#[expect(
    clippy::future_not_send,
    reason = "native controls live on the GPUI foreground executor"
)]
async fn settle(cx: &mut AsyncWindowContext) -> gpui::Result<()> {
    wait(cx.background_executor(), 500).await;
    cx.update(|window, cx| window.draw(cx).clear(cx))
}

fn capture(file: &Path, cx: &mut AsyncWindowContext) -> gpui::Result<()> {
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
        window.render_to_image()
    })??
    .save(file)?;
    Ok(())
}

#[expect(
    clippy::future_not_send,
    reason = "the native UI exercise runs on the GPUI foreground executor"
)]
pub(super) async fn exercise(
    dir: &Path,
    view: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
) -> gpui::Result<()> {
    let trace = dir.join("actions.txt");
    let expected = std::env::var("MOBIUS_SNAPSHOT_TRANSCRIPT")?;
    let partial = std::env::var("MOBIUS_SNAPSHOT_PARTIAL").ok();
    press("composer-dictate", cx)?;
    std::fs::write(&trace, "Clicked Dictate\n")?;
    wait(cx.background_executor(), 250).await;
    let deadline = Instant::now() + Duration::from_secs(8);
    while !PROBES.with(|items| items.borrow().contains_key("composer-dictation-stop")) {
        cx.update(|window, cx| window.draw(cx).clear(cx))?;
        if Instant::now() >= deadline {
            cx.update(|window, app| {
                window.draw(app).clear(app);
                window.render_to_image()
            })??
            .save(dir.join("failed.png"))?;
            anyhow::bail!("dictation controls did not appear");
        }
        wait(cx.background_executor(), 50).await;
    }
    if let Some(partial) = partial {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !view.read_with(cx, |this, cx| {
            this.composer.read(cx).value().ends_with(&partial)
        })? {
            anyhow::ensure!(
                Instant::now() < deadline,
                "live partial did not reach the composer"
            );
            wait(cx.background_executor(), 100).await;
        }
        cx.update(|window, app| {
            window.draw(app).clear(app);
            window.render_to_image()
        })??
        .save(dir.join("partial.png"))?;
    }
    press("composer-dictation-stop", cx)?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let full = format!("Draft: {expected}");
    while !view.read_with(cx, |this, cx| {
        !this.dictating && this.composer.read(cx).value().as_str() == full
    })? {
        anyhow::ensure!(
            Instant::now() < deadline,
            "expected final transcript did not reach the draft"
        );
        wait(cx.background_executor(), 100).await;
    }
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
        window.render_to_image()
    })??
    .save(dir.join("final.png"))?;
    std::fs::write(
        trace,
        format!("Clicked Dictate\nClicked Stop dictation\nComposer contains {full}\n"),
    )?;
    Ok(())
}

#[expect(
    clippy::future_not_send,
    reason = "native controls live on the GPUI foreground executor"
)]
pub(super) async fn edit_endpoint(
    dir: &Path,
    view: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
) -> gpui::Result<()> {
    let path = std::env::var("MOBIUS_SNAPSHOT_ENDPOINT_PATH")?;
    let instance = std::env::var("MOBIUS_SNAPSHOT_PROVIDER")?;
    let dictation = ProviderPart::Models(ProviderModelKind::Dictation);
    capture(&dir.join("model-groups.png"), cx)?;
    open_card(&instance, dictation, cx).await?;
    capture(&dir.join("inherited-group-endpoint.png"), cx)?;
    model_template(dir, view, cx).await?;
    edit_text(
        "provider:Dictation:endpoint",
        "missing/transcription-probe",
        cx,
    )?;
    press("test:Dictation", cx)?;
    test_result("failed", view, cx).await?;
    capture(&dir.join("failed-test.png"), cx)?;
    back_to_grid(view, cx).await?;
    capture(&dir.join("failed-test-card.png"), cx)?;
    open_card(&instance, dictation, cx).await?;
    edit_text("provider:Dictation:endpoint", &path, cx)?;
    test_result("idle", view, cx).await?;
    capture(&dir.join("edited-group-endpoint.png"), cx)?;
    press("test:Dictation", cx)?;
    test_result("passed", view, cx).await?;
    anyhow::ensure!(
        view.read_with(cx, |this, _| this.provider_record(&instance).is_some_and(
            |record| {
                !record
                    .model_groups
                    .iter()
                    .filter(|g| g.kind == ProviderModelKind::Dictation)
                    .flat_map(|g| &g.fields)
                    .any(|f| {
                        f.setting.id == "endpoint"
                            && matches!(&f.value,
                        mobius::protocol::FrontendSettingValue::String(value) if value == &path)
                    })
            }
        ))?,
        "Testing an unsaved draft changed the stored endpoint"
    );
    capture(&dir.join("unsaved-group-tested.png"), cx)?;
    model_settings(dir, &instance, view, cx).await?;
    press("provider-save", cx)?;
    let deadline = Instant::now() + Duration::from_secs(20);
    let effective = loop {
        let saved = view.read_with(cx, |this, _| {
            this.provider_record(&instance).and_then(|record| {
                let group = record
                    .model_groups
                    .iter()
                    .find(|g| g.kind == ProviderModelKind::Dictation)?;
                let saved = group.fields.iter().any(|f| {
                    f.setting.id == "endpoint"
                        && matches!(&f.value,
                    mobius::protocol::FrontendSettingValue::String(value) if value == &path)
                });
                (saved && group.models.len() >= 2).then(|| {
                    format!(
                        "{path} ({})",
                        group
                            .models
                            .iter()
                            .map(|m| m.id.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })
            })
        })?;
        if let Some(saved) = saved {
            break saved;
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "Gateway did not acknowledge the edited group endpoint"
        );
        wait(cx.background_executor(), 100).await;
    };
    capture(&dir.join("saved-group-endpoint.png"), cx)?;
    std::fs::write(dir.join("endpoint.txt"), effective)?;
    Ok(())
}

/// Opens the first Language model's settings, and the first Image model's when asked.
#[expect(
    clippy::future_not_send,
    reason = "native controls live on the GPUI foreground executor"
)]
async fn model_settings(
    dir: &Path,
    instance: &str,
    view: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
) -> gpui::Result<()> {
    back_to_grid(view, cx).await?;
    open_card(
        instance,
        ProviderPart::Models(ProviderModelKind::Language),
        cx,
    )
    .await?;
    press("provider:Language:model:0:settings", cx)?;
    wait(cx.background_executor(), 300).await;
    capture(&dir.join("expanded-language-settings.png"), cx)?;
    if std::env::var_os("MOBIUS_SNAPSHOT_IMAGE_MODEL").is_some() {
        back_to_grid(view, cx).await?;
        open_card(instance, ProviderPart::Models(ProviderModelKind::Image), cx).await?;
        press("provider:Image:model:0:settings", cx)?;
        wait(cx.background_executor(), 250).await;
        capture(&dir.join("image-model-settings.png"), cx)?;
    }
    Ok(())
}

fn edit_text(control: &str, value: &str, cx: &mut AsyncWindowContext) -> gpui::Result<()> {
    press(control, cx)?;
    cx.update(|window, cx| -> gpui::Result<()> {
        window.dispatch_keystroke(gpui::Keystroke::parse("cmd-a")?, cx);
        window.dispatch_keystroke(
            gpui::Keystroke {
                key_char: Some(value.to_owned()),
                ..Default::default()
            },
            cx,
        );
        window.draw(cx).clear(cx);
        Ok(())
    })??;
    Ok(())
}
#[expect(
    clippy::future_not_send,
    reason = "observes the live foreground editor"
)]
async fn test_result(
    expected: &str,
    view: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
) -> gpui::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(35);
    loop {
        if view.read_with(cx, |this, _| {
            this.snapshot_provider_test(ProviderModelKind::Dictation) == expected
        })? {
            cx.update(|window, cx| window.draw(cx).clear(cx))?;
            return Ok(());
        }
        anyhow::ensure!(Instant::now() < deadline, "Group never reached {expected}");
        wait(cx.background_executor(), 100).await;
    }
}

#[expect(
    clippy::future_not_send,
    reason = "native input and layout require the foreground executor"
)]
async fn model_template(
    dir: &Path,
    view: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
) -> gpui::Result<()> {
    press("provider:Dictation:add-model", cx)?;
    for (id, mode, filename) in [
        ("unknown-mac-model", "After stop", "new-batch-model"),
        ("gpt-live-transcribe", "Live", "new-live-model"),
    ] {
        cx.update(|window, cx| -> gpui::Result<()> {
            window.dispatch_keystroke(gpui::Keystroke::parse("cmd-a")?, cx);
            window.dispatch_keystroke(
                gpui::Keystroke {
                    key_char: Some(id.to_owned()),
                    ..Default::default()
                },
                cx,
            );
            Ok(())
        })??;
        let deadline = Instant::now() + Duration::from_secs(3);
        while !view.read_with(cx,|this,cx|this.snapshot_provider_draft(cx).and_then(|mut form|form.groups.remove(&ProviderModelKind::Dictation)).and_then(|mut group|group.models.pop()).is_some_and(|model|
            model.id==id && model.values.values().any(|value|matches!(value,mobius::protocol::FrontendSettingValue::String(text) if text.eq_ignore_ascii_case(mode)))))? {
            anyhow::ensure!(Instant::now()<deadline,"Unsaved {id} did not show {mode}");
            wait(cx.background_executor(),50).await;
        }
        capture(&dir.join(format!("{filename}.png")), cx)?;
    }
    press("provider:Dictation:model:2:remove", cx)
}

#[expect(
    clippy::future_not_send,
    reason = "exercises foreground native controls"
)]
pub(super) async fn new_provider(
    dir: &Path,
    view: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
) -> gpui::Result<()> {
    let name = "Unsaved macOS provider";
    let dictation = ProviderPart::Models(ProviderModelKind::Dictation);
    let id = view
        .read_with(cx, |this, cx| {
            this.snapshot_provider_draft(cx).map(|f| f.config.instance)
        })?
        .ok_or_else(|| anyhow::anyhow!("Missing unsaved form"))?;
    open_card(&id, ProviderPart::General, cx).await?;
    edit_text("provider-name", name, cx)?;
    edit_text("provider-base", &std::env::var("MOBIUS_SNAPSHOT_BASE")?, cx)?;
    edit_text("provider-key", "fixture-new-key", cx)?;
    back_to_grid(view, cx).await?;
    open_card(&id, dictation, cx).await?;
    press("provider:Dictation:add-model", cx)?;
    cx.update(|window, cx| {
        window.dispatch_keystroke(
            gpui::Keystroke {
                key_char: Some("whisper-1".into()),
                ..Default::default()
            },
            cx,
        )
    })?;
    press("test:Dictation", cx)?;
    test_result("passed", view, cx).await?;
    anyhow::ensure!(
        view.read_with(cx, |this, _| this.provider_record(&id).is_none())?,
        "Test persisted new provider"
    );
    capture(&dir.join("unsaved-credential-test.png"), cx)?;
    press("provider-save", cx)?;
    let deadline = Instant::now() + Duration::from_secs(15);
    while !view.read_with(cx, |this, _| {
        this.provider_record(&id).is_some_and(|r| {
            r.configured
                && r.label == name
                && r.model_groups
                    .iter()
                    .find(|g| g.kind == ProviderModelKind::Dictation)
                    .is_some_and(
                        |g| matches!(g.models.as_slice(), [model] if model.id == "whisper-1"),
                    )
        })
    })? {
        anyhow::ensure!(
            Instant::now() < deadline,
            "New provider with the submitted key was not saved"
        );
        wait(cx.background_executor(), 75).await;
    }
    // Saving a new setup lands on its grid.
    settle(cx).await?;
    open_card(&id, dictation, cx).await?;
    press("test:Dictation", cx)?;
    test_result("passed", view, cx).await?;
    capture(&dir.join("saved-credential-test.png"), cx)?;
    std::fs::write(
        dir.join("actions.txt"),
        "Tested draft using the entered API key; no provider was stored. Saved provider, verified its model, and tested again with the saved credential.\n",
    )?;
    Ok(())
}
