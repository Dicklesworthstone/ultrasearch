//! UltraSearch - World-class desktop search application
//!
//! A high-performance Windows desktop search engine combining instant filename
//! search with deep content indexing, wrapped in a beautiful native UI.
// Background: UI layer powered by GPUI and custom models.

use gpui::prelude::*;
use gpui::{App, AppContext, AsyncApp, KeyBinding, *};
use ui::actions::{CloseShortcuts, ToggleShortcuts};
use ui::globals::GlobalAppState;
use ui::icon_cache::IconCache;
use ui::model::state::{BackendMode, SearchAppModel};
use ui::theme::{self, Theme};
use ui::views::help_panel::HelpPanel;
use ui::views::onboarding::OnboardingView;
use ui::views::preview_view::PreviewView;
use ui::views::quick_search::QuickBarView;
use ui::views::results_table::ResultsView;
use ui::views::search_view::SearchView;
use ui::views::update_panel::UpdatePanel;

use ui::actions::*;

/// Main application window containing all UI components
struct UltraSearchWindow {
    model: Entity<SearchAppModel>,
    search_view: Entity<SearchView>,
    results_view: Entity<ResultsView>,
    preview_view: Entity<PreviewView>,
    onboarding_view: Entity<OnboardingView>,
    update_panel: Entity<UpdatePanel>,
    help_panel: Entity<HelpPanel>,
    focus_handle: FocusHandle,
}

impl UltraSearchWindow {
    fn new(cx: &mut Context<Self>, show_onboarding: bool) -> Self {
        let model = cx.new(SearchAppModel::new);

        // Update model with onboarding state
        model.update(cx, |model, _cx| {
            model.show_onboarding = show_onboarding;
        });

        let search_view = cx.new(|cx| SearchView::new(model.clone(), cx));
        let results_view = cx.new(|cx| ResultsView::new(model.clone(), cx));
        let preview_view = cx.new(|cx| PreviewView::new(model.clone(), cx));
        let onboarding_view = cx.new(|cx| OnboardingView::new(model.clone(), cx));
        let update_panel = cx.new(|cx| UpdatePanel::new(model.clone(), cx));
        let help_panel = cx.new(HelpPanel::new);

        let focus_handle = cx.focus_handle();

        Self {
            model,
            search_view,
            results_view,
            preview_view,
            onboarding_view,
            update_panel,
            help_panel,
            focus_handle,
        }
    }

    fn on_check_updates(
        &mut self,
        _: &CheckForUpdates,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.model
            .update(cx, |model, cx| model.check_for_updates(cx));
    }

    fn on_download_update(
        &mut self,
        _: &DownloadUpdate,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.model
            .update(cx, |model, cx| model.start_update_download(cx));
    }

    fn on_restart_update(
        &mut self,
        _: &RestartToUpdate,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.model
            .update(cx, |model, cx| model.restart_to_update(cx));
    }

    fn on_toggle_opt_in(
        &mut self,
        _: &ToggleUpdateOptIn,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.model.update(cx, |model, cx| {
            let new_val = !model.updates.opt_in;
            model.set_update_opt_in(new_val, cx);
        });
    }

    fn on_minimize_to_tray(
        &mut self,
        _: &MinimizeToTray,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Hide window; tray menu / hotkeys can restore it.
        cx.hide();
    }

    fn on_toggle_shortcuts(
        &mut self,
        _: &ToggleShortcuts,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.model.update(cx, |model, cx| {
            model.show_shortcuts = !model.show_shortcuts;
            cx.notify();
        });
    }

    fn on_close_shortcuts(
        &mut self,
        _: &CloseShortcuts,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.model.update(cx, |model, cx| {
            model.show_shortcuts = false;
            cx.notify();
        });
    }

    fn on_focus_search(&mut self, _: &FocusSearch, window: &mut Window, cx: &mut Context<Self>) {
        self.search_view.update(cx, |view, _cx| {
            window.focus(&view.focus_handle());
        });
    }

