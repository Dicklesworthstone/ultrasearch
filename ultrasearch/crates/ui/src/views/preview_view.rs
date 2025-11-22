use crate::model::state::SearchAppModel;
use gpui::prelude::*;
use gpui::{InteractiveElement, UniformListScrollHandle, *};
use std::process::Command;

fn preview_bg() -> Hsla {
    hsla(0.0, 0.0, 0.102, 1.0)
}
fn preview_border() -> Hsla {
    hsla(0.0, 0.0, 0.2, 1.0)
}
fn text_primary() -> Hsla {
    hsla(0.0, 0.0, 0.894, 1.0)
}
fn text_secondary() -> Hsla {
    hsla(0.0, 0.0, 0.616, 1.0)
}
fn text_dim() -> Hsla {
    hsla(0.0, 0.0, 0.416, 1.0)
}
fn accent_blue() -> Hsla {
    hsla(207.0, 1.0, 0.416, 1.0)
}
fn snippet_bg() -> Hsla {
    hsla(0.0, 0.0, 0.157, 1.0)
}
fn snippet_border() -> Hsla {
    hsla(0.0, 0.0, 0.243, 1.0)
}

pub struct PreviewView {
    model: Entity<SearchAppModel>,
    snippet_scroll: UniformListScrollHandle,
    last_item_id: Option<String>,
}

impl PreviewView {
    pub fn new(model: Entity<SearchAppModel>, cx: &mut Context<PreviewView>) -> Self {
        cx.observe(&model, |this: &mut PreviewView, model, cx| {
            let selected_path = model
                .read(cx)
                .selected_row()
                .and_then(|hit| hit.path.clone())
                .unwrap_or_default();
            if this
                .last_item_id
                .as_ref()
                .map(|p| p != &selected_path)
                .unwrap_or(true)
            {
                this.snippet_scroll = UniformListScrollHandle::new();
                this.last_item_id = Some(selected_path);
                cx.notify();
            }
        })
        .detach();
        Self {
            model,
            snippet_scroll: UniformListScrollHandle::new(),
            last_item_id: None,
        }
    }

    fn open_in_explorer(&mut self, path: &str) {
        #[cfg(target_os = "windows")]
        {
            Command::new("explorer")
                .arg("/select,")
                .arg(path)
                .spawn()
                .ok();
        }
        #[cfg(target_os = "macos")]
        {
            Command::new("open").arg("-R").arg(path).spawn().ok();
        }
        #[cfg(target_os = "linux")]
        {
            if let Some(parent) = std::path::Path::new(path).parent() {
                Command::new("xdg-open").arg(parent).spawn().ok();
            }
        }
    }

    fn open_file(&mut self, path: &str) {
        #[cfg(target_os = "windows")]
        {
            Command::new("cmd")
                .args(["/C", "start", "", path])
                .spawn()
                .ok();
        }
        #[cfg(target_os = "macos")]
        {
            Command::new("open").arg(path).spawn().ok();
        }
        #[cfg(target_os = "linux")]
        {
            Command::new("xdg-open").arg(path).spawn().ok();
        }
    }

    fn format_file_size(bytes: u64) -> String {
        const KB: u64 = 1024;
        const MB: u64 = KB * 1024;
        const GB: u64 = MB * 1024;
        const TB: u64 = GB * 1024;

        if bytes >= TB {
            format!("{:.2} TB", bytes as f64 / TB as f64)
        } else if bytes >= GB {
            format!("{:.2} GB", bytes as f64 / GB as f64)
        } else if bytes >= MB {
            format!("{:.1} MB", bytes as f64 / MB as f64)
        } else if bytes >= KB {
            format!("{:.1} KB", bytes as f64 / KB as f64)
        } else {
            format!("{bytes} bytes")
        }
    }

    fn format_modified_time(timestamp: i64) -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let datetime = UNIX_EPOCH + std::time::Duration::from_secs(timestamp as u64);

        if let Ok(duration) = SystemTime::now().duration_since(datetime) {
            let days = duration.as_secs() / 86400;
            if days == 0 {
                "Today".to_string()
            } else if days == 1 {
                "Yesterday".to_string()
            } else {
                format!("{days} days ago")
            }
        } else {
            "In the future".to_string()
        }
    }

    fn render_action_button(
        &self,
        icon: &'static str,
        label: &'static str,
        enabled: bool,
        on_click: impl Fn(&mut Self, &MouseDownEvent, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let base = div()
            .flex()
            .items_center()
            .gap_2()
            .px_4()
            .py_2p5()
            .bg(accent_blue())
            .rounded_lg()
            .text_color(white())
            .font_weight(FontWeight::MEDIUM)
            .text_size(px(13.))
            .shadow_md()
            .child(div().text_size(px(16.)).child(icon))
            .child(label);

        if enabled {
            base.cursor_pointer()
                .hover(|style| style.bg(hsla(207.0, 0.897, 0.556, 1.0)))
                .on_mouse_down(MouseButton::Left, cx.listener(on_click))
        } else {
            base.opacity(0.5).cursor(CursorStyle::Arrow)
        }
    }

    fn render_info_row(&self, label: &str, value: String, icon: &'static str) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap_3()
            .px_4()
            .py_3()
            .rounded_lg()
            .bg(hsla(0.0, 0.0, 0.141, 1.0))
            .child(div().text_size(px(18.)).child(icon))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(text_dim())
                            .child(label.to_uppercase()),
                    )
                    .child(
                        div()
                            .text_size(px(14.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(text_primary())
                            .child(value),
                    ),
            )
    }
}

