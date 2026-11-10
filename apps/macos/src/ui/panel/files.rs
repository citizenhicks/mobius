use super::*;
use mobius::SessionId;

impl Workspace {
    pub(in crate::ui) fn choose_attachments(&self, window: &Window, cx: &Context<Self>) {
        if !self.can_attach_files() {
            return;
        }
        let target = self.attachment_target();
        let epoch = self.client.epoch;
        let bot = self.draft_bot().map(|bot| Arc::clone(&bot.id));
        let workspace = self.draft_workspace.clone(); // rules-allow: the 'static prompt compares the draft's folder from when it opened
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach to message".into()),
        });
        cx.spawn_in(window, async move |this, cx| match paths.await {
            Ok(Ok(Some(paths))) => {
                let attached = this.update(cx, |this, cx| {
                    if this.client.epoch != epoch || paths.is_empty() {
                        return;
                    }
                    if let Some(target) = target {
                        this.attach_files(target, paths, cx);
                    } else if this.can_attach_files()
                        && this.draft_selected()
                        && this.draft_bot().map(|bot| &bot.id) == bot.as_ref()
                        && this.draft_workspace == workspace
                    {
                        this.create_draft(runtime::DraftIntent::Attachments(paths), cx);
                    } else {
                        this.report(
                            "The chat changed while choosing attachments. Choose the files again.",
                        );
                        cx.notify();
                    }
                });
                if let Err(error) = attached {
                    tracing::debug!(%error, "the workspace closed before the attachments were chosen");
                }
            }
            Ok(Err(error)) => report_prompt_error(&this, cx, &error),
            _ => {}
        })
        .detach();
    }

    pub(in crate::ui) fn choose_file(&self, window: &Window, cx: &Context<Self>) {
        let owner = TabOwner::of(&self.page_tabs);
        let epoch = self.client.epoch;
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Open in Möbius".into()),
        });
        cx.spawn_in(window, async move |this, cx| match paths.await {
            Ok(Ok(Some(paths))) => {
                let opened = this.update(cx, |this, cx| {
                    if this.client.epoch != epoch {
                        return;
                    }
                    for path in paths {
                        let id = path.to_string_lossy().into_owned();
                        let title = path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned();
                        this.open_file_preview_for(owner.clone(), id, title, path, cx); // rules-allow: each chosen file belongs to the same original chat
                    }
                });
                if let Err(error) = opened {
                    tracing::debug!(%error, "the workspace closed before the chosen files opened");
                }
            }
            Ok(Err(error)) => report_prompt_error(&this, cx, &error),
            _ => {}
        })
        .detach();
    }
}

/// Shows why the file prompt failed, unless the workspace closed meanwhile.
fn report_prompt_error(
    this: &WeakEntity<Workspace>,
    cx: &mut gpui::AsyncWindowContext,
    error: &anyhow::Error,
) {
    let reported = this.update(cx, |this, cx| {
        this.report(error.to_string());
        cx.notify();
    });
    if let Err(error) = reported {
        tracing::debug!(%error, "the workspace closed before a file prompt error could show");
    }
}

impl Workspace {
    pub(in crate::ui) fn download_file(
        &mut self,
        session_id: SessionId,
        file: mobius::protocol::SessionFileReference,
        cx: &mut Context<Self>,
    ) {
        let id: Arc<str> = format!("{session_id}:{}", file.id).into();
        let id = self
            .content
            .navigation
            .file_id(&TabOwner::Chat(session_id.share()), id);
        if self.content.tabs.contains_key(&id) {
            self.activate_tab(PageTab::Content(id));
            cx.notify();
            return;
        }
        let Some(directory) = &self.downloads else {
            self.report("Unable to create a private preview directory.");
            cx.notify();
            return;
        };
        let path = crate::ui::runtime::private_path(directory.path(), &file.name);
        if let Some(gateway) = &self.client.gateway
            && let Err(error) = gateway.download(session_id, file, path)
        {
            self.report(crate::gateway::describe(&error));
        }
        cx.notify();
    }
}
