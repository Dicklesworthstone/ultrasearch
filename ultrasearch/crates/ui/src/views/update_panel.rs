use crate::actions::{CheckForUpdates, DownloadUpdate, RestartToUpdate, ToggleUpdateOptIn};
use crate::model::state::{SearchAppModel, UpdateStatus};
use crate::theme;
use gpui::*;

pub struct UpdatePanel {
    model: Entity<SearchAppModel>,
}

impl UpdatePanel {
    pub fn new(model: Entity<SearchAppModel>, _cx: &mut Context<Self>) -> Self {
        Self { model }
    }
}

impl Render for UpdatePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = theme::active_colors(cx);
        let model = self.model.read(cx);
        let updates = &model.updates;

        let status_text = match &updates.status {
            UpdateStatus::Idle => "Up to date".into(),
            UpdateStatus::Checking => "Checking for updates…".into(),
            UpdateStatus::NeedsOptIn => "Enable update checks to proceed".into(),
            UpdateStatus::Available { version, .. } => format!("Update available: {version}"),
            UpdateStatus::Downloading { version, progress } => {
                format!("Downloading {version}… {progress}%")
            }
            UpdateStatus::ReadyToRestart { version, .. } => {
                format!("Downloaded {version}. Ready to restart.")
            }
            UpdateStatus::Restarting => "Restarting to apply update…".into(),
        };

        let notes = match &updates.status {
            UpdateStatus::Available { notes, .. } => Some(notes.clone()),
            UpdateStatus::ReadyToRestart { notes, .. } => Some(notes.clone()),
            _ => None,
        };

        let opt_in_label = if updates.opt_in {
            "Automatic update checks: ON"
        } else {
            "Automatic update checks: OFF"
        };

        let show_download = matches!(updates.status, UpdateStatus::Available { .. });
        let show_restart = matches!(updates.status, UpdateStatus::ReadyToRestart { .. });

        let mut root = div()
            .bg(colors.panel_bg)
            .border_1()
            .border_color(colors.border)
            .rounded_md()
            .p_3()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_weight(FontWeight::BOLD)
                            .child("Updates"),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(colors.text_secondary)
                            .child(status_text),
                    ),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(colors.text_primary)
                    .child(opt_in_label),
            )
            .child(
                div().flex().gap_2().children(
                    [
                        Some(
                            div()
                                .px_3()
                                .py_1p5()
                                .rounded_md()
                                .bg(colors.match_highlight)
                                .text_color(colors.bg)
                                .cursor_pointer()
                                .child("Check for Updates")
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _, _, cx| {
                                        this.model.update(cx, |m, cx| m.check_for_updates(cx));
                                        cx.dispatch_action(&CheckForUpdates);
                                    }),
                                ),
                        ),
                        Some(
                            div()
                                .px_3()
                                .py_1p5()
                                .rounded_md()
                                .border_1()
                                .border_color(colors.border)
                                .text_color(colors.text_primary)
                                .cursor_pointer()
                                .child(if updates.opt_in {
                                    "Disable Auto"
                                } else {
                                    "Enable Auto"
                                })
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, _, cx| {
                                        this.model.update(cx, |m, cx| {
                                            m.set_update_opt_in(!m.updates.opt_in, cx);
                                        });
                                        cx.dispatch_action(&ToggleUpdateOptIn);
                                    }),
                                ),
                        ),
                        show_download.then(|| {
                            div()
                                .px_3()
                                .py_1p5()
                                .rounded_md()
                                .bg(colors.panel_bg)
                                .border_1()
                                .border_color(colors.match_highlight)
                                .text_color(colors.match_highlight)
                                .cursor_pointer()
                                .child("Download and Install")
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _, _, cx| {
                                        this.model.update(cx, |m, cx| m.start_update_download(cx));
                                        cx.dispatch_action(&DownloadUpdate);
                                    }),
                                )
                        }),
                        show_restart.then(|| {
                            div()
                                .px_3()
                                .py_1p5()
                                .rounded_md()
                                .bg(colors.match_highlight)
                                .text_color(colors.bg)
                                .cursor_pointer()
                                .child("Restart to Update")
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _, _, cx| {
                                        this.model.update(cx, |m, cx| m.restart_to_update(cx));
                                        cx.dispatch_action(&RestartToUpdate);
                                    }),
                                )
                        }),
                    ]
                    .into_iter()
                    .flatten(),
                ),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(colors.text_secondary)
                    .child(
                        "Tip: Enable auto-checks to unlock updates. Download pulls the patch; Restart applies it and reopens UltraSearch.",
                    ),
            );

        if let Some(notes) = notes {
            root = root.child(
                div()
                    .mt_2()
                    .p_2()
                    .bg(colors.bg)
                    .border_1()
                    .border_color(colors.border)
                    .rounded_md()
                    .text_size(px(12.))
                    .text_color(colors.text_primary)
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(FontWeight::BOLD)
                            .child("Release notes"),
                    )
                    .child(div().child(notes)),
            );
        }

        root
    }
}
