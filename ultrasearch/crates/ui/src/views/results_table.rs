use crate::globals::GlobalAppState;
use crate::icon_cache::IconCache;
use crate::model::state::SearchAppModel;
use crate::theme;
use crate::views::context_menu::{
    open_path_in_os, ContextMenu, ContextMenuCommand, ContextMenuItem,
};
use gpui::prelude::*;
use gpui::{InteractiveElement, *};
use ipc::SearchHit;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn row_height() -> Pixels {
    px(48.)
}

pub struct ResultsView {
    model: Entity<SearchAppModel>,
    icon_cache: Entity<IconCache>,
    list_state: ListState,
    hover_index: Option<usize>,
}

impl ResultsView {
    pub fn new(model: Entity<SearchAppModel>, cx: &mut Context<ResultsView>) -> Self {
        let list_state = ListState::new(0, ListAlignment::Top, row_height());
        let icon_cache = cx.global::<GlobalAppState>().icon_cache.clone();

        cx.observe(&model, |this: &mut Self, model, cx| {
            let read = model.read(cx);
            let count = read.current_page_results().len();
            this.list_state.reset(count);
            cx.notify();
        })
        .detach();

        cx.observe(&icon_cache, |_, _, cx| {
            cx.notify();
        })
        .detach();

        Self {
            model,
            icon_cache,
            list_state,
            hover_index: None,
        }
    }

    fn highlight_text(
        &self,
        text: &str,
        query: &str,
        is_primary: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = theme::active_colors(cx);
        let needle = query.trim();
        if needle.is_empty() {
            return div()
                .text_size(if is_primary { px(14.) } else { px(11.) })
                .font_weight(if is_primary {
                    FontWeight::MEDIUM
                } else {
                    FontWeight::NORMAL
                })
                .text_color(if is_primary {
                    colors.text_primary
                } else {
                    colors.text_secondary
                })
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .child(text.to_string())
                .into_any_element();
        }

        let lower = text.to_ascii_lowercase();
        let needle_lower = needle.to_ascii_lowercase();
        if let Some(pos) = lower.find(&needle_lower) {
            let end = pos + needle_lower.len();
            if let (Some(pre), Some(matched), Some(post)) =
                (text.get(..pos), text.get(pos..end), text.get(end..))
            {
                return div()
                    .text_size(if is_primary { px(14.) } else { px(11.) })
                    .font_weight(if is_primary {
                        FontWeight::MEDIUM
                    } else {
                        FontWeight::NORMAL
                    })
                    .text_color(if is_primary {
                        colors.text_primary
                    } else {
                        colors.text_secondary
                    })
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(pre.to_string())
                    .child(
                        div()
                            .px_1()
                            .rounded_sm()
                            .bg(colors.match_highlight)
                            .text_color(colors.bg)
                            .child(matched.to_string()),
                    )
                    .child(post.to_string())
                    .into_any_element();
            }
        }
        div()
            .text_size(if is_primary { px(14.) } else { px(11.) })
            .font_weight(if is_primary {
                FontWeight::MEDIUM
            } else {
                FontWeight::NORMAL
            })
            .text_color(if is_primary {
                colors.text_primary
            } else {
                colors.text_secondary
            })
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .child(text.to_string())
            .into_any_element()
    }

    fn render_highlighted(
        &self,
        text: &str,
        query: &str,
        is_primary: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.highlight_text(text, query, is_primary, cx)
    }

