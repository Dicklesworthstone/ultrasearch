use crate::model::state::{BackendMode, SearchAppModel};
use gpui::prelude::*;
use gpui::{InteractiveElement, *};

fn search_bg() -> Hsla {
    hsla(0.0, 0.0, 0.102, 1.0)
}
fn search_border() -> Hsla {
    hsla(0.0, 0.0, 0.243, 1.0)
}
fn input_bg() -> Hsla {
    hsla(0.0, 0.0, 0.176, 1.0)
}
fn input_bg_focus() -> Hsla {
    hsla(0.0, 0.0, 0.208, 1.0)
}
fn input_border_focus() -> Hsla {
    hsla(207.0, 1.0, 0.404, 1.0)
}
fn text_primary() -> Hsla {
    hsla(0.0, 0.0, 0.894, 1.0)
}
fn text_secondary() -> Hsla {
    hsla(0.0, 0.0, 0.616, 1.0)
}
fn text_placeholder() -> Hsla {
    hsla(0.0, 0.0, 0.416, 1.0)
}
fn accent_blue() -> Hsla {
    hsla(207.0, 1.0, 0.416, 1.0)
}
fn status_success() -> Hsla {
    hsla(146.0, 0.444, 0.502, 1.0)
}
fn status_error() -> Hsla {
    hsla(0.0, 0.903, 0.661, 1.0)
}

pub struct SearchView {
    model: Entity<SearchAppModel>,
    focus_handle: FocusHandle,
    input_text: SharedString,
}

impl SearchView {
    pub fn new(model: Entity<SearchAppModel>, cx: &mut Context<SearchView>) -> Self {
        let focus_handle = cx.focus_handle();
        cx.observe(&model, |_, _, cx| cx.notify()).detach();

        Self {
            model,
            focus_handle,
            input_text: "".into(),
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    fn handle_input(&mut self, text: &str, cx: &mut Context<Self>) {
        self.input_text = SharedString::from(text.to_owned());
        self.model.update(cx, |model, cx| {
            model.set_query(text.to_string(), cx);
        });
        cx.notify();
    }

    fn clear_search(&mut self, cx: &mut Context<Self>) {
        self.input_text = "".into();
        self.model.update(cx, |model, cx| {
            model.set_query(String::new(), cx);
        });
        cx.notify();
    }

    fn set_mode(&mut self, mode: BackendMode, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| {
            model.set_backend_mode(mode, cx);
        });
    }

    fn format_number(n: u64) -> String {
        if n >= 1_000_000 {
            format!("{:.1}M", n as f64 / 1_000_000.0)
        } else if n >= 1_000 {
            format!("{:.1}K", n as f64 / 1_000.0)
        } else {
            n.to_string()
        }
    }

    fn render_mode_button(
        &self,
        label: &'static str,
        icon: &'static str,
        mode: BackendMode,
        current: BackendMode,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_active = mode == current;

        div()
            .flex()
            .items_center()
            .gap_1p5()
            .px_3()
            .py_1p5()
            .rounded_md()
            .when(is_active, |this| {
                this.bg(accent_blue()).text_color(white()).shadow_sm()
            })
            .when(!is_active, |this| {
                this.bg(input_bg())
                    .text_color(text_secondary())
                    .hover(|style| style.bg(input_bg_focus()).text_color(text_primary()))
            })
            .cursor_pointer()
            .child(div().text_size(px(14.)).child(icon))
            .child(
                div()
                    .text_size(px(13.))
                    .font_weight(FontWeight::MEDIUM)
                    .child(label),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| this.set_mode(mode, cx)),
            )
    }
}

