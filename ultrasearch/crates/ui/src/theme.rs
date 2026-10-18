use gpui::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeMode {
    Light,
    Dark,
}

pub struct Theme {
    pub mode: ThemeMode,
    pub tokens: Tokens,
}

impl Theme {
    pub fn new(mode: ThemeMode) -> Self {
        Self {
            mode,
            tokens: Tokens::default(),
        }
    }

    pub fn detect() -> ThemeMode {
        if let Ok(override_mode) = std::env::var("ULTRASEARCH_THEME") {
            if override_mode.trim().eq_ignore_ascii_case("light") {
                return ThemeMode::Light;
            }
            if override_mode.trim().eq_ignore_ascii_case("dark") {
                return ThemeMode::Dark;
            }
        }
        #[cfg(target_os = "windows")]
        {
            use windows::core::w;
            use windows::Win32::System::Registry::{
                RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD,
            };

            let subkey = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
            let value = w!("AppsUseLightTheme");

            let mut data: u32 = 0;
            let mut size = std::mem::size_of::<u32>() as u32;

            let result = unsafe {
                RegGetValueW(
                    HKEY_CURRENT_USER,
                    subkey,
                    value,
                    RRF_RT_REG_DWORD,
                    None,
                    Some(&mut data as *mut _ as *mut _),
                    Some(&mut size),
                )
            };

            if result.is_ok() {
                if data == 1 {
                    return ThemeMode::Light;
                } else {
                    return ThemeMode::Dark;
                }
            }
        }
        ThemeMode::Dark
    }

    pub fn colors(&self) -> ThemeColors {
        match self.mode {
            ThemeMode::Dark => ThemeColors {
                bg: hsla(0.0, 0.0, 0.102, 1.0),           // #1a1a1a
                divider: hsla(0.0, 0.0, 0.2, 1.0),        // #333333
                text_primary: hsla(0.0, 0.0, 0.894, 1.0), // #e4e4e4
                text_secondary: hsla(0.0, 0.0, 0.6, 1.0), // #999999
                accent: hsla(0.08, 0.92, 0.52, 1.0),      // richer orange for primary actions
                match_highlight: hsla(0.08, 0.70, 0.55, 0.65),
                selection_bg: hsla(0.6, 0.5, 0.4, 0.3),
                border: hsla(0.0, 0.0, 0.25, 1.0),
                panel_bg: hsla(0.0, 0.0, 0.13, 1.0),
            },
            ThemeMode::Light => ThemeColors {
                bg: hsla(0.0, 0.0, 0.98, 1.0),            // #fafafa
                divider: hsla(0.0, 0.0, 0.9, 1.0),        // #e5e5e5
                text_primary: hsla(0.0, 0.0, 0.1, 1.0),   // #1a1a1a
                text_secondary: hsla(0.0, 0.0, 0.4, 1.0), // #666666
                accent: hsla(0.08, 0.88, 0.50, 1.0),
                match_highlight: hsla(0.08, 0.75, 0.52, 0.55),
                selection_bg: hsla(0.6, 0.6, 0.8, 0.2),
                border: hsla(0.0, 0.0, 0.85, 1.0),
                panel_bg: hsla(0.0, 0.0, 0.95, 1.0),
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct ThemeColors {
    pub bg: Hsla,
    pub divider: Hsla,
    pub text_primary: Hsla,
    pub text_secondary: Hsla,
    pub accent: Hsla,
    pub match_highlight: Hsla,
    pub selection_bg: Hsla,
    pub border: Hsla,
    pub panel_bg: Hsla,
}

#[derive(Debug, Clone)]
pub struct Tokens {
    pub radius_sm: Pixels,
    pub radius_md: Pixels,
    pub radius_lg: Pixels,
    pub spacing_sm: f32,
    pub spacing_md: f32,
    pub spacing_lg: f32,
    pub type_sm: Pixels,
    pub type_md: Pixels,
    pub type_lg: Pixels,
}

impl Default for Tokens {
    fn default() -> Self {
        Self {
            radius_sm: px(6.),
            radius_md: px(10.),
            radius_lg: px(14.),
            spacing_sm: 6.0,
            spacing_md: 12.0,
            spacing_lg: 20.0,
            type_sm: px(12.),
            type_md: px(14.),
            type_lg: px(16.),
        }
    }
}

use crate::globals::GlobalAppState;

pub fn active_colors(cx: &App) -> ThemeColors {
    if let Some(state) = cx.try_global::<GlobalAppState>() {
        state.theme.read(cx).colors()
    } else {
        Theme::new(ThemeMode::Dark).colors()
    }
}

pub fn active_tokens(cx: &App) -> Tokens {
    if let Some(state) = cx.try_global::<GlobalAppState>() {
        state.theme.read(cx).tokens.clone()
    } else {
        Tokens::default()
    }
}