    fn handle_click(&mut self, global_index: usize, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| {
            if model.results.is_empty() {
                model.selected_index = None;
            } else {
                model.selected_index =
                    Some(global_index.min(model.results.len().saturating_sub(1)));
                model.ensure_page_for_selection();
            }
            cx.notify();
        });
    }

    fn handle_context_menu(
        &mut self,
        index: usize,
        event: &MouseDownEvent,
        cx: &mut Context<Self>,
    ) {
        // 1. Select the row
        self.handle_click(index, cx);

        // 2. Create items
        let items = vec![
            ContextMenuItem {
                label: "Open".into(),
                icon: Some("📂"),
                command: ContextMenuCommand::Open,
            },
            ContextMenuItem {
                label: "Open Containing Folder".into(),
                icon: Some("🗂"),
                command: ContextMenuCommand::OpenContainingFolder,
            },
            ContextMenuItem {
                label: "Copy Full Path".into(),
                icon: Some("📋"),
                command: ContextMenuCommand::CopyFullPath,
            },
            ContextMenuItem {
                label: "Copy File".into(),
                icon: Some("📄"),
                command: ContextMenuCommand::CopyFile,
            },
            ContextMenuItem {
                label: "Properties".into(),
                icon: Some("⚙"),
                command: ContextMenuCommand::Properties,
            },
        ];

        let origin = Point::new(px(0.), px(0.));
        let position = event.position;
        let screen_pos = origin + position;
        let model = self.model.clone();

        let _handle = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: screen_pos,
                        size: Size {
                            width: px(200.),
                            height: px(150.),
                        },
                    })),
                    titlebar: None,
                    window_background: WindowBackgroundAppearance::Transparent,
                    kind: WindowKind::PopUp,
                    ..WindowOptions::default()
                },
                move |_, cx| cx.new(|cx| ContextMenu::new(Point::default(), items, model, cx)),
            )
            .ok();
    }

    fn handle_double_click(&mut self, global_index: usize, cx: &mut Context<Self>) {
        let hit_info = self
            .model
            .read(cx)
            .results
            .get(global_index)
            .map(|h| (h.key, h.path.clone()));
        if let Some((key, path_opt)) = hit_info {
            self.model.update(cx, |m, _| m.record_result_open(key));
            if let Some(path) = path_opt {
                self.open_file(&path);
            }
        }
    }

    fn open_file(&self, path: &str) {
        open_path_in_os(path);
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
        let Ok(secs) = u64::try_from(timestamp) else {
            return "-".to_string();
        };
        let now = SystemTime::now();
        let file_time = UNIX_EPOCH + Duration::from_secs(secs);

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

    fn get_file_icon_char(ext: Option<&String>) -> &'static str {
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
        query: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let is_even = index.is_multiple_of(2);
        let colors = theme::active_colors(cx);

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

        // Icon logic: Try native, fallback to emoji
        let ext_str = hit.ext.as_deref().unwrap_or("");
        let icon_img = self
            .icon_cache
            .update(cx, |cache: &mut IconCache, cx| cache.get(ext_str, cx));

        let icon_el = if let Some(src) = icon_img {
            img(src).w(px(20.)).h(px(20.)).into_any_element()
        } else {
            let char = Self::get_file_icon_char(hit.ext.as_ref());
            div().text_size(px(20.)).child(char).into_any_element()
        };

        let score_pct = (hit.score * 100.0) as u32;
        let row_bg = if is_selected {
            colors.selection_bg
        } else if is_hover {
            colors.panel_bg
        } else if is_even {
            colors.bg
        } else {
            colors.panel_bg
        };

        div()
            .w_full()
            .h(row_height())
            .flex()
            .items_center()
            .px_4()
            .gap_3()
            .bg(row_bg)
            .border_b_1()
            .border_color(colors.divider)
            .cursor_pointer()
            .tab_stop(true)
            .tab_index(0)
            .focus_visible(|style| style.border_color(colors.match_highlight).border_2())
            .on_mouse_move(cx.listener(move |this, _, _, cx| {
                if this.hover_index != Some(index) {
                    this.hover_index = Some(index);
                    cx.notify();
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.handle_click(index, cx);
                    if event.click_count >= 2 {
                        this.handle_double_click(index, cx);
                    }
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.handle_context_menu(index, event, cx);
                }),
            )
            .child(icon_el)
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .overflow_hidden()
                    .child(self.render_highlighted(&name, query, true, cx))
                    .child(self.render_highlighted(&path, query, false, cx)),
            )
            // Score badge
            .when(score_pct > 0, |mut this: Div| {
                this = this.child(
                    div()
                        .px_2()
                        .py_0p5()
                        .rounded_md()
                        .bg(colors.panel_bg)
                        .text_size(px(10.))
                        .font_weight(FontWeight::BOLD)
                        .text_color(colors.text_secondary)
                        .child(format!("{score_pct}%")),
                );
                this
            })
            // Size column
            .child(
                div()
                    .w(px(80.))
                    .text_size(px(12.))
                    .text_color(colors.text_secondary)
                    .child(size_text),
            )
            // Modified column
            .child(
                div()
                    .w(px(100.))
                    .text_size(px(12.))
                    .text_color(colors.text_secondary)
                    .child(modified_text),
            )
            .into_any_element()
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = theme::active_colors(cx);
        div()
            .w_full()
            .h(px(40.))
            .flex()
            .items_center()
            .px_4()
            .gap_3()
            .bg(colors.panel_bg)
            .border_b_1()
            .border_color(colors.border)
            .text_size(px(11.))
            .font_weight(FontWeight::BOLD)
            .text_color(colors.text_secondary)
            .child(div().w(px(20.))) // Icon space
            .child(div().flex_1().child("NAME"))
            .child(div().w(px(80.)).child("SIZE"))
            .child(div().w(px(100.)).child("MODIFIED"))
    }

    fn render_pagination_footer(
        &self,
        page: usize,
        total_pages: usize,
        start: usize,
        shown: usize,
        total_loaded: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = theme::active_colors(cx);
        let can_prev = page > 0;
        let can_next = page + 1 < total_pages;
        let end = start + shown;

        div()
            .w_full()
            .h(px(36.))
            .flex()
            .items_center()
            .justify_between()
            .px_4()
            .bg(colors.panel_bg)
            .border_t_1()
            .border_color(colors.border)
            .text_size(px(11.))
            .text_color(colors.text_secondary)
            .child(format!(
                "Showing {}–{} of {} loaded results (Page {} of {})",
                start + 1,
                end,
                total_loaded,
                page + 1,
                total_pages
            ))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .border_1()
                            .border_color(colors.border)
                            .bg(colors.bg)
                            .text_color(if can_prev {
                                colors.text_primary
                            } else {
                                colors.text_secondary
                            })
                            .when(can_prev, |el| {
                                el.cursor_pointer().on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, _, cx| {
                                        this.model.update(cx, |m, cx| {
                                            m.set_page(page.saturating_sub(1), cx);
                                        });
                                    }),
                                )
                            })
                            .child("← Prev"),
                    )
                    .child(
                        div()
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .border_1()
                            .border_color(colors.border)
                            .bg(colors.bg)
                            .text_color(if can_next {
                                colors.text_primary
                            } else {
                                colors.text_secondary
                            })
                            .when(can_next, |el| {
                                el.cursor_pointer().on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, _, cx| {
                                        this.model.update(cx, |m, cx| {
                                            m.set_page(page + 1, cx);
                                        });
                                    }),
                                )
                            })
                            .child("Next →"),
                    ),
            )
    }

    fn render_skeleton_loading(&self, cx: &mut Context<Self>) -> Div {
        let colors = theme::active_colors(cx);
        let widths = [px(180.), px(240.), px(150.), px(210.), px(190.), px(160.)];
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(self.render_header(cx))
            .children(widths.into_iter().enumerate().map(|(i, w)| {
                let row_bg = if i.is_multiple_of(2) {
                    colors.bg
                } else {
                    colors.panel_bg
                };
                div()
                    .w_full()
                    .h(row_height())
                    .flex()
                    .items_center()
                    .px_4()
                    .gap_3()
                    .bg(row_bg)
                    .border_b_1()
                    .border_color(colors.divider)
                    .child(
                        div()
                            .w(px(20.))
                            .h(px(20.))
                            .rounded_sm()
                            .bg(colors.divider)
                            .opacity(0.65),
                    )
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap_1p5()
                            .child(
                                div()
                                    .w(w)
                                    .h(px(12.))
                                    .rounded_sm()
                                    .bg(colors.divider)
                                    .opacity(0.75),
                            )
                            .child(
                                div()
                                    .w(px(280.))
                                    .h(px(9.))
                                    .rounded_sm()
                                    .bg(colors.divider)
                                    .opacity(0.45),
                            ),
                    )
                    .child(
                        div()
                            .w(px(56.))
                            .h(px(10.))
                            .rounded_sm()
                            .bg(colors.divider)
                            .opacity(0.5),
                    )
                    .child(
                        div()
                            .w(px(72.))
                            .h(px(10.))
                            .rounded_sm()
                            .bg(colors.divider)
                            .opacity(0.5),
                    )
            }))
    }

    fn render_empty_state(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let model = self.model.read(cx);
        if model.status.in_flight {
            return self.render_skeleton_loading(cx);
        }
        let has_query = !model.query.is_empty();
        let (empty_title, empty_detail) = model.status.empty_search_message(has_query);
        let pinned = model.pinned_searches.clone();
        let recent: Vec<String> = model.history.iter().take(6).cloned().collect();
        let colors = theme::active_colors(cx);

        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_4()
            .p_6()
            .child(
                div()
                    .text_size(px(48.))
                    .child(if has_query { "🤔" } else { "🔍" }),
            )
            .child(
                div()
                    .text_size(px(16.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(colors.text_secondary)
                    .child(empty_title),
            )
            .when(!empty_detail.is_empty(), |this| {
                this.child(
                    div()
                        .text_size(px(13.))
                        .max_w(px(760.))
                        .text_color(colors.text_secondary)
                        .child(empty_detail),
                )
            })
            .when(!has_query && !pinned.is_empty(), |this| {
                this.child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap_2()
                        .mt_2()
                        .child(
                            div()
                                .text_size(px(12.))
                                .font_weight(FontWeight::BOLD)
                                .text_color(colors.text_secondary)
                                .child("📌 Smart Folders (Pinned Searches)"),
                        )
                        .child(div().flex().flex_wrap().justify_center().gap_2().children(
                            pinned.into_iter().map(|q| {
                                let q_click = q.clone();
                                div()
                                    .px_3()
                                    .py_1p5()
                                    .rounded_md()
                                    .bg(colors.panel_bg)
                                    .border_1()
                                    .border_color(colors.border)
                                    .text_size(px(12.))
                                    .text_color(colors.text_primary)
                                    .hover(|s| {
                                        s.bg(colors.selection_bg)
                                            .border_color(colors.match_highlight)
                                    })
                                    .cursor_pointer()
                                    .child(q)
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _, _, cx| {
                                            this.model.update(cx, |m, cx| {
                                                m.set_query(q_click.clone(), cx);
                                            });
                                        }),
                                    )
                            }),
                        )),
                )
            })
            .when(!has_query && !recent.is_empty(), |this| {
                this.child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap_2()
                        .mt_2()
                        .child(
                            div()
                                .text_size(px(12.))
                                .font_weight(FontWeight::BOLD)
                                .text_color(colors.text_secondary)
                                .child("🕘 Recent Searches"),
                        )
                        .child(div().flex().flex_wrap().justify_center().gap_2().children(
                            recent.into_iter().map(|q| {
                                let q_click = q.clone();
                                div()
                                    .px_2p5()
                                    .py_1()
                                    .rounded_md()
                                    .bg(colors.bg)
                                    .border_1()
                                    .border_color(colors.divider)
                                    .text_size(px(11.))
                                    .text_color(colors.text_secondary)
                                    .hover(|s| {
                                        s.bg(colors.panel_bg).text_color(colors.text_primary)
                                    })
                                    .cursor_pointer()
                                    .child(q)
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _, _, cx| {
                                            this.model.update(cx, |m, cx| {
                                                m.set_query(q_click.clone(), cx);
                                            });
                                        }),
                                    )
                            }),
                        )),
                )
            })
    }
}

