use crate::actions::FinishOnboarding;
use crate::model::state::SearchAppModel;
use crate::theme;
use gpui::prelude::FluentBuilder;
use gpui::*;
use ipc;
use std::path::PathBuf;
use sysinfo::{DiskKind, Disks};
use uuid;

#[derive(Clone)]
struct DriveChoice {
    name: String,
    label: String,
    selected: bool,
    content_indexing: bool,
}

pub struct OnboardingView {
    step: usize,
    drives: Vec<DriveChoice>,
    focus_handle: FocusHandle,
    model: Entity<SearchAppModel>,
}

impl OnboardingView {
    pub fn new(model: Entity<SearchAppModel>, cx: &mut Context<Self>) -> Self {
        let mut drives = Vec::new();
        let sys_disks = Disks::new_with_refreshed_list();
        let mut fixed_ntfs_found = 0usize;
        for disk in sys_disks.list() {
            let mount = disk.mount_point().to_string_lossy().to_string();
            let fs = disk.file_system().to_string_lossy().to_string();
            let is_fixed = matches!(disk.kind(), DiskKind::HDD | DiskKind::SSD);
            let is_ntfs = fs.eq_ignore_ascii_case("ntfs");
            if is_fixed && is_ntfs {
                fixed_ntfs_found += 1;
                drives.push(DriveChoice {
                    name: mount.clone(),
                    label: format!("{mount} - NTFS"),
                    selected: true,
                    content_indexing: true,
                });
            }
        }

        if fixed_ntfs_found == 0 {
            drives.clear();
            for disk in sys_disks.list() {
                let mount = disk.mount_point().to_string_lossy().to_string();
                let fs = disk.file_system().to_string_lossy().to_string();
                let label = format!("{mount} - {}", fs.to_uppercase());
                drives.push(DriveChoice {
                    name: mount,
                    label,
                    selected: true,
                    content_indexing: true,
                });
            }
        }

        Self {
            step: 0,
            drives,
            focus_handle: cx.focus_handle(),
            model,
        }
    }

    fn next_step(&mut self, cx: &mut Context<Self>) {
        if self.step < 1 {
            self.step += 1;
            cx.notify();
        } else {
            self.finish(cx);
        }
    }

    fn prev_step(&mut self, cx: &mut Context<Self>) {
        if self.step > 0 {
            self.step -= 1;
            cx.notify();
        }
    }

    fn toggle_drive(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(drive) = self.drives.get_mut(index) {
            drive.selected = !drive.selected;
            cx.notify();
        }
    }

    fn toggle_content(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(drive) = self.drives.get_mut(index) {
            drive.content_indexing = !drive.content_indexing;
            cx.notify();
        }
    }

    fn finish(&mut self, cx: &mut Context<Self>) {
        if let Ok(mut config) = core_types::config::load_or_create_config(None) {
            let selected: Vec<_> = self
                .drives
                .iter()
                .filter(|d| d.selected)
                .map(|d| d.name.clone())
                .collect();
            let content_enabled: Vec<_> = self
                .drives
                .iter()
                .filter(|d| d.selected && d.content_indexing)
                .map(|d| d.name.clone())
                .collect();

            config.volumes = selected;
            config.content_index_volumes = content_enabled;
            // Telemetry is permanently disabled; enforce false regardless of UI.
            config.app.telemetry_opt_in = false;

            let target = PathBuf::from("config/config.toml");
            if let Ok(toml) = toml::to_string_pretty(&config) {
                let _ = std::fs::write(target, toml);
            }
        }

        let client = self.model.read(cx).client.clone();
        cx.spawn(|_, _cx: &mut AsyncApp| async move {
            let req = ipc::ReloadConfigRequest {
                id: uuid::Uuid::new_v4(),
            };
            let _ = client.reload_config(req).await;
        })
        .detach();

        // Give the user immediate feedback that indexing is starting.
        self.model.update(cx, |model, cx| {
            model.status.indexing_state = "Starting indexing…".to_string();
            cx.notify();
        });

        cx.dispatch_action(&FinishOnboarding);
    }

    fn render_progress(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = theme::active_colors(cx);
        let labels = ["Welcome", "Choose drives"];
        div()
            .flex()
            .items_center()
            .gap_3()
            .children(labels.iter().enumerate().map(|(i, label)| {
                let active = i == self.step;
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px(20.))
                            .h(px(20.))
                            .rounded_full()
                            .bg(if active {
                                colors.match_highlight
                            } else {
                                colors.border
                            })
                            .border_1()
                            .border_color(colors.border)
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(12.))
                            .text_color(if active {
                                colors.bg
                            } else {
                                colors.text_primary
                            })
                            .child(format!("{}", i + 1)),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(if active {
                                colors.text_primary
                            } else {
                                colors.text_secondary
                            })
                            .child(*label),
                    )
            }))
    }

    fn render_drive_row(
        &self,
        index: usize,
        drive: &DriveChoice,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = theme::active_colors(cx);
        let name_color = if drive.selected {
            colors.text_primary
        } else {
            colors.text_secondary
        };

        let checkbox = |checked: bool| {
            div()
                .w(px(22.))
                .h(px(22.))
                .rounded_full()
                .border_2()
                .border_color(if checked {
                    colors.match_highlight
                } else {
                    colors.border
                })
                .bg(if checked {
                    colors.match_highlight
                } else {
                    colors.panel_bg
                })
        };

        let toggle = |checked: bool| {
            let knob_x = if checked { px(18.) } else { px(2.) };
            div()
                .w(px(38.))
                .h(px(20.))
                .rounded_full()
                .bg(if checked {
                    colors.match_highlight
                } else {
                    colors.border
                })
                .child(
                    div()
                        .absolute()
                        .top(px(2.))
                        .left(knob_x)
                        .w(px(16.))
                        .h(px(16.))
                        .rounded_full()
                        .bg(colors.bg),
                )
        };

        div()
            .flex()
            .items_center()
            .justify_between()
            .gap_3()
            .p_3()
            .rounded_lg()
            .border_1()
            .border_color(colors.border)
            .bg(if drive.selected {
                colors.panel_bg
            } else {
                colors.bg
            })
            .hover(|s| s.bg(colors.divider))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .cursor_pointer()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, _, cx| {
                                    this.toggle_drive(index, cx);
                                }),
                            )
                            .child(checkbox(drive.selected)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .text_color(name_color)
                            .child(div().text_size(px(13.)).child(drive.label.clone()))
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(colors.text_secondary)
                                    .child(
                                        "Fixed NTFS drives recommended for USN + content indexing.",
                                    ),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(colors.text_secondary)
                            .child("Content"),
                    )
                    .child(
                        div()
                            .cursor_pointer()
                            .relative()
                            .when(drive.selected, |d: Div| {
                                d.on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, _, cx| {
                                        this.toggle_content(index, cx);
                                    }),
                                )
                            })
                            .opacity(if drive.selected { 1.0 } else { 0.4 })
                            .child(toggle(drive.content_indexing)),
                    ),
            )
    }
}

