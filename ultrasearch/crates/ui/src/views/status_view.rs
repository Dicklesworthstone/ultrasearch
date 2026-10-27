use crate::actions::CloseStatus;
use crate::model::state::SearchAppModel;
use crate::theme;
use gpui::prelude::FluentBuilder;
use gpui::*;

pub struct StatusView {
    focus_handle: FocusHandle,
    model: Entity<SearchAppModel>,
}

impl StatusView {
    pub fn new(model: Entity<SearchAppModel>, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            model,
        }
    }

    fn render_kv_row(&self, key: &str, value: String, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = theme::active_colors(cx);
        div()
            .flex()
            .justify_between()
            .py_1()
            .border_b_1()
            .border_color(colors.divider)
            .child(
                div()
                    .text_color(colors.text_secondary)
                    .child(key.to_string()),
            )
            .child(
                div()
                    .text_color(colors.text_primary)
                    .font_weight(FontWeight::MEDIUM)
                    .child(value),
            )
    }

    fn format_bytes(bytes: u64) -> String {
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
}

impl Render for StatusView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = theme::active_colors(cx);
        let status = self.model.read(cx).status.clone();
        let totals = status
            .volumes
            .iter()
            .fold((0u64, 0u64, 0u64, 0u64), |acc, v| {
                (
                    acc.0 + v.indexed_files,
                    acc.1 + v.pending_files,
                    acc.2 + v.indexed_bytes,
                    acc.3 + v.pending_bytes,
                )
            });
        let (indexed_files, pending_files, indexed_bytes, pending_bytes) = totals;

        let total_jobs = status.content_jobs_total;
        let remaining_jobs = status.content_jobs_remaining;
        let total_bytes = status.content_bytes_total;
        let remaining_bytes = status.content_bytes_remaining;
        let queue_depth = status
            .metrics
            .as_ref()
            .and_then(|m| m.queue_depth)
            .unwrap_or(0);
        let active_workers = status
            .metrics
            .as_ref()
            .and_then(|m| m.active_workers)
            .unwrap_or(0);

        let (
            progress_pct,
            display_completed,
            display_remaining,
            display_bytes_done,
            display_bytes_left,
        ) = if total_jobs > 0 {
            let completed = total_jobs.saturating_sub(remaining_jobs);
            let pct = (completed as f64 / total_jobs as f64) * 100.0;
            (
                pct.min(100.0),
                completed,
                remaining_jobs,
                total_bytes.saturating_sub(remaining_bytes),
                remaining_bytes,
            )
        } else {
            let total_files = indexed_files + pending_files;
            let pct = if total_files > 0 {
                (indexed_files as f64 / total_files as f64) * 100.0
            } else {
                0.0
            };
            (
                pct.min(100.0),
                indexed_files,
                pending_files,
                indexed_bytes,
                pending_bytes,
            )
        };
        let eta_label = if !status.connected {
            "Unknown while disconnected".to_string()
        } else if !status.ingestion_state().starts_with("watching") {
            "Waiting for initial filename reconciliation".to_string()
        } else if display_remaining == 0 {
            "Complete".to_string()
        } else if queue_depth > 0 || active_workers > 0 {
            format!(
                "≈ {} jobs remaining ({} workers active)",
                display_remaining, active_workers
            )
        } else {
            "Waiting to start indexing…".to_string()
        };

        div()
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(colors.bg)
            .text_color(colors.text_primary)
            .flex()
            .items_center()
            .justify_center()
            .on_action(cx.listener(|this, _: &CloseStatus, _window, cx| {
                this.model.update(cx, |m, cx| {
                    m.show_status = false;
                    cx.notify();
                });
            }))
            .child(
                div()
                    .w(px(600.))
                    .h(px(500.)) // Fixed height for now
                    .bg(colors.panel_bg)
                    .border_1()
                    .border_color(colors.border)
                    .rounded_xl()
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    // Header
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .items_center()
                            .p_4()
                            .border_b_1()
                            .border_color(colors.border)
                            .child(
                                div()
                                    .text_size(px(18.))
                                    .font_weight(FontWeight::BOLD)
                                    .child("Service Health Dashboard"),
                            )
                            .child(
                                div()
                                    .child("✕")
                                    .cursor_pointer()
                                    .text_color(colors.text_secondary)
                                    .hover(|s| s.text_color(colors.text_primary))
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|this, _, window, cx| {
                                            this.model.update(cx, |m, cx| {
                                                m.show_status = false;
                                                cx.notify();
                                            });
                                            window.dispatch_action(Box::new(CloseStatus), cx);
                                        }),
                                    ),
                            ),
                    )
                    // Body
                    .child(
                        div()
                            .id("service-health-body")
                            .flex_1()
                            .overflow_y_scroll()
                            .p_6()
                            .flex()
                            .flex_col()
                            .gap_6()
                            // Section: General
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_size(px(14.))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(colors.match_highlight)
                                            .child("General"),
                                    )
                                    .child(self.render_kv_row(
                                        "Connection",
                                        status.connection_label().into(),
                                        cx,
                                    ))
                                    .child(self.render_kv_row(
                                        "Service Host",
                                        status.served_by.clone().unwrap_or("-".into()),
                                        cx,
                                    ))
                                    .child(
                                        self.render_kv_row(
                                            "Scheduler State",
                                            status
                                                .indexing_state
                                                .split("; ingestion=")
                                                .next()
                                                .unwrap_or(&status.indexing_state)
                                                .into(),
                                            cx,
                                        ),
                                    )
                                    .child(self.render_kv_row(
                                        "Filename indexing",
                                        status.ingestion_state().to_string(),
                                        cx,
                                    ))
                                    .when_some(status.connection_error.as_ref(), |this, error| {
                                        this.child(div().text_size(px(12.)).child(error.clone()))
                                    })
                                    .when_some(status.search_error.as_ref(), |this, error| {
                                        this.child(
                                            div()
                                                .text_size(px(12.))
                                                .child(format!("Search: {error}")),
                                        )
                                    })
                                    .when_some(status.config_error.as_ref(), |this, error| {
                                        this.child(
                                            div()
                                                .text_size(px(12.))
                                                .child(format!("Configuration: {error}")),
                                        )
                                    }),
                            )
                            // Section: Progress
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_size(px(14.))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(colors.match_highlight)
                                            .child("Indexing Progress"),
                                    )
                                    .child(self.render_kv_row(
                                        "Indexed files",
                                        format!("{}", display_completed),
                                        cx,
                                    ))
                                    .child(self.render_kv_row(
                                        "Pending / queued",
                                        format!("{}", display_remaining),
                                        cx,
                                    ))
                                    .child(self.render_kv_row(
                                        "Bytes indexed",
                                        Self::format_bytes(display_bytes_done),
                                        cx,
                                    ))
                                    .child(self.render_kv_row(
                                        "Bytes remaining (est.)",
                                        Self::format_bytes(display_bytes_left),
                                        cx,
                                    ))
                                    .child(self.render_kv_row("ETA", eta_label.clone(), cx))
                                    .when(status.metrics.is_some(), |d: Div| {
                                        let m = status.metrics.as_ref().unwrap();
                                        d.child(self.render_kv_row(
                                            "Queue depth",
                                            format!("{}", m.queue_depth.unwrap_or(0)),
                                            cx,
                                        ))
                                        .child(self.render_kv_row(
                                            "Active workers",
                                            format!("{}", m.active_workers.unwrap_or(0)),
                                            cx,
                                        ))
                                        .child(
                                            self.render_kv_row(
                                                "Jobs enqueued",
                                                format!("{}", m.content_enqueued.unwrap_or(0)),
                                                cx,
                                            ),
                                        )
                                    })
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap_2()
                                            .child(
                                                div().text_color(colors.text_secondary).child(
                                                    format!("{:.0}% complete", progress_pct),
                                                ),
                                            )
                                            .child(
                                                div()
                                                    .w(px(200.))
                                                    .h(px(8.))
                                                    .rounded_full()
                                                    .bg(colors.divider)
                                                    .child(
                                                        div()
                                                            .h_full()
                                                            .rounded_full()
                                                            .bg(colors.match_highlight)
                                                            .w(px((progress_pct as f32).max(0.0)
                                                                * 2.0)),
                                                    ),
                                            ),
                                    ),
                            )
                            // Section: Metrics
                            .when(status.metrics.is_some(), |this| {
                                let m = status.metrics.as_ref().unwrap();
                                this.child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .gap_2()
                                        .child(
                                            div()
                                                .text_size(px(14.))
                                                .font_weight(FontWeight::BOLD)
                                                .text_color(colors.match_highlight)
                                                .child("Metrics"),
                                        )
                                        .child(self.render_kv_row(
                                            "Latency (P50)",
                                            format!(
                                                "{:.2} ms",
                                                m.search_latency_ms_p50.unwrap_or(0.0)
                                            ),
                                            cx,
                                        ))
                                        .child(self.render_kv_row(
                                            "Latency (P95)",
                                            format!(
                                                "{:.2} ms",
                                                m.search_latency_ms_p95.unwrap_or(0.0)
                                            ),
                                            cx,
                                        ))
                                        .child(self.render_kv_row(
                                            "Worker CPU",
                                            format!("{:.1}%", m.worker_cpu_pct.unwrap_or(0.0)),
                                            cx,
                                        ))
                                        .child(self.render_kv_row(
                                            "Worker Mem",
                                            Self::format_bytes(m.worker_mem_bytes.unwrap_or(0)),
                                            cx,
                                        ))
                                        .child(self.render_kv_row(
                                            "Queue Depth",
                                            format!("{}", m.queue_depth.unwrap_or(0)),
                                            cx,
                                        ))
                                        .child(self.render_kv_row(
                                            "Active Workers",
                                            format!("{}", m.active_workers.unwrap_or(0)),
                                            cx,
                                        ))
                                        .child(self.render_kv_row(
                                            "Jobs Enqueued",
                                            format!("{}", m.content_enqueued.unwrap_or(0)),
                                            cx,
                                        ))
                                        .child(self.render_kv_row(
                                            "Jobs Dropped",
                                            format!("{}", m.content_dropped.unwrap_or(0)),
                                            cx,
                                        )),
                                )
                            })
                            // Section: Volumes
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_size(px(14.))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(colors.match_highlight)
                                            .child("Volumes"),
                                    )
                                    .children(status.volumes.iter().map(|v| {
                                        div()
                                            .p_3()
                                            .bg(colors.bg)
                                            .rounded_md()
                                            .border_1()
                                            .border_color(colors.divider)
                                            .child(
                                                div()
                                                    .font_weight(FontWeight::BOLD)
                                                    .child(format!("Volume {}", v.volume)),
                                            )
                                            .child(
                                                div()
                                                    .text_size(px(12.))
                                                    .text_color(colors.text_secondary)
                                                    .child(format!(
                                                        "Indexed: {} files",
                                                        v.indexed_files
                                                    )),
                                            )
                                            .child(
                                                div()
                                                    .text_size(px(12.))
                                                    .text_color(colors.text_secondary)
                                                    .child(format!(
                                                        "Pending: {} files",
                                                        v.pending_files
                                                    )),
                                            )
                                    })),
                            ),
                    ),
            )
    }
}