impl Render for ResultsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let model = self.model.clone();
        let model_read = model.read(cx);
        let start = model_read.page_start();
        let page = model_read.page;
        let page_size = model_read.page_size.max(1);
        let total_loaded = model_read.results.len();
        let total_pages = total_loaded.div_ceil(page_size).max(1);
        let page_hits = model_read.current_page_results().to_vec();
        let shown = page_hits.len();
        let has_results = !page_hits.is_empty();
        let hover_index = self.hover_index;
        let colors = theme::active_colors(cx);
        let query = model_read.query.clone();

        div()
            .size_full()
            .bg(colors.bg)
            .flex()
            .flex_col()
            .on_mouse_move(cx.listener(|this, _, _, cx| {
                if this.hover_index.is_some() {
                    this.hover_index = None;
                    cx.notify();
                }
            }))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                if this.hover_index.is_some() {
                    this.hover_index = None;
                    cx.notify();
                }
            }))
            .when(has_results, |this: Div| {
                this.child(self.render_header(cx))
                    .child(
                        div().flex_1().w_full().overflow_hidden().child(
                            list(
                                self.list_state.clone(),
                                cx.processor(move |this, ix, _window, cx| {
                                    let global_index = start + ix;
                                    let (hit, is_selected) = match page_hits.get(ix).cloned() {
                                        Some(hit) => {
                                            let sel = model.read(cx).is_selected(global_index);
                                            (hit, sel)
                                        }
                                        None => return div().into_any_element(),
                                    };

                                    let is_hover = hover_index == Some(global_index);
                                    this.render_row(
                                        global_index,
                                        &hit,
                                        is_selected,
                                        is_hover,
                                        &query,
                                        cx,
                                    )
                                }),
                            )
                            .size_full(),
                        ),
                    )
                    .when(total_pages > 1, |this: Div| {
                        this.child(self.render_pagination_footer(
                            page,
                            total_pages,
                            start,
                            shown,
                            total_loaded,
                            cx,
                        ))
                    })
            })
            .when(!has_results, |this: Div| {
                this.child(self.render_empty_state(cx))
            })
    }
}