impl Render for OnboardingView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = theme::active_colors(cx);

        let drives = self
            .drives
            .iter()
            .enumerate()
            .map(|(i, d)| self.render_drive_row(i, d, cx))
            .collect::<Vec<_>>();
        let selected_count = self.drives.iter().filter(|d| d.selected).count();

        let body = match self.step {
            0 => div()
                .flex()
                .flex_col()
                .gap_4()
                .child(
                    div()
                        .text_size(px(28.))
                        .font_weight(FontWeight::BOLD)
                        .child("Welcome to UltraSearch"),
                )
                .child(
                    div()
                        .text_size(px(14.))
                        .text_color(colors.text_secondary)
                        .child(
                            "UltraSearch blends lightning-fast filename search with deep content indexing, native UI polish, and background-friendly resource use.",
                        ),
                )
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(colors.text_secondary)
                        .child("This three-step setup picks your drives, privacy stance, and starts the first index run."),
                )
                .child(
                    div()
                        .flex()
                        .gap_3()
                        .child(
                            div()
                                .px_3()
                                .py_2()
                                .rounded_lg()
                                .bg(colors.panel_bg)
                                .border_1()
                                .border_color(colors.border)
                                .child(
                                    div()
                                        .text_size(px(12.))
                                        .text_color(colors.text_secondary)
                                        .child("Tip: You can re-open this wizard later from Settings if you change drives."),
                                ),
                        ),
                ),
            1 => div()
                .flex()
                .flex_col()
                .gap_3()
                .child(div().text_size(px(22.)).font_weight(FontWeight::BOLD).child("Choose what to index"))
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(colors.text_secondary)
                        .child("Uncheck a drive to exclude it entirely. Toggle the content switch to control deep content indexing per drive."),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(if drives.is_empty() {
                            div()
                                .text_color(colors.text_secondary)
                                .text_size(px(12.))
                                .child("No drives detected. Connect a drive or continue to start with an empty set.")
                        } else {
                            div().children(drives)
                        }),
                )
                .when(selected_count == 0, |d: Div| {
                    d.child(
                        div()
                            .text_color(colors.text_secondary)
                            .text_size(px(12.))
                            .child("No drives selected. You can continue, but nothing will be indexed until you enable a drive in Settings."),
                    )
                }),
            _ => div()
                .flex()
                .flex_col()
                .gap_3()
                .child(div().text_size(px(22.)).font_weight(FontWeight::BOLD).child("Ready to index"))
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(colors.text_secondary)
                        .child("Telemetry is disabled by design. Click \"Start indexing\" to save these drive choices and begin the first scan.")
                ),
        };

        let primary_label = match self.step {
            0 => "Next",
            1 => {
                if selected_count == 0 {
                    "Skip (no drives selected)"
                } else {
                    "Start indexing"
                }
            }
            _ => "Next",
        };

        let can_go_back = self.step > 0;
        let disable_primary = false;

        div()
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(colors.bg)
            .text_color(colors.text_primary)
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .w(px(640.))
                    .bg(colors.panel_bg)
                    .border_1()
                    .border_color(colors.border)
                    .rounded_xl()
                    .shadow_lg()
                    .p_8()
                    .flex()
                    .flex_col()
                    .gap_5()
                    .child(self.render_progress(cx))
                    .child(body)
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .items_center()
                            .gap_3()
                            .child(div().when(can_go_back, |btn: Div| {
                                btn.px_4()
                                    .py_2()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(colors.border)
                                    .text_color(colors.text_primary)
                                    .cursor_pointer()
                                    .child("Back")
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|this, _, _, cx| {
                                            this.prev_step(cx);
                                        }),
                                    )
                            }))
                            .child(
                                div()
                                    .px_5()
                                    .py_2()
                                    .rounded_md()
                                    .bg(if disable_primary {
                                        colors.border
                                    } else {
                                        colors.match_highlight
                                    })
                                    .text_color(if disable_primary {
                                        colors.text_secondary
                                    } else {
                                        colors.bg
                                    })
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .opacity(if disable_primary { 0.7 } else { 1.0 })
                                    .cursor_pointer()
                                    .child(primary_label)
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|this, _, _, cx| {
                                            if this.step == 2
                                                && this.drives.iter().all(|d| !d.selected)
                                            {
                                                return;
                                            }
                                            this.next_step(cx);
                                        }),
                                    ),
                            ),
                    ),
            )
    }
}
