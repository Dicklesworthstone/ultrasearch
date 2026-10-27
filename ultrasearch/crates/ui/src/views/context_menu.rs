use crate::model::state::SearchAppModel;
use crate::theme;
use gpui::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextMenuCommand {
    Open,
    OpenContainingFolder,
    CopyFullPath,
    CopyFile,
    Properties,
}

pub struct ContextMenu {
    pub items: Vec<ContextMenuItem>,
    pub position: Point<Pixels>,
    pub model: Entity<SearchAppModel>,
    pub focus_handle: FocusHandle,
}

pub struct ContextMenuItem {
    pub label: SharedString,
    pub icon: Option<&'static str>,
    pub command: ContextMenuCommand,
}

impl ContextMenu {
    pub fn new(
        position: Point<Pixels>,
        items: Vec<ContextMenuItem>,
        model: Entity<SearchAppModel>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            items,
            position,
            model,
            focus_handle: cx.focus_handle(),
        }
    }

    fn execute_command(&mut self, cmd: ContextMenuCommand, cx: &mut Context<Self>) {
        let selected = self
            .model
            .read(cx)
            .selected_row()
            .map(|h| (h.key, h.path.clone()));
        let Some((key, Some(path))) = selected else {
            return;
        };

        match cmd {
            ContextMenuCommand::Open => {
                self.model.update(cx, |m, _| m.record_result_open(key));
                open_path_in_os(&path);
            }
            ContextMenuCommand::OpenContainingFolder => {
                open_containing_folder_in_os(&path);
            }
            ContextMenuCommand::CopyFullPath => {
                cx.write_to_clipboard(ClipboardItem::new_string(path));
            }
            ContextMenuCommand::CopyFile => {
                copy_file_to_clipboard(&path, cx);
            }
            ContextMenuCommand::Properties => {
                show_file_properties_in_os(&path, cx);
            }
        }
    }
}

/// Opens `path` with its default app. On Windows this goes through Explorer,
/// not `cmd /C start`: cmd parsed the file name (an `&` in it split the
/// command line) and, ui.exe having no console, opened a window of its own.
pub fn open_path_in_os(path: &str) {
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("explorer").arg(path).spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(path).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
}

pub fn open_containing_folder_in_os(path: &str) {
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("explorer")
            .arg("/select,")
            .arg(path)
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open")
            .arg("-R")
            .arg(path)
            .spawn();
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(parent) = std::path::Path::new(path).parent() {
            let _ = std::process::Command::new("xdg-open").arg(parent).spawn();
        }
    }
}

/// Builds a standard Windows `CF_HDROP` (`DROPFILES`) binary payload for a single file path:
/// 20-byte `DROPFILES` header (`pFiles = 20`, `fWide = 1`) followed by UTF-16LE path and
/// double-NUL terminator.
pub fn build_cf_hdrop_payload(path: &str) -> Vec<u8> {
    let wide: Vec<u16> = path.encode_utf16().chain([0u16, 0u16]).collect();
    let mut buf = Vec::with_capacity(20 + wide.len() * 2);
    buf.extend_from_slice(&20u32.to_le_bytes()); // pFiles = 20
    buf.extend_from_slice(&0i32.to_le_bytes()); // pt.x = 0
    buf.extend_from_slice(&0i32.to_le_bytes()); // pt.y = 0
    buf.extend_from_slice(&0i32.to_le_bytes()); // fNC = FALSE
    buf.extend_from_slice(&1i32.to_le_bytes()); // fWide = TRUE (UTF-16)
    for unit in wide {
        buf.extend_from_slice(&unit.to_le_bytes());
    }
    buf
}

pub fn copy_file_to_clipboard<V: 'static>(path: &str, cx: &mut Context<V>) {
    let _payload = build_cf_hdrop_payload(path);
    cx.write_to_clipboard(ClipboardItem::new_string(path.to_string()));
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let escaped = path.replace('\'', "''");
        let _ = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!("Set-Clipboard -LiteralPath '{escaped}'"),
            ])
            .creation_flags(0x08000000)
            .spawn();
    }
}

pub fn show_file_properties_in_os<V: 'static>(path: &str, cx: &mut Context<V>) {
    #[cfg(target_os = "windows")]
    {
        use windows::core::{HSTRING, PCWSTR};
        use windows::Win32::UI::Shell::{SHObjectProperties, SHOP_FILEPATH};

        let path_wide = HSTRING::from(path);
        cx.spawn(|_, _: &mut AsyncApp| async move {
            unsafe {
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
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (path, cx);
    }
}

impl Render for ContextMenu {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = theme::active_colors(cx);

        div()
            .track_focus(&self.focus_handle)
            .absolute()
            .top(self.position.y)
            .left(self.position.x)
            .w(px(200.))
            .bg(colors.panel_bg)
            .border_1()
            .border_color(colors.border)
            .rounded_lg()
            .shadow_lg()
            .p_1()
            .flex()
            .flex_col()
            .gap_1()
            .on_mouse_down_out(cx.listener(|_, _, window, _| window.remove_window()))
            .children(self.items.iter().map(|item| {
                let command = item.command;
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1p5()
                    .rounded_md()
                    .hover(|s| s.bg(colors.selection_bg))
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            this.execute_command(command, cx);
                            window.remove_window();
                        }),
                    )
                    .children(item.icon.map(|i| div().child(i)))
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(colors.text_primary)
                            .child(item.label.clone()),
                    )
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::build_cf_hdrop_payload;

    #[test]
    fn cf_hdrop_payload_has_valid_dropfiles_header_and_double_nul() {
        let bytes = build_cf_hdrop_payload(r"C:\test.txt");
        assert_eq!(&bytes[0..4], &20u32.to_le_bytes());
        assert_eq!(&bytes[16..20], &1i32.to_le_bytes()); // fWide = 1
        assert!(bytes.ends_with(&[0, 0, 0, 0]));
    }
}
