use crate::actions::CloseShortcuts;
use crate::theme;
use gpui::prelude::FluentBuilder;
use gpui::*;
use chrono::{DateTime, Local};

/// Full-screen overlay that provides a rich help + shortcuts experience.
pub struct HelpPanel {
    focus_handle: FocusHandle,
    filter_focus: FocusHandle,
    filter: String,
    docs: Option<String>,
    docs_updated: Option<String>,
}

impl HelpPanel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let docs_path = "docs/FEATURES.md";
        let docs = std::fs::read_to_string(docs_path).ok();
        let docs_updated = std::fs::metadata(docs_path)
            .and_then(|m| m.modified())
            .ok()
            .map(|ts| DateTime::<Local>::from(ts).format("%Y-%m-%d %H:%M").to_string());

        Self {
            focus_handle: cx.focus_handle(),
            filter_focus: cx.focus_handle(),
            filter: String::new(),
            docs,
            docs_updated,
        }
    }

    fn key_badge(label: &'static str, cx: &mut Context<Self>) -> Div {
        let colors = theme::active_colors(cx);
        let text = label.to_string();
        div()
            .px_2()
            .py_0p5()
            .rounded_md()
            .bg(colors.panel_bg)
            .border_1()
            .border_color(colors.border)
            .text_color(colors.text_primary)
            .text_size(px(11.))
            .cursor_pointer()
            .child(label)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                    window.focus(&this.filter_focus);
                }),
            )
    }

    fn section(
        title: &'static str,
        items: &'static [(&'static str, &'static str)],
        filter: &str,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let colors = theme::active_colors(cx);
        let needle = filter.trim().to_lowercase();
        let filtered: Vec<_> = items
            .iter()
            .filter(|(k, v)| {
                if needle.is_empty() {
                    true
                } else {
                    k.to_lowercase().contains(&needle) || v.to_lowercase().contains(&needle)
                }
            })
            .collect();
        if filtered.is_empty() {
            return None;
        }

        Some(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_size(px(13.))
                        .font_weight(FontWeight::BOLD)
                        .text_color(colors.text_primary)
                        .child(title),
                )
                .children(filtered.into_iter().map(|(k, v)| {
                    div()
                        .flex()
                        .justify_between()
                        .items_center()
                        .gap_2()
                        .text_size(px(12.))
                        .text_color(colors.text_secondary)
                        .child(div().child(*v))
                        .child(Self::key_badge(k, cx))
                })),
        )
    }
}

