use crate::actions::{ClearSearch, OpenSelected, SelectNext, SelectPrev};
use crate::model::state::SearchAppModel;
use crate::theme;
use crate::views::results_table::ResultsView;
use crate::views::search_view::SearchView;
use gpui::*;

pub struct QuickBarView {
    model: Entity<SearchAppModel>,
    search_view: Entity<SearchView>,
    results_view: Entity<ResultsView>,
    focus_handle: FocusHandle,
}

impl QuickBarView {
    pub fn new(model: Entity<SearchAppModel>, cx: &mut Context<Self>) -> Self {
        let search_view = cx.new(|cx| SearchView::new(model.clone(), cx));
        let results_view = cx.new(|cx| ResultsView::new(model.clone(), cx));

        // Use the search view's focus handle as the main handle for this view
        let focus_handle = search_view.read(cx).focus_handle();

        Self {
            model,
            search_view,
            results_view,
            focus_handle,
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    fn open_selected_and_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (query, selected) = {
            let m = self.model.read(cx);
            (
                m.query.clone(),
                m.selected_row().map(|h| (h.key, h.path.clone())),
            )
        };
        self.model.update(cx, |m, _| {
            m.push_history(&query);
            if let Some((key, _)) = selected {
                m.record_result_open(key);
            }
        });
        if let Some((_, Some(path))) = selected {
            crate::views::context_menu::open_path_in_os(&path);
        } else {
            cx.activate(true);
        }
        window.remove_window();
    }
}

impl Render for QuickBarView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = theme::active_colors(cx);

        div()
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(hsla(0.0, 0.0, 0.0, 0.38))
            .key_context("QuickBar")
            .on_action(cx.listener(|_, _: &ClearSearch, window, _cx| {
                window.remove_window();
            }))
            .on_action(cx.listener(|this, _: &SelectNext, _window, cx| {
                this.model.update(cx, |m, cx| m.select_next(cx));
            }))
            .on_action(cx.listener(|this, _: &SelectPrev, _window, cx| {
                this.model.update(cx, |m, cx| m.select_previous(cx));
            }))
            .on_action(cx.listener(|this, _: &OpenSelected, window, cx| {
                this.open_selected_and_close(window, cx);
            }))
            .on_mouse_down_out(cx.listener(|_, _, window, _cx| {
                window.remove_window();
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                match event.keystroke.key.as_str() {
                    "escape" => window.remove_window(),
                    "arrowdown" | "down" => {
                        this.model.update(cx, |m, cx| m.select_next(cx));
                    }
                    "arrowup" | "up" => {
                        this.model.update(cx, |m, cx| m.select_previous(cx));
                    }
                    "enter" => {
                        this.open_selected_and_close(window, cx);
                    }
                    _ => {}
                }
            }))
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .w(px(900.))
                    .bg(colors.panel_bg)
                    .border_1()
                    .border_color(colors.match_highlight)
                    .rounded_xl()
                    .shadow_2xl()
                    .p_4()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(div().flex_shrink_0().child(self.search_view.clone()))
                    .child(self.render_history(cx))
                    .child(
                        div()
                            .flex_1()
                            .max_h(px(380.))
                            .overflow_hidden()
                            .child(self.results_view.clone()),
                    ),
            )
    }
}

impl QuickBarView {
    fn render_history(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = theme::active_colors(cx);
        let model = self.model.read(cx);
        if !model.query.is_empty() || model.history.is_empty() {
            return Empty.into_any_element();
        }

        let items = model.history.iter().take(5).cloned().collect::<Vec<_>>();

        div()
            .bg(colors.bg)
            .border_1()
            .border_color(colors.border)
            .rounded_md()
            .p_2()
            .flex()
            .flex_col()
            .gap_1()
            .text_size(px(12.))
            .text_color(colors.text_secondary)
            .child(
                div()
                    .text_color(colors.text_primary)
                    .font_weight(FontWeight::BOLD)
                    .child("Recent searches"),
            )
            .children(items.into_iter().map(|q| {
                div()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .hover(|s| s.bg(colors.divider))
                    .cursor_pointer()
                    .child(q.clone())
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            // Update query and trigger search
                            this.model.update(cx, |m, cx| {
                                m.set_query(q.clone(), cx);
                                m.push_history(&q);
                            });
                        }),
                    )
            }))
            .into_any_element()
    }
}