impl Render for PreviewView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.model.read(cx).selected_row().cloned();

        let content = if let Some(hit) = selected {
            let name = hit.name.as_deref().unwrap_or("<unknown>").to_string();
            let path = hit.path.clone().unwrap_or_default();
            let has_path = !path.is_empty();
            let size_text = hit
                .size
                .map(Self::format_file_size)
                .unwrap_or_else(|| "Unknown".to_string());
            let modified_text = hit
                .modified
                .map(Self::format_modified_time)
                .unwrap_or_else(|| "Unknown".to_string());
            let ext = hit.ext.clone().unwrap_or_else(|| "None".to_string());
            let score = format!("{:.1}%", hit.score * 100.0);

            let mut content = div()
                .flex()
                .flex_col()
                .size_full()
                .child(
                    // Header section with file name and actions
                    div()
                        .flex()
                        .flex_col()
                        .gap_4()
                        .p_6()
                        .border_b_1()
                        .border_color(preview_border())
                        .child(
                            div()
                                .text_size(px(20.))
                                .font_weight(FontWeight::BOLD)
                                .text_color(text_primary())
                                .child(name.clone()),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(text_secondary())
                                .child(path.clone()),
                        )
                        .child(
                            // Action buttons
                            div()
                                .flex()
                                .gap_3()
                                .mt_2()
                                .child(self.render_action_button(
                                    "📂",
                                    "Open",
                                    has_path,
                                    {
                                        let path = path.clone();
                                        move |this, _, _, _| this.open_file(&path)
                                    },
                                    cx,
                                ))
                                .child(self.render_action_button(
                                    "🗂",
                                    "Show in Folder",
                                    has_path,
                                    {
                                        let path = path.clone();
                                        move |this, _, _, _| this.open_in_explorer(&path)
                                    },
                                    cx,
                                )),
                        ),
                )
                .child(
                    // Details section
                    div()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .p_6()
                        .child(
                            div()
                                .text_size(px(13.))
                                .font_weight(FontWeight::BOLD)
                                .text_color(text_dim())
                                .mb_3()
                                .child("FILE DETAILS"),
                        )
                        .child(self.render_info_row("Size", size_text, "📐"))
                        .child(self.render_info_row("Modified", modified_text, "⏱"))
                        .child(self.render_info_row("Extension", ext.to_uppercase(), "📎"))
                        .child(self.render_info_row("Match Score", score, "⭐")),
                );

            if let Some(snippet) = hit.snippet.clone() {
                content = content.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .p_6()
                        .border_t_1()
                        .border_color(preview_border())
                        .child(
                            div()
                                .text_size(px(13.))
                                .font_weight(FontWeight::BOLD)
                                .text_color(text_dim())
                                .mb_2()
                                .child("CONTENT PREVIEW"),
                        )
                        .child(
                            div()
                                .p_4()
                                .bg(snippet_bg())
                                .border_1()
                                .border_color(snippet_border())
                                .rounded_lg()
                                .max_h(px(260.))
                                .child({
                                    let mut lines: Vec<String> =
                                        snippet.to_string().lines().map(|l| l.to_string()).collect();
                                    if lines.is_empty() {
                                        lines.push(String::new());
                                    }
                                    let count = lines.len();
                                    let handle = self.snippet_scroll.clone();
                                    uniform_list("preview-snippet", count, move |range, _, _| {
                                        lines[range]
                                            .iter()
                                            .map(|line| {
                                                div()
                                                    .px_1()
                                                    .py_0p5()
                                                    .text_size(px(12.))
                                                    .text_color(text_secondary())
                                                    .whitespace_nowrap()
                                                    .text_ellipsis()
                                                    .child(line.clone())
                                            })
                                            .collect()
                                    })
                                    .track_scroll(handle)
                                }),
                        ),
                );
            }

            content
        } else {
            // Empty state
            div()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .size_full()
                .gap_4()
                .child(div().text_size(px(64.)).child("🪄"))
                .child(
                    div()
                        .text_size(px(16.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(text_secondary())
                        .child("No file selected"),
                )
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(text_dim())
                        .child("Select a file from the results to see details and preview"),
                )
        };

        div()
            .size_full()
            .bg(preview_bg())
            .border_l_1()
            .border_color(preview_border())
            .child(content)
    }
}