    fn on_clear_search(&mut self, _: &ClearSearch, _window: &mut Window, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| {
            model.set_query(String::new(), cx);
        });
        self.search_view.update(cx, |view, cx| {
            view.clear_search(cx);
        });
    }

    fn on_select_next(&mut self, _: &SelectNext, _window: &mut Window, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| model.select_next(cx));
    }

    fn on_select_prev(&mut self, _: &SelectPrev, _window: &mut Window, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| model.select_previous(cx));
    }

    fn on_submit_search(&mut self, _: &SubmitSearch, window: &mut Window, cx: &mut Context<Self>) {
        self.search_view.update(cx, |view, _cx| {
            window.focus(&view.focus_handle());
        });
        self.open_selected(window, cx);
    }

    fn on_open_selected(&mut self, _: &OpenSelected, window: &mut Window, cx: &mut Context<Self>) {
        self.open_selected(window, cx);
    }

    fn on_mode_metadata(&mut self, _: &ModeMetadata, _window: &mut Window, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| {
            model.set_backend_mode(BackendMode::MetadataOnly, cx)
        });
    }

    fn on_mode_mixed(&mut self, _: &ModeMixed, _window: &mut Window, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| {
            model.set_backend_mode(BackendMode::Mixed, cx)
        });
    }

    fn on_mode_content(&mut self, _: &ModeContent, _window: &mut Window, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| {
            model.set_backend_mode(BackendMode::ContentOnly, cx)
        });
    }

    fn on_copy_selected_path(
        &mut self,
        _: &CopySelectedPath,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(path) = self
            .model
            .read(cx)
            .selected_row()
            .and_then(|hit| hit.path.clone())
        {
            cx.write_to_clipboard(ClipboardItem::new_string(path));
        }
    }

    fn on_copy_selected_file(
        &mut self,
        _: &CopySelectedFile,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // For now, copy path to clipboard. TODO: add file drop clipboard on Windows.
        if let Some(path) = self
            .model
            .read(cx)
            .selected_row()
            .and_then(|hit| hit.path.clone())
        {
            cx.write_to_clipboard(ClipboardItem::new_string(path));
        }
    }

    fn on_quit(&mut self, _: &QuitApp, _window: &mut Window, cx: &mut Context<Self>) {
        cx.quit();
    }

    fn on_finish_onboarding(
        &mut self,
        _: &crate::FinishOnboarding,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.model.update(cx, |model, cx| {
            model.show_onboarding = false;
            cx.notify();
        });
    }

    fn on_open_folder(
        &mut self,
        _: &crate::OpenContainingFolder,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(path) = self
            .model
            .read(cx)
            .selected_row()
            .and_then(|hit| hit.path.clone())
        {
            #[cfg(target_os = "windows")]
            {
                let _ = std::process::Command::new("explorer")
                    .arg("/select,")
                    .arg(&path)
                    .spawn();
            }
            #[cfg(target_os = "macos")]
            {
                let _ = std::process::Command::new("open")
                    .arg("-R")
                    .arg(&path)
                    .spawn();
            }
            #[cfg(target_os = "linux")]
            {
                if let Some(parent) = std::path::Path::new(&path).parent() {
                    let _ = std::process::Command::new("xdg-open").arg(parent).spawn();
                }
            }
        }
    }

    fn on_show_properties(
        &mut self,
        _: &crate::ShowProperties,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(path) = self
            .model
            .read(cx)
            .selected_row()
            .and_then(|hit| hit.path.clone())
        {
            #[cfg(target_os = "windows")]
            {
                use windows::core::{HSTRING, PCWSTR};
                use windows::Win32::UI::Shell::{SHObjectProperties, SHOP_FILEPATH};

                let path_wide = HSTRING::from(&path);
                // Run on background thread to avoid blocking UI
                cx.spawn(|_, _: &mut AsyncApp| async move {
                    unsafe {
                        // 0 = props page
                        let _ = SHObjectProperties(
                            None,
                            SHOP_FILEPATH,
                            PCWSTR(path_wide.as_ptr()),
                            PCWSTR::null(),
                        );
                    }
                })
                .detach();
            }
        }
    }

    fn on_hotkey_conflict_general(
        &mut self,
        _: &HotkeyConflictGeneral,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.model.update(cx, |model, cx| {
            model.set_hotkey_conflict(
                "Alt+Space could not be registered; another app is using it.",
                cx,
            );
        });
    }

    fn on_hotkey_conflict_powertoys(
        &mut self,
        _: &HotkeyConflictPowerToys,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.model.update(cx, |model, cx| {
            model.set_hotkey_conflict(
                "Alt+Space is already bound by PowerToys Run. Disable or rebind it there to use UltraSearch.",
                cx,
            );
        });
    }

    fn open_selected(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self
            .model
            .read(cx)
            .selected_row()
            .and_then(|hit| hit.path.clone())
        {
            #[cfg(target_os = "windows")]
            {
                let _ = std::process::Command::new("explorer").arg(&path).spawn();
            }
            #[cfg(target_os = "macos")]
            {
                let _ = std::process::Command::new("open").arg(&path).spawn();
            }
            #[cfg(target_os = "linux")]
            {
                let _ = std::process::Command::new("xdg-open").arg(&path).spawn();
            }
        }
    }
}

