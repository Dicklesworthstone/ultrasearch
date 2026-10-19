use crate::icon_cache::IconCache;
use crate::model::state::SearchAppModel;
use crate::theme::Theme;
use crate::views::quick_search::QuickBarView;
use gpui::{Entity, Global, WindowHandle};

pub struct GlobalAppState {
    pub app_model: Entity<SearchAppModel>,
    pub quick_bar: Option<WindowHandle<QuickBarView>>,
    pub icon_cache: Entity<IconCache>,
    pub theme: Entity<Theme>,
}

impl Global for GlobalAppState {}