impl Render for SearchView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let model = self.model.read(cx);
        let status = model.status.clone();
        let query = model.query.clone();
        let _ = model;
        let has_query = !query.is_empty();

        div()
            .flex()
            .flex_col()
            .w_full()
            .bg(search_bg())
            .border_b_1()
            .border_color(search_border())
            .shadow_sm()
            .child(
                // Search input area with modern styling
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_4()
                    .py_3()
                    .track_focus(&self.focus_handle)
                    .child(
                        // Search icon
                        div()
                            .text_size(px(20.))
                            .text_color(text_secondary())
                            .child("🔎"),
                    )
                    .child(
                        // Text input with focus ring
                        div()
                            .id("search-input")
                            .flex_1()
                            .px_3()
                            .py_2p5()
                            .bg(input_bg())
                            .border_1()
                            .border_color(search_border())
                            .rounded_lg()
                            .text_color(text_primary())
                            .text_size(px(15.))
                            .when(self.focus_handle.is_focused(window), |this| {
                                this.bg(input_bg_focus())
                                    .border_color(input_border_focus())
                                    .shadow_md()
                            })
                            .child(if self.input_text.is_empty() {
                                div()
                                    .text_color(text_placeholder())
                                    .child("Search files by name or content...")
                            } else {
                                div().child(self.input_text.to_string())
                            })
                            .cursor(CursorStyle::IBeam)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, window, _| {
                                    window.focus(&this.focus_handle);
                                }),
                            )
                            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                                if event.is_held {
                                    return;
                                }
                                let mods = &event.keystroke.modifiers;
                                let control = mods.control || mods.platform;
                                match event.keystroke.key.as_str() {
                                    "backspace" => {
                                        let mut current = this.input_text.to_string();
                                        current.pop();
                                        this.handle_input(&current, cx);
                                        cx.stop_propagation();
                                    }
                                    "delete" => {
                                        let mut current = this.input_text.to_string();
                                        current.pop();
                                        this.handle_input(&current, cx);
                                        cx.stop_propagation();
                                    }
                                    "enter" => {
                                        this.model.update(cx, |model, cx| {
                                            model.set_query(this.input_text.to_string(), cx);
                                        });
                                        cx.stop_propagation();
                                    }
                                    _ => {
                                        if !control && !mods.alt {
                                            if let Some(ch) = &event.keystroke.key_char {
                                                let mut current = this.input_text.to_string();
                                                current.push_str(ch);
                                                this.handle_input(&current, cx);
                                                cx.stop_propagation();
                                            }
                                        }
                                    }
                                };
                            })),
                    )
                    .when(has_query, |this| {
                        this.child(
                            // Clear button (only shown when there's text)
                            div()
                                .px_2()
                                .py_1p5()
                                .rounded_md()
                                .text_color(text_secondary())
                                .hover(|style| {
                                    style.bg(input_bg_focus()).text_color(text_primary())
                                })
                                .cursor_pointer()
                                .child("✕")
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _, _, cx| this.clear_search(cx)),
                                ),
                        )
                    })
                    .child(
                        // Mode selector buttons
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(self.render_mode_button(
                                "Name",
                                "🗂",
                                BackendMode::MetadataOnly,
                                status.backend_mode,
                                cx,
                            ))
                            .child(self.render_mode_button(
                                "Mixed",
                                "🌐",
                                BackendMode::Mixed,
                                status.backend_mode,
                                cx,
                            ))
                            .child(self.render_mode_button(
                                "Content",
                                "📝",
                                BackendMode::ContentOnly,
                                status.backend_mode,
                                cx,
                            )),
                    ),
            )
            .child(
                // Status bar with elegant information display
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_4()
                    .py_2()
                    .bg(hsla(0.0, 0.0, 0.141, 1.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_4()
                            .text_size(px(12.))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1p5()
                                    .child(div().text_color(text_secondary()).child(format!(
                                        "{} results",
                                        Self::format_number(status.total)
                                    )))
                                    .when(status.shown < status.total as usize, |this| {
                                        this.child(
                                            div()
                                                .text_color(text_placeholder())
                                                .child(format!("(showing {})", status.shown)),
                                        )
                                    }),
                            )
                            .when(status.last_latency_ms.is_some(), |this| {
                                this.child(div().text_color(search_border()).child("⏱"))
                                    .child(
                                        div().text_color(text_secondary()).child(format!(
                                            "{} ms",
                                            status.last_latency_ms.unwrap()
                                        )),
                                    )
                            })
                            .child(div().text_color(search_border()).child("•"))
                            .child(
                                div()
                                    .text_color(text_secondary())
                                    .child(status.indexing_state.clone()),
                            ),
                    )
                    .child(
                        // Connection status with animated indicator
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().w(px(8.)).h(px(8.)).rounded_full().bg(
                                if status.connected {
                                    status_success()
                                } else {
                                    status_error()
                                },
                            ))
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(if status.connected {
                                        status_success()
                                    } else {
                                        status_error()
                                    })
                                    .child(if status.connected {
                                        "Connected"
                                    } else {
                                        "Disconnected"
                                    }),
                            ),
                    ),
            )
    }
}
