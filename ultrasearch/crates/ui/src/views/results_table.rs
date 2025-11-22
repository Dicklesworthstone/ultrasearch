use crate::model::state::SearchAppModel;
use gpui::prelude::*;
use gpui::{InteractiveElement, *};
use ipc::SearchHit;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn row_height() -> Pixels {
    px(48.)
}
fn table_bg() -> Hsla {
    hsla(0.0, 0.0, 0.118, 1.0)
}
fn row_even() -> Hsla {
    hsla(0.0, 0.0, 0.118, 1.0)
}
fn row_odd() -> Hsla {
    hsla(0.0, 0.0, 0.141, 1.0)
}
fn row_selected() -> Hsla {
    hsla(210.0, 0.274, 0.243, 1.0)
}
fn row_hover() -> Hsla {
    hsla(0.0, 0.0, 0.165, 1.0)
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
fn border_color() -> Hsla {
    hsla(0.0, 0.0, 0.2, 1.0)
}

pub struct ResultsView {
    model: Entity<SearchAppModel>,
    list_state: ListState,
    hover_index: Option<usize>,
}

impl ResultsView {
    pub fn new(model: Entity<SearchAppModel>, cx: &mut Context<ResultsView>) -> Self {
        let list_state = ListState::new(0, ListAlignment::Top, row_height());

        cx.observe(&model, |this: &mut Self, model, cx| {
            let count = model.read(cx).results.len();
            this.list_state.reset(count);
            cx.notify();
        })
        .detach();

        Self {
            model,
            list_state,
            hover_index: None,
        }
    }

    fn handle_click(&mut self, index: usize, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| {
            model.selected_index = Some(index);
            cx.notify();
        });
    }

    #[allow(dead_code)]
    fn handle_double_click(&mut self, index: usize, cx: &mut Context<Self>) {
        let model = self.model.read(cx);
        if let Some(hit) = model.results.get(index) {
            if let Some(path) = &hit.path {
                self.open_file(path);
            }
        }
    }

    #[allow(dead_code)]
    fn open_file(&self, path: &str) {
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

        if bytes >= GB {
            format!("{:.2} GB", bytes as f64 / GB as f64)
        } else if bytes >= MB {
            format!("{:.1} MB", bytes as f64 / MB as f64)
        } else if bytes >= KB {
            format!("{:.1} KB", bytes as f64 / KB as f64)
        } else {
            format!("{bytes} B")
        }
    }

    fn format_modified_time(timestamp: i64) -> String {
        let now = SystemTime::now();
        let file_time = UNIX_EPOCH + Duration::from_secs(timestamp as u64);

        if let Ok(duration) = now.duration_since(file_time) {
            let days = duration.as_secs() / 86400;
            if days == 0 {
                "Today".to_string()
            } else if days == 1 {
                "Yesterday".to_string()
            } else if days < 7 {
                format!("{days} days ago")
            } else if days < 30 {
                format!("{} weeks ago", days / 7)
            } else if days < 365 {
                format!("{} months ago", days / 30)
            } else {
                format!("{} years ago", days / 365)
            }
        } else {
            "Future".to_string()
        }
    }

    fn get_file_icon(ext: Option<&String>) -> &'static str {
        match ext.map(|s| s.as_str()) {
            Some("rs") | Some("toml") | Some("js") | Some("ts") | Some("tsx") | Some("jsx")
            | Some("py") | Some("go") => "🧑‍💻",
            Some("pdf") => "📄",
            Some("docx") | Some("doc") => "📝",
            Some("xlsx") | Some("xls") => "📊",
            Some("pptx") | Some("ppt") => "📈",
            Some("zip") | Some("rar") | Some("7z") => "🗜",
            Some("jpg") | Some("jpeg") | Some("png") | Some("gif") | Some("svg") => "🖼",
            Some("mp4") | Some("avi") | Some("mkv") => "🎞",
            Some("mp3") | Some("wav") | Some("flac") => "🎵",
            Some("exe") | Some("dll") => "⚙",
            Some("md") | Some("txt") => "📄",
            _ => "📁",
        }
    }

    fn render_row(
        &self,
        index: usize,
        hit: &SearchHit,
        is_selected: bool,
        is_hover: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let is_even = index.is_multiple_of(2);

        let name = hit.name.clone().unwrap_or_else(|| "<unknown>".to_string());
        let path = hit.path.clone().unwrap_or_default();
        let size_text = hit
            .size
            .map(Self::format_file_size)
            .unwrap_or_else(|| "-".to_string());
        let modified_text = hit
            .modified
            .map(Self::format_modified_time)
            .unwrap_or_else(|| "-".to_string());
        let icon = Self::get_file_icon(hit.ext.as_ref());
        let score_pct = (hit.score * 100.0) as u32;

        div()
            .w_full()
            .h(row_height())
            .flex()
            .items_center()
            .px_4()
            .gap_3()
            .bg(if is_selected {
                row_selected()
            } else if is_hover {
                row_hover()
            } else if is_even {
                row_even()
            } else {
                row_odd()
            })
            .border_b_1()
            .border_color(border_color())
            .cursor_pointer()
            .on_mouse_move(cx.listener(move |this, _, _, cx| {
                this.hover_index = Some(index);
                cx.notify();
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    if event.click_count >= 2 {
                        this.handle_double_click(index, cx);
                    } else {
                        this.handle_click(index, cx);
                    }
                }),
            )
            // File icon
            .child(div().text_size(px(20.)).child(icon))
            // Name column (flexible)
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .overflow_hidden()
                    .child(
                        div()
                            .text_size(px(14.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(text_primary())
                            .overflow_hidden()
                            .child(name),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(text_secondary())
                            .overflow_hidden()
                            .child(path),
                    ),
            )
            // Score badge
            .when(score_pct > 0, |mut this: Div| {
                this = this.child(
                    div()
                        .px_2()
                        .py_0p5()
                        .rounded_md()
                        .bg(hsla(0.0, 0.0, 0.2, 1.0))
                        .text_size(px(10.))
                        .font_weight(FontWeight::BOLD)
                        .text_color(text_dim())
                        .child(format!("{score_pct}%")),
                );
                this
            })
            // Size column
            .child(
                div()
                    .w(px(80.))
                    .text_size(px(12.))
                    .text_color(text_secondary())
                    .child(size_text),
            )
            // Modified column
            .child(
                div()
                    .w(px(100.))
                    .text_size(px(12.))
                    .text_color(text_secondary())
                    .child(modified_text),
            )
            .into_any_element()
    }

    fn render_header(&self) -> impl IntoElement {
        div()
            .w_full()
            .h(px(40.))
            .flex()
            .items_center()
            .px_4()
            .gap_3()
            .bg(hsla(0.0, 0.0, 0.141, 1.0))
            .border_b_1()
            .border_color(border_color())
            .text_size(px(11.))
            .font_weight(FontWeight::BOLD)
            .text_color(text_dim())
            .child(div().w(px(20.))) // Icon space
            .child(div().flex_1().child("NAME"))
            .child(div().w(px(80.)).child("SIZE"))
            .child(div().w(px(100.)).child("MODIFIED"))
    }

    fn render_empty_state(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let model = self.model.read(cx);
        let has_query = !model.query.is_empty();

        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_4()
            .child(
                div()
                    .text_size(px(48.))
                    .child(if has_query { "🤔" } else { "🔍" }),
            )
            .child(
                div()
                    .text_size(px(16.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(text_secondary())
                    .child(if has_query {
                        "No results found"
                    } else {
                        "Start typing to search"
                    }),
            )
            .when(has_query, |this| {
                this.child(
                    div()
                        .text_size(px(13.))
                        .text_color(text_dim())
                        .child("Try different search terms or search mode"),
                )
            })
    }
}

impl Render for ResultsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let model = self.model.clone();
        let has_results = !model.read(cx).results.is_empty();
        let hover_index = self.hover_index;

        div()
            .size_full()
            .bg(table_bg())
            .flex()
            .flex_col()
            .when(has_results, |this| {
                this.child(self.render_header()).child(
                    list(
                        self.list_state.clone(),
                        cx.processor(move |this, ix, _window, cx| {
                            let (hit, is_selected) = {
                                let model_read = model.read(cx);
                                if let Some(hit) = model_read.results.get(ix).cloned() {
                                    (hit, model_read.is_selected(ix))
                                } else {
                                    return div().into_any_element();
                                }
                            };

                            let is_hover = hover_index == Some(ix);
                            this.render_row(ix, &hit, is_selected, is_hover, cx)
                        }),
                    )
                    .size_full(),
                )
            })
            .when(!has_results, |this| this.child(self.render_empty_state(cx)))
    }
}