impl Render for HelpPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = theme::active_colors(cx);

        let callout = |title: &'static str, body: &'static str| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .p_3()
                .rounded_lg()
                .bg(colors.panel_bg)
                .border_1()
                .border_color(colors.border)
                .child(
                    div()
                        .text_size(px(13.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(colors.text_primary)
                        .child(title),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(colors.text_secondary)
                        .child(body),
                )
        };

        let sections: Vec<Div> = [
            Self::section(
                "Navigation",
                &[
                    ("Ctrl/Cmd+K", "Focus search"),
                    ("Up / Down", "Move selection"),
                    ("Enter", "Open selected"),
                    ("Ctrl+1/2/3", "Switch modes"),
                ],
                &self.filter,
                cx,
            ),
            Self::section(
                "Actions",
                &[
                    ("Ctrl/Cmd+C", "Copy path"),
                    ("Ctrl+Shift+C", "Copy file"),
                    ("Ctrl+Shift+O", "Open folder"),
                    ("Alt+Enter", "Properties"),
                ],
                &self.filter,
                cx,
            ),
            Self::section(
                "System",
                &[
                    ("F1 / Ctrl+/", "Toggle help"),
                    ("Alt+Space", "Quick Search palette"),
                    ("Ctrl/Cmd+Q", "Quit"),
                ],
                &self.filter,
                cx,
            ),
        ]
        .into_iter()
        .flatten()
        .collect();

        let no_results = sections.is_empty() && !self.filter.trim().is_empty();

        let filter_input = div()
            .track_focus(&self.filter_focus)
            .px_3()
            .py_2()
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            .bg(colors.bg)
            .text_color(colors.text_primary)
            .text_size(px(13.))
            .min_w(px(260.))
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .text_color(colors.text_secondary)
                    .child("Search shortcuts"),
            )
            .child(div().flex_1().child(self.filter.clone()))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, _| {
                    window.focus(&this.filter_focus);
                }),
            )
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                window.focus(&this.filter_focus);
                let mods = &event.keystroke.modifiers;
                let control = mods.control || mods.platform;
                match event.keystroke.key.as_str() {
                    "backspace" => {
                        this.filter.pop();
                        cx.notify();
                    }
                    "escape" => {
                        this.filter.clear();
                        cx.notify();
                    }
                    _ => {
                        if !control && !mods.alt {
                            if let Some(ch) = &event.keystroke.key_char {
                                this.filter.push_str(&ch.to_string());
                                cx.notify();
                            }
                        }
                    }
                }
                cx.stop_propagation();
            }));

        div()
            .absolute()
            .top_0()
            .left_0()
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(hsla(0.0, 0.0, 0.0, 0.45))
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down_out(cx.listener(|_, _, window, cx| {
                window.dispatch_action(Box::new(CloseShortcuts), cx);
            }))
            .on_action(cx.listener(|_, _: &CloseShortcuts, window, cx| {
                window.dispatch_action(Box::new(CloseShortcuts), cx);
            }))
            .child(
                div()
                    .w(px(840.))
                    .max_h(px(620.))
                    .bg(colors.panel_bg)
                    .rounded_xl()
                    .shadow_2xl()
                    .border_1()
                    .border_color(colors.border)
                    .p_6()
                    .flex()
                    .flex_col()
                    .gap_5()
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .items_center()
                            .child(
                                div()
                                    .text_size(px(18.))
                                    .font_weight(FontWeight::BOLD)
                                    .child("Help & Shortcuts"),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .items_center()
                                    .child(filter_input)
                                    .child(
                                        div()
                                            .text_size(px(11.))
                                            .text_color(colors.text_secondary)
                                            .child("Click a key to copy"),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_6()
                            .children(sections)
                            .child(div().when(no_results, |d: Div| {
                                d.text_color(colors.text_secondary)
                                    .text_size(px(12.))
                                    .child("No shortcuts match your filter.")
                            })),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_4()
                            .child(callout(
                                "Quick Search (Alt+Space)",
                                "Floating palette with history, fuzzy highlights, and keyboard-only navigation. If PowerToys Run owns Alt+Space, rebind there or pick another hotkey.",
                            ))
                            .child(callout(
                                "Tray & Updates",
                                "Tray tooltip shows: Idle, Indexing, Update available, Offline. Updates flow: Check -> Download -> Restart to Update. Opt-in is required before checking.",
                            ))
                            .child(callout(
                                "Docs & Setup",
                                "See docs/FEATURES.md for UI highlights and docs/GRAALVM_SETUP.md for Extractous prerequisites (GraalVM 23.x + checksum).",
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .font_weight(FontWeight::BOLD)
                                    .child("Features (from docs/FEATURES.md)"),
                            )
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(colors.text_secondary)
                                    .child(
                                        self.docs_updated
                                            .clone()
                                            .unwrap_or_else(|| "Updated on: unknown".into()),
                                    ),
                            )
                            .child(
                                div()
                                    .max_h(px(220.))
                                    .overflow_y_hidden()
                                    .p_3()
                                    .rounded_md()
                                    .bg(colors.bg)
                                    .border_1()
                                    .border_color(colors.border)
                                    .text_size(px(12.))
                                    .text_color(colors.text_primary)
                                    .whitespace_nowrap()
                                    .child(
                                        self.docs
                                            .clone()
                                            .unwrap_or_else(|| "docs/FEATURES.md not found.".into()),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_3()
                            .child(callout(
                                "Privacy",
                                "Index data and telemetry stay local unless you explicitly opt in. You can revisit onboarding via Settings > Onboarding.",
                            ))
                            .child(callout(
                                "Support shortcuts",
                                "Need a refresher? Press F1 or Ctrl+/ from anywhere, or use the header Help chip.",
                            ))
                            .child(callout(
                                "Alt+Space conflict",
                                "If Alt+Space won’t register, PowerToys Run or another launcher may own it. Rebind there or configure an alternate quick-search hotkey.",
                            )),
                    ),
            )
    }
}