impl Render for UltraSearchWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = theme::active_colors(cx);
        let read = self.model.read(cx);
        let show_onboarding = read.show_onboarding;
        let conflict = read.hotkey_conflict.clone();
        let show_shortcuts = read.show_shortcuts;

        div()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_focus_search))
            .on_action(cx.listener(Self::on_clear_search))
            .on_action(cx.listener(Self::on_select_next))
            .on_action(cx.listener(Self::on_select_prev))
            .on_action(cx.listener(Self::on_submit_search))
            .on_action(cx.listener(Self::on_open_selected))
            .on_action(cx.listener(Self::on_mode_metadata))
            .on_action(cx.listener(Self::on_mode_mixed))
            .on_action(cx.listener(Self::on_mode_content))
            .on_action(cx.listener(Self::on_copy_selected_path))
            .on_action(cx.listener(Self::on_copy_selected_file))
            .on_action(cx.listener(Self::on_check_updates))
            .on_action(cx.listener(Self::on_download_update))
            .on_action(cx.listener(Self::on_restart_update))
            .on_action(cx.listener(Self::on_toggle_opt_in))
            .on_action(cx.listener(Self::on_minimize_to_tray))
            .on_action(cx.listener(Self::on_quit))
            .on_action(cx.listener(Self::on_finish_onboarding))
            .on_action(cx.listener(Self::on_open_folder))
            .on_action(cx.listener(Self::on_show_properties))
            .on_action(cx.listener(Self::on_hotkey_conflict_general))
            .on_action(cx.listener(Self::on_hotkey_conflict_powertoys))
            .on_action(cx.listener(Self::on_toggle_shortcuts))
            .on_action(cx.listener(Self::on_close_shortcuts))
            .size_full()
            .flex()
            .flex_col()
            .relative()
            .bg(colors.bg)
            .text_color(colors.text_primary)
            .child(
                // Search header - fixed at top
                div()
                    .flex_shrink_0()
                    .child(self.update_panel.clone())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(self.search_view.clone())
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(colors.text_secondary)
                                    .cursor_pointer()
                                    .child("Help")
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|this, _, _, cx| {
                                            this.model.update(cx, |m, cx| {
                                                m.show_shortcuts = true;
                                                cx.notify();
                                            });
                                        }),
                                    ),
                            ),
                    ),
            )
            .child(
                // Main content area - flexible height
                div()
                    .flex_1()
                    .flex()
                    .overflow_hidden()
                    .child(
                        // Results table - 60% width
                        div()
                            .flex_basis(relative(0.6))
                            .flex_grow()
                            .overflow_hidden()
                            .border_r_1()
                            .border_color(colors.divider)
                            .child(self.results_view.clone()),
                    )
                    .child(
                        // Preview pane - 40% width
                        div()
                            .flex_basis(relative(0.4))
                            .flex_shrink_0()
                            .overflow_hidden()
                            .child(self.preview_view.clone()),
                    ),
            )
            // Onboarding Overlay
            .when(show_onboarding, |this| {
                this.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .bg(hsla(0.0, 0.0, 0.0, 0.5)) // Dim background
                        .child(self.onboarding_view.clone()),
                )
            })
            // Shortcut overlay
            .when(show_shortcuts, |this| this.child(self.help_panel.clone()))
            .when(conflict.is_some(), |this| {
                let msg = conflict
                    .clone()
                    .unwrap_or_else(|| "Alt+Space is in use by another app.".into());
                this.child(
                    div()
                        .absolute()
                        .top(px(14.))
                        .right(px(14.))
                        .bg(colors.panel_bg)
                        .border_1()
                        .border_color(colors.match_highlight)
                        .rounded_md()
                        .shadow_lg()
                        .p_3()
                        .text_size(px(12.))
                        .text_color(colors.text_primary)
                        .child(msg),
                )
            })
    }
}

