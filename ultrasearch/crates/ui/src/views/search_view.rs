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
    cursor: usize,
    selection: Option<(usize, usize)>, // (start, end)
}

impl SearchView {
    pub fn new(model: Entity<SearchAppModel>, cx: &mut Context<SearchView>) -> Self {
        let focus_handle = cx.focus_handle().tab_stop(true).tab_index(0);
        cx.observe(&model, |_, _, cx| cx.notify()).detach();

        Self {
            model,
            focus_handle,
            input_text: "".into(),
            cursor: 0,
            selection: None,
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    fn clear_selection(&mut self) {
        self.selection = None;
    }

    fn set_selection(&mut self, start: usize, end: usize) {
        if start == end {
            self.selection = None;
        } else {
            self.selection = Some((start.min(end), start.max(end)));
        }
    }

    fn replace_selection(&mut self, text: &str) {
        if let Some((s, e)) = self.selection.take() {
            let mut current = self.input_text.to_string();
            current.replace_range(s..e, text);
            self.cursor = s + text.len();
            self.input_text = current.into();
        } else {
            self.insert_at_cursor(text);
        }
    }

    fn insert_at_cursor(&mut self, text: &str) {
        let mut current = self.input_text.to_string();
        current.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.input_text = current.into();
    }

    fn delete_backward(&mut self) {
        if let Some((s, e)) = self.selection.take() {
            let mut current = self.input_text.to_string();
            current.replace_range(s..e, "");
            self.cursor = s;
            self.input_text = current.into();
            return;
        }
        if self.cursor == 0 {
            return;
        }
        let mut current = self.input_text.to_string();
        if let Some((idx, len)) = current[..self.cursor]
            .char_indices()
            .last()
            .map(|(i, c)| (i, c.len_utf8()))
        {
            current.drain(idx..idx + len);
            self.cursor = idx;
            self.input_text = current.into();
        }
    }

    fn delete_forward(&mut self) {
        if let Some((s, e)) = self.selection.take() {
            let mut current = self.input_text.to_string();
            current.replace_range(s..e, "");
            self.cursor = s;
            self.input_text = current.into();
            return;
        }
        let current = self.input_text.to_string();
        if self.cursor >= current.len() {
            return;
        }
        if let Some((off, ch)) = current[self.cursor..].char_indices().next() {
            let mut new = current;
            let start = self.cursor + off;
            new.drain(start..start + ch.len_utf8());
            self.input_text = new.into();
        }
    }

    fn move_cursor_left(&mut self, selecting: bool) {
        if self.cursor == 0 {
            return;
        }
        let current = self.input_text.to_string();
        if let Some((idx, _)) = current[..self.cursor].char_indices().last() {
            if selecting {
                let anchor = self.selection.map(|(s, _)| s).unwrap_or(self.cursor);
                self.cursor = idx;
                self.set_selection(anchor, self.cursor);
            } else {
                self.cursor = idx;
                self.clear_selection();
            }
        }
    }

    fn move_cursor_right(&mut self, selecting: bool) {
        let current = self.input_text.to_string();
        if self.cursor >= current.len() {
            return;
        }
        if let Some((off, ch)) = current[self.cursor..].char_indices().next() {
            let new_pos = self.cursor + off + ch.len_utf8();
            if selecting {
                let anchor = self.selection.map(|(s, _)| s).unwrap_or(self.cursor);
                self.cursor = new_pos;
                self.set_selection(anchor, self.cursor);
            } else {
                self.cursor = new_pos;
                self.clear_selection();
            }
        }
    }

    fn move_cursor_home(&mut self, selecting: bool) {
        if selecting {
            let anchor = self.selection.map(|(s, _)| s).unwrap_or(self.cursor);
            self.cursor = 0;
            self.set_selection(anchor, self.cursor);
        } else {
            self.cursor = 0;
            self.clear_selection();
        }
    }

    fn move_cursor_end(&mut self, selecting: bool) {
        let end = self.input_text.len();
        if selecting {
            let anchor = self.selection.map(|(s, _)| s).unwrap_or(self.cursor);
            self.cursor = end;
            self.set_selection(anchor, self.cursor);
        } else {
            self.cursor = end;
            self.clear_selection();
        }
    }

    fn copy_selection(&mut self, cx: &mut Context<Self>) {
        if let Some((s, e)) = self.selection {
            let current = self.input_text.to_string();
            let clip = current[s..e].to_string();
            cx.write_to_clipboard(ClipboardItem::new_string(clip));
        }
    }

    fn cut_selection(&mut self, cx: &mut Context<Self>) {
        if let Some((s, e)) = self.selection {
            let mut current = self.input_text.to_string();
            cx.write_to_clipboard(ClipboardItem::new_string(current[s..e].to_string()));
            current.replace_range(s..e, "");
            self.input_text = current.into();
            self.cursor = s;
            self.clear_selection();
            cx.notify();
        }
    }

    fn paste_clipboard(&mut self, cx: &mut Context<Self>) {
        if let Some(item) = cx.read_from_clipboard() {
            if let Some(s) = item.entries().iter().find_map(|entry| match entry {
                ClipboardEntry::String(s) => Some(s.clone()),
                _ => None,
            }) {
                self.replace_selection(s.text().as_str());
                cx.notify();
            }
        }
    }

    fn handle_input(&mut self, text: &str, cx: &mut Context<Self>) {
        self.input_text = SharedString::from(text.to_owned());
        self.model.update(cx, |model, cx| {
            model.set_query(text.to_string(), cx);
        });
        cx.notify();
    }

    pub fn clear_search(&mut self, cx: &mut Context<Self>) {
        self.input_text = "".into();
        self.cursor = 0;
        self.selection = None;
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
            .tab_stop(true)
            .tab_index(0)
            .when(is_active, |this| {
                this.bg(accent_blue()).text_color(white()).shadow_sm()
            })
            .when(!is_active, |this| {
                this.bg(input_bg())
                    .text_color(text_secondary())
                    .hover(|style| style.bg(input_bg_focus()).text_color(text_primary()))
            })
            .focus_visible(|style| style.border_1().border_color(input_border_focus()))
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let model = self.model.read(cx);
        let status = model.status.clone();
        let query = model.query.clone();
        // Keep local text in sync if model was changed externally.
        if query != self.input_text {
            self.input_text = query.clone().into();
            let len = self.input_text.len();
            if self.cursor > len {
                self.cursor = len;
            }
            self.selection = None;
        }
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
                            .tab_index(0)
                            .px_3()
                            .py_2p5()
                            .bg(input_bg())
                            .border_1()
                            .border_color(search_border())
                            .rounded_lg()
                            .text_color(text_primary())
                            .text_size(px(15.))
                            .focus(|style| {
                                style
                                    .border_color(input_border_focus())
                                    .bg(input_bg_focus())
                            })
                            .focus_visible(|style| {
                                style.border_color(input_border_focus()).shadow_md()
                            })
                            .cursor(CursorStyle::IBeam)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                                    window.focus(&this.focus_handle);
                                    // approximate: single click places caret at end; double-click selects all
                                    let len = this.input_text.len();
                                    this.cursor = len;
                                    if event.click_count >= 2 {
                                        this.set_selection(0, len);
                                    } else {
                                        this.clear_selection();
                                    }
                                    cx.notify();
                                }),
                            )
                            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                                let mods = &event.keystroke.modifiers;
                                let control = mods.control || mods.platform;
                                let shift = mods.shift;
                                match event.keystroke.key.as_str() {
                                    "backspace" => {
                                        this.delete_backward();
                                        let updated = this.input_text.to_string();
                                        this.handle_input(&updated, cx);
                                        cx.stop_propagation();
                                    }
                                    "delete" => {
                                        this.delete_forward();
                                        let updated = this.input_text.to_string();
                                        this.handle_input(&updated, cx);
                                        cx.stop_propagation();
                                    }
                                    "enter" => {
                                        this.model.update(cx, |model, cx| {
                                            model.set_query(this.input_text.to_string(), cx);
                                        });
                                        cx.stop_propagation();
                                    }
                                    "left" => {
                                        this.move_cursor_left(shift);
                                        cx.stop_propagation();
                                    }
                                    "right" => {
                                        this.move_cursor_right(shift);
                                        cx.stop_propagation();
                                    }
                                    "home" => {
                                        this.move_cursor_home(shift);
                                        cx.stop_propagation();
                                    }
                                    "end" => {
                                        this.move_cursor_end(shift);
                                        cx.stop_propagation();
                                    }
                                    "a" if control => {
                                        this.set_selection(0, this.input_text.len());
                                        this.cursor = this.input_text.len();
                                        cx.stop_propagation();
                                    }
                                    "c" if control => {
                                        this.copy_selection(cx);
                                        cx.stop_propagation();
                                    }
                                    "x" if control => {
                                        this.cut_selection(cx);
                                        cx.stop_propagation();
                                    }
                                    "v" if control => {
                                        this.paste_clipboard(cx);
                                        let updated = this.input_text.to_string();
                                        this.handle_input(&updated, cx);
                                        cx.stop_propagation();
                                    }
                                    _ => {
                                        if !control && !mods.alt {
                                            if let Some(ch) = &event.keystroke.key_char {
                                                this.replace_selection(ch);
                                                let updated = this.input_text.to_string();
                                                this.handle_input(&updated, cx);
                                                cx.stop_propagation();
                                            }
                                        }
                                    }
                                };
                            }))
                            .child({
                                if self.input_text.is_empty() {
                                    div()
                                        .text_color(text_placeholder())
                                        .child("Search files by name or content...")
                                } else {
                                    let text = self.input_text.to_string();
                                    let (sel_start, sel_end) =
                                        self.selection.unwrap_or((self.cursor, self.cursor));
                                    let head = SharedString::from(text[..sel_start].to_string());
                                    let sel_str =
                                        SharedString::from(text[sel_start..sel_end].to_string());
                                    let tail = SharedString::from(text[sel_end..].to_string());
                                    let caret = div().w(px(1.5)).h(px(18.)).bg(text_primary());
                                    div()
                                        .flex()
                                        .items_center()
                                        .child(div().child(head.clone()))
                                        .when(sel_start != sel_end, |d| {
                                            d.child(
                                                div()
                                                    .bg(hsla(207.0, 1.0, 0.416, 0.2))
                                                    .text_color(text_primary())
                                                    .child(sel_str.clone()),
                                            )
                                        })
                                        .when(sel_start == sel_end, |d| d.child(caret))
                                        .child(div().child(tail))
                                }
                            }),
                    )
                    .when(has_query, |this| {
                        this.child(
                            // Clear button (only shown when there's text)
                            div()
                                .px_2()
                                .py_1p5()
                                .rounded_md()
                                .tab_stop(true)
                                .tab_index(0)
                                .text_color(text_secondary())
                                .hover(|style| {
                                    style.bg(input_bg_focus()).text_color(text_primary())
                                })
                                .focus_visible(|style| {
                                    style.border_1().border_color(input_border_focus())
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
                                    .text_color(text_secondary())
                                    .child(if status.in_flight { "⏳" } else { "" }),
                            )
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
                    )
                    .when(!status.connected, |this| {
                        this.child(
                            div()
                                .ml_3()
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .bg(status_error())
                                .text_color(white())
                                .text_size(px(11.))
                                .cursor_pointer()
                                .hover(|s| s.bg(hsla(0.0, 0.903, 0.7, 1.0)))
                                .focus_visible(|s| s.border_1().border_color(input_border_focus()))
                                .tab_stop(true)
                                .tab_index(0)
                                .child("Retry")
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _, _, cx| {
                                        this.model.update(cx, |model, cx| {
                                            let current = model.query.clone();
                                            if current.is_empty() {
                                                model.start_status_polling(cx);
                                            } else {
                                                model.set_query(current, cx);
                                            }
                                        });
                                    }),
                                )
                                .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                                    match event.keystroke.key.as_str() {
                                        "enter" | "space" => {
                                            this.model.update(cx, |model, cx| {
                                                let current = model.query.clone();
                                                if current.is_empty() {
                                                    model.start_status_polling(cx);
                                                } else {
                                                    model.set_query(current, cx);
                                                }
                                            });
                                            cx.stop_propagation();
                                        }
                                        _ => {}
                                    }
                                })),
                        )
                    }),
            )
    }
}