fn main() {
    // Provide a Tokio runtime so async tasks in the UI (status/search polling) have a reactor.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");
    let _rt_guard = runtime.enter();

    // Load configuration
    let config = core_types::config::load_or_create_config(None).ok();
    let show_onboarding = config
        .as_ref()
        .map(|c| c.volumes.is_empty())
        .unwrap_or(true);

    if config.is_none() {
        eprintln!("Failed to load configuration, proceeding with defaults (and onboarding).");
    }

    // Start Background Tasks (Tray + Hotkeys)
    let bg_rx = match ui::background::spawn() {
        Ok(rx) => Some(rx),
        Err(e) => {
            eprintln!("Failed to spawn background tasks: {}", e);
            None
        }
    };

    // Initialize GPUI application
    Application::new().run(move |cx: &mut App| {
        // Initialize Theme
        let initial_theme = Theme::detect();
        let theme_model = cx.new(|_| Theme::new(initial_theme));

        // Theme Polling Task
        let theme_handle = theme_model.clone();
        cx.spawn(|cx: &mut AsyncApp| {
            let cx = cx.clone();
            async move {
                let mut last_theme = Theme::detect();
                loop {
                    cx.background_executor()
                        .timer(std::time::Duration::from_secs(2))
                        .await;

                    let current = Theme::detect();
                    if current != last_theme {
                        last_theme = current;
                        let _ = cx.update(|cx| {
                            theme_handle.update(cx, |theme, cx| {
                                theme.mode = current;
                                cx.notify();
                            });
                        });
                    }
                }
            }
        })
        .detach();

        // Initialize Icon Cache
        let icon_cache = cx.new(IconCache::new);
        cx.set_global(GlobalAppState {
            quick_bar: None,
            icon_cache,
            theme: theme_model,
        });

        // Handle Background Events
        if let Some(rx) = bg_rx {
            cx.spawn(|cx: &mut AsyncApp| {
                let cx = cx.clone();
                async move {
                    loop {
                        if let Ok(action) = rx.try_recv() {
                            match action {
                                ui::background::UserAction::Show => {
                                    // Activate app (bring to front)
                                    let _ = cx.update(|cx: &mut App| cx.activate(true));
                                }
                                ui::background::UserAction::ShowHelp => {
                                    let _ = cx.update(|cx: &mut App| {
                                        cx.dispatch_action(&ToggleShortcuts)
                                    });
                                }
                                ui::background::UserAction::Quit => {
                                    // Quit app
                                    let _ = cx.update(|cx: &mut App| cx.quit());
                                    break;
                                }
                                ui::background::UserAction::ToggleQuickSearch => {
                                    // Toggle Quick Search Window
                                    let _ = cx.update(|cx: &mut App| {
                                        let mut global_state =
                                            cx.global::<GlobalAppState>().quick_bar;

                                        if let Some(handle) = global_state.as_ref() {
                                            if handle
                                                .update(cx, |view, window, _| {
                                                    window.focus(&view.focus_handle())
                                                })
                                                .is_ok()
                                            {
                                                // Window exists and activated
                                                return;
                                            } else {
                                                // Window dropped/closed
                                                global_state = None;
                                            }
                                        }

                                        if global_state.is_none() {
                                            let handle = cx
                                                .open_window(
                                                    WindowOptions {
                                                        window_bounds: Some(
                                                            WindowBounds::Windowed(Bounds {
                                                                origin: Point {
                                                                    x: px(120.0),
                                                                    y: px(60.0),
                                                                },
                                                                size: Size {
                                                                    width: px(1500.0),
                                                                    height: px(900.0),
                                                                },
                                                            }),
                                                        ),
                                                        titlebar: None,
                                                        window_background:
                                                            WindowBackgroundAppearance::Transparent,
                                                        kind: WindowKind::PopUp,
                                                        ..WindowOptions::default()
                                                    },
                                                    |_, cx| {
                                                        let model = cx.new(SearchAppModel::new);
                                                        cx.new(|cx| QuickBarView::new(model, cx))
                                                    },
                                                )
                                                .expect("failed to open quick bar");

                                            cx.update_global::<GlobalAppState, _>(|state, _| {
                                                state.quick_bar = Some(handle);
                                            });
                                        }
                                    });
                                }
                                ui::background::UserAction::CheckUpdates => {
                                    let _ = cx.update(|cx: &mut App| {
                                        cx.dispatch_action(&CheckForUpdates)
                                    });
                                }
                                ui::background::UserAction::RestartUpdate => {
                                    let _ = cx.update(|cx: &mut App| {
                                        cx.dispatch_action(&RestartToUpdate)
                                    });
                                }
                                ui::background::UserAction::ToggleOptIn => {
                                    let _ = cx.update(|cx: &mut App| {
                                        cx.dispatch_action(&ToggleUpdateOptIn)
                                    });
                                }
                                ui::background::UserAction::HotkeyConflict { powertoys } => {
                                    let _ = cx.update(|cx: &mut App| {
                                        if powertoys {
                                            cx.dispatch_action(&HotkeyConflictPowerToys)
                                        } else {
                                            cx.dispatch_action(&HotkeyConflictGeneral)
                                        }
                                    });
                                }
                            }
                        }
                        // Poll interval
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(100))
                            .await;
                    }
                }
            })
            .detach();
        }
        cx.bind_keys([
            KeyBinding::new("cmd-k", FocusSearch, None),
            KeyBinding::new("ctrl-k", FocusSearch, None),
            KeyBinding::new("f1", ToggleShortcuts, None),
            KeyBinding::new("ctrl-/", ToggleShortcuts, None),
            KeyBinding::new("cmd-/", ToggleShortcuts, None),
            KeyBinding::new("escape", ClearSearch, None),
            KeyBinding::new("enter", SubmitSearch, None),
            KeyBinding::new("down", SelectNext, None),
            KeyBinding::new("up", SelectPrev, None),
            KeyBinding::new("cmd-1", ModeMetadata, None),
            KeyBinding::new("ctrl-1", ModeMetadata, None),
            KeyBinding::new("cmd-2", ModeMixed, None),
            KeyBinding::new("ctrl-2", ModeMixed, None),
            KeyBinding::new("cmd-3", ModeContent, None),
            KeyBinding::new("ctrl-3", ModeContent, None),
            KeyBinding::new("cmd-o", OpenSelected, None),
            KeyBinding::new("ctrl-o", OpenSelected, None),
            KeyBinding::new("cmd-c", CopySelectedPath, None),
            KeyBinding::new("ctrl-c", CopySelectedPath, None),
            KeyBinding::new("ctrl-shift-c", CopySelectedFile, None),
            KeyBinding::new("alt-enter", ShowProperties, None),
            KeyBinding::new("ctrl-shift-o", OpenContainingFolder, None),
            KeyBinding::new("cmd-q", QuitApp, None),
            KeyBinding::new("ctrl-q", QuitApp, None),
            KeyBinding::new("ctrl-m", MinimizeToTray, None),
        ]);

        // Open the main window
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Point {
                        x: px(100.0),
                        y: px(100.0),
                    },
                    size: Size {
                        width: px(1400.0),
                        height: px(900.0),
                    },
                })),
                titlebar: Some(TitlebarOptions {
                    title: Some(SharedString::from("UltraSearch")),
                    appears_transparent: false,
                    traffic_light_position: None,
                }),
                window_min_size: Some(Size {
                    width: px(960.0),
                    height: px(600.0),
                }),
                window_background: WindowBackgroundAppearance::Opaque,
                app_id: Some("com.ultrasearch.desktop".to_string()),
                ..WindowOptions::default()
            },
            move |_, cx| cx.new(|cx| UltraSearchWindow::new(cx, show_onboarding)),
        )
        .expect("Failed to open window");

        // Print startup message
        eprintln!("✅ UltraSearch started successfully!");
        eprintln!("🌀 Keyboard shortcuts:");
        eprintln!("   Ctrl/Cmd+K    - Focus search");
        eprintln!("   Escape        - Clear search");
        eprintln!("   ↑/↓           - Navigate results");
        eprintln!("   Ctrl+1/2/3    - Switch search modes");
        eprintln!("   Ctrl+Q        - Quit");

        cx.activate(true);
    });
}
