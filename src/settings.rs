//! User preferences + the Settings window (⌘,).
//!
//! Prefs live in `~/Library/Application Support/tusk/settings.json` and in a
//! process-wide `RwLock`, so render code reads fonts with plain functions
//! ([`ui_font`], [`table_font`], …) without threading `cx` through.
//! Every change is saved immediately and applied live to all windows.

use std::sync::{LazyLock, RwLock};

use gpui_kit::assets::IconName;
use gpui_kit::component::Disableable as _;
use gpui_kit::component::searchable_list::SearchableVec;
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::setting::{
    NumberFieldOptions, SettingField, SettingGroup, SettingItem, SettingPage, Settings,
};
use gpui_kit::component::{ActiveTheme as _, IndexPath, Root, TitleBar, v_flex};
use gpui_kit::*;
use serde::{Deserialize, Serialize};

use crate::themes;

/// Which theme slot is active.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Appearance {
    /// Follow macOS light / dark.
    #[default]
    System,
    Light,
    Dark,
    /// Follow the Omarchy desktop's active theme (colors and light / dark).
    Omarchy,
}

impl Appearance {
    pub const ALL: [Appearance; 4] = [
        Appearance::System,
        Appearance::Light,
        Appearance::Dark,
        Appearance::Omarchy,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Appearance::System => "System",
            Appearance::Light => "Light",
            Appearance::Dark => "Dark",
            Appearance::Omarchy => crate::omarchy::NAME,
        }
    }

    fn from_label(s: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|a| a.label() == s)
            .unwrap_or_default()
    }

    /// Modes offered in the pickers: "Omarchy" only where Omarchy's theme
    /// exists (or it is the current choice).
    pub fn available() -> Vec<Appearance> {
        let current = get().appearance;
        Self::ALL
            .into_iter()
            .filter(|a| *a != Appearance::Omarchy || *a == current || crate::omarchy::detected())
            .collect()
    }

    /// Default mode: follow Omarchy's theme when it is there.
    fn initial() -> Self {
        if crate::omarchy::detected() {
            Appearance::Omarchy
        } else {
            Appearance::System
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub appearance: Appearance,
    /// Theme used in light mode / dark mode (chosen separately).
    pub light_theme: String,
    pub dark_theme: String,
    /// Pre-mode single `theme` from older settings files (migrated on load).
    #[serde(rename = "theme", skip_serializing)]
    legacy_theme: Option<String>,
    /// Interface font. Its size is the rem base, so it scales the whole UI
    /// .
    pub ui_font_family: String,
    pub ui_font_size: f32,
    /// Data grid, structure view and SQL editor ("buffer" font).
    pub table_font_family: String,
    pub table_font_size: f32,
    // ---- general ----
    /// Reconnect to the last connection when the app starts.
    pub reopen_last: bool,
    /// Check for stable releases daily on Windows and Linux.
    pub automatic_update_checks: bool,
    /// Separate database engines by type in the new-connection selector.
    pub group_database_types: bool,
    /// `statement_timeout` for new connections, seconds (0 = none).
    pub query_timeout_secs: u32,
    /// Linux renderer device ID (four hexadecimal digits); empty = automatic.
    pub gpu_device: String,
    // ---- SQL editor ----
    pub editor_line_numbers: bool,
    pub editor_soft_wrap: bool,
    pub editor_tab_size: u32,
    /// Keyword completions insert UPPERCASE.
    pub editor_uppercase_keywords: bool,
    // ---- data grid ----
    pub grid_stripes: bool,
    pub null_text: String,
    // ---- safe mode ----
    /// Ask before DROP / TRUNCATE / ALTER and UPDATE / DELETE without WHERE.
    pub confirm_destructive: bool,
    /// Ask before ⌘S writes pending changes, and before the SQL editor runs
    /// a statement that writes (INSERT / UPDATE / DELETE / DDL).
    pub confirm_save: bool,
    /// Accent (primary) color name, see [`ACCENTS`]; "Theme" = the theme's own.
    pub accent: String,
    // ---- sidebar ----
    /// What the left sidebar lists: see [`SIDEBAR_LAYOUTS`].
    pub sidebar_layout: String,
    /// Connection groups folded in the "Connections tree" sidebar.
    pub sidebar_collapsed_groups: Vec<String>,
    // ---- filter bar defaults (its ☰ menu) ----
    /// Column picker order: "Table order" | "Alphabetical".
    pub filter_column_sort: String,
    /// New filter row's column: "First column" | "Primary key" | "Raw SQL".
    pub filter_default_column: String,
    /// New filter row's operator label (`=`, `Contains`, …).
    pub filter_default_operator: String,
    /// New filter rows start enabled.
    pub filter_default_enabled: bool,
    /// Grid order when no column is sorted: "Primary key ascending" |
    /// "Primary key descending" | "None".
    pub default_table_sort: String,
    // ---- AI panel ----
    /// The agent new chats start with (registry id or `custom:<name>`).
    pub agent: String,
    /// Automatically attach the open query text to assistant prompts.
    pub ai_include_active_query: bool,
    pub ai_http: std::collections::BTreeMap<String, crate::http_ai::Provider>,
    /// Agents added by hand: name → command, args, env.
    pub agent_servers: std::collections::BTreeMap<String, crate::acp_registry::CustomAgent>,
    /// Per agent, the session options picked last (model, mode, …), applied
    /// to new chats: agent id → option id → value.
    pub agent_config:
        std::collections::BTreeMap<String, std::collections::BTreeMap<String, serde_json::Value>>,
}

pub const FILTER_COLUMN_SORTS: [&str; 2] = ["Table order", "Alphabetical"];
pub const FILTER_DEFAULT_COLUMNS: [&str; 3] = ["First column", "Primary key", "Raw SQL"];
pub const TABLE_SORTS: [&str; 3] = ["Primary key ascending", "Primary key descending", "None"];
/// Sidebar layouts: the connected database's objects only, or every saved
/// connection (grouped) with the connected one's objects nested under it.
pub const SIDEBAR_LAYOUTS: [&str; 2] = ["Objects", "Connections tree"];

/// Accent choices (dark, light variant).
pub const ACCENTS: &[(&str, u32, u32)] = &[
    ("Teal", 0x2EB8A6, 0x0F8C7E),
    ("Blue", 0x4A90F0, 0x2F6FE0),
    ("Violet", 0x9B7CF4, 0x6D4FD8),
    // No orange / red / green: those mark edited / deleted / added rows.
    ("Magenta", 0xD66BD0, 0xA83AA0),
    ("Sky", 0x3FB6E8, 0x0B7CB0),
    ("Theme", 0, 0),
];

pub const UI_SIZE_RANGE: (f32, f32) = (10., 20.);
pub const TABLE_SIZE_RANGE: (f32, f32) = (9., 24.);

impl Default for Prefs {
    fn default() -> Self {
        Self {
            appearance: Appearance::initial(),
            light_theme: themes::DEFAULT_LIGHT.to_string(),
            dark_theme: themes::DEFAULT_DARK.to_string(),
            legacy_theme: None,
            ui_font_family: crate::theme::UI_FONT.to_string(),
            ui_font_size: 14.,
            table_font_family: crate::theme::MONO_FONT.to_string(),
            table_font_size: 13.,
            automatic_update_checks: true,
            group_database_types: false,
            reopen_last: true,
            query_timeout_secs: 0,
            gpu_device: String::new(),
            editor_line_numbers: true,
            editor_soft_wrap: false,
            editor_tab_size: 4,
            editor_uppercase_keywords: false,
            grid_stripes: true,
            null_text: "NULL".to_string(),
            confirm_destructive: true,
            confirm_save: false,
            accent: "Teal".to_string(),
            sidebar_layout: SIDEBAR_LAYOUTS[0].into(),
            sidebar_collapsed_groups: Vec::new(),
            filter_column_sort: FILTER_COLUMN_SORTS[0].into(),
            filter_default_column: FILTER_DEFAULT_COLUMNS[0].into(),
            filter_default_operator: "=".into(),
            filter_default_enabled: true,
            default_table_sort: TABLE_SORTS[0].into(),
            agent: String::new(),
            ai_include_active_query: true,
            ai_http: crate::http_ai::defaults(),
            agent_servers: Default::default(),
            agent_config: Default::default(),
        }
    }
}

impl Prefs {
    fn sanitized(mut self) -> Self {
        let d = Prefs::default();
        if let Some(old) = self.legacy_theme.take() {
            if themes::names(true).contains(&old.as_str()) {
                self.light_theme = old;
            } else {
                self.dark_theme = old;
            }
        }
        if !themes::names(true).contains(&self.light_theme.as_str()) {
            self.light_theme = d.light_theme;
        }
        if !themes::names(false).contains(&self.dark_theme.as_str()) {
            self.dark_theme = d.dark_theme;
        }
        if self.ui_font_family.trim().is_empty() {
            self.ui_font_family = d.ui_font_family;
        }
        if self.table_font_family.trim().is_empty() {
            self.table_font_family = d.table_font_family;
        }
        self.ui_font_size = self
            .ui_font_size
            .round()
            .clamp(UI_SIZE_RANGE.0, UI_SIZE_RANGE.1);
        if !ACCENTS.iter().any(|(n, _, _)| *n == self.accent) {
            self.accent = d.accent.clone();
        }
        if !SIDEBAR_LAYOUTS.contains(&self.sidebar_layout.as_str()) {
            self.sidebar_layout = d.sidebar_layout.clone();
        }
        if !FILTER_COLUMN_SORTS.contains(&self.filter_column_sort.as_str()) {
            self.filter_column_sort = d.filter_column_sort.clone();
        }
        if !FILTER_DEFAULT_COLUMNS.contains(&self.filter_default_column.as_str()) {
            self.filter_default_column = d.filter_default_column.clone();
        }
        if !TABLE_SORTS.contains(&self.default_table_sort.as_str()) {
            self.default_table_sort = d.default_table_sort.clone();
        }
        if !crate::filter::FilterOp::ALL
            .iter()
            .any(|op| op.label() == self.filter_default_operator)
        {
            self.filter_default_operator = d.filter_default_operator.clone();
        }
        self.editor_tab_size = self.editor_tab_size.clamp(1, 8);
        self.query_timeout_secs = self.query_timeout_secs.min(86_400);
        if self.gpu_device.len() != 4 || !self.gpu_device.bytes().all(|c| c.is_ascii_hexdigit()) {
            self.gpu_device.clear();
        }
        if self.null_text.trim().is_empty() {
            self.null_text = d.null_text.clone();
        }
        self.table_font_size = self
            .table_font_size
            .round()
            .clamp(TABLE_SIZE_RANGE.0, TABLE_SIZE_RANGE.1);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::{Prefs, SIDEBAR_LAYOUTS};

    #[test]
    fn legacy_preferences_keep_query_context_and_repair_invalid_grid_defaults() {
        let prefs: Prefs = serde_json::from_str(r#"{"filter_default_operator":"unknown","default_table_sort":"random","filter_column_sort":"invalid","filter_default_column":"invalid"}"#).unwrap();
        assert!(prefs.ai_include_active_query);
        let prefs = prefs.sanitized();
        let default = Prefs::default();
        assert_eq!(
            prefs.filter_default_operator,
            default.filter_default_operator
        );
        assert_eq!(prefs.default_table_sort, default.default_table_sort);
        assert_eq!(prefs.filter_column_sort, default.filter_column_sort);
        assert_eq!(prefs.filter_default_column, default.filter_default_column);
        let mut prefs = prefs;
        prefs.ai_include_active_query = false;
        let saved = serde_json::to_string(&prefs).unwrap();
        assert!(
            !serde_json::from_str::<Prefs>(&saved)
                .unwrap()
                .ai_include_active_query
        );
    }

    #[test]
    fn gpu_preference_accepts_only_pci_device_ids() {
        let mut prefs = Prefs {
            gpu_device: "2520".into(),
            ..Prefs::default()
        };
        assert_eq!(prefs.clone().sanitized().gpu_device, "2520");
        prefs.gpu_device = "not-a-device".into();
        assert!(prefs.sanitized().gpu_device.is_empty());
    }

    #[test]
    fn sidebar_layout_defaults_to_objects_and_rejects_unknown() {
        assert_eq!(Prefs::default().sidebar_layout, SIDEBAR_LAYOUTS[0]);
        let p: Prefs = serde_json::from_str(r#"{"sidebar_layout": "Sideways"}"#).unwrap();
        assert_eq!(p.sanitized().sidebar_layout, SIDEBAR_LAYOUTS[0]);
        let p: Prefs = serde_json::from_str(
            r#"{"sidebar_layout": "Connections tree", "sidebar_collapsed_groups": ["Acme"]}"#,
        )
        .unwrap();
        let p = p.sanitized();
        assert_eq!(p.sidebar_layout, SIDEBAR_LAYOUTS[1]);
        assert_eq!(p.sidebar_collapsed_groups, ["Acme"]);
    }
}

// Behind an `Arc`: render code reads settings per cell, a whole-struct clone
// each time would copy every string and map.
static PREFS: LazyLock<RwLock<std::sync::Arc<Prefs>>> =
    LazyLock::new(|| RwLock::new(std::sync::Arc::new(load())));
/// The two font families as ready `SharedString`s (UI, table).
static FONTS: LazyLock<RwLock<(SharedString, SharedString)>> = LazyLock::new(|| {
    let p = get();
    RwLock::new((
        p.ui_font_family.clone().into(),
        p.table_font_family.clone().into(),
    ))
});

/// Read one field without cloning the struct.
fn with<R>(f: impl FnOnce(&Prefs) -> R) -> R {
    match PREFS.read() {
        Ok(p) => f(&p),
        Err(_) => f(&Prefs::default()),
    }
}

fn path() -> std::path::PathBuf {
    crate::db::connections_path().with_file_name("settings.json")
}

fn load() -> Prefs {
    std::fs::read_to_string(path())
        .ok()
        .and_then(|t| serde_json::from_str::<Prefs>(&t).ok())
        .unwrap_or_default()
        .sanitized()
}

fn save(p: &Prefs) {
    let path = path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match serde_json::to_string_pretty(p) {
        Ok(text) => {
            if let Err(e) = std::fs::write(&path, text) {
                log::warn!("settings save failed: {e}");
            }
        }
        Err(e) => log::warn!("settings serialize failed: {e}"),
    }
}

pub fn get() -> std::sync::Arc<Prefs> {
    PREFS.read().map(|p| p.clone()).unwrap_or_default()
}

#[cfg(target_os = "linux")]
pub fn apply_gpu_preference() {
    let device = &get().gpu_device;
    if !device.is_empty() {
        // Called before GPUI starts and before any threads can read the environment.
        unsafe { std::env::set_var("ZED_DEVICE_ID", device) };
    }
}

#[cfg(target_os = "linux")]
fn gpu_options() -> Vec<(SharedString, SharedString)> {
    let mut options = vec![("".into(), "Automatic".into())];
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
        return options;
    };
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with("renderD") {
            continue;
        }
        let Ok(id) = std::fs::read_to_string(entry.path().join("device/device")) else {
            continue;
        };
        let id = id.trim().trim_start_matches("0x");
        if id.len() != 4 || !id.bytes().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        let vendor =
            std::fs::read_to_string(entry.path().join("device/vendor")).unwrap_or_default();
        let vendor = match vendor.trim() {
            "0x8086" => "Intel",
            "0x10de" => "NVIDIA",
            "0x1002" => "AMD",
            _ => "GPU",
        };
        let id = id.to_ascii_lowercase();
        if options.iter().any(|(value, _)| value.as_ref() == id) {
            continue;
        }
        options.push((id.clone().into(), format!("{vendor} GPU (0x{id})").into()));
    }
    options[1..].sort_by(|a, b| a.1.cmp(&b.1));
    options
}

/// Change a setting: save, re-apply the theme and redraw every window.
pub fn update(cx: &mut App, f: impl FnOnce(&mut Prefs)) {
    let mut p = (*get()).clone();
    f(&mut p);
    let p = p.sanitized();
    if p == *get() {
        return;
    }
    save(&p);
    if let Ok(mut fonts) = FONTS.write() {
        *fonts = (
            p.ui_font_family.clone().into(),
            p.table_font_family.clone().into(),
        );
    }
    if let Ok(mut w) = PREFS.write() {
        *w = std::sync::Arc::new(p);
    }
    crate::theme::apply(cx);
    cx.refresh_windows();
}

/// Is the light theme in effect right now (setting + macOS appearance)?
pub fn is_light(cx: &App) -> bool {
    let system = || {
        matches!(
            cx.window_appearance(),
            WindowAppearance::Light | WindowAppearance::VibrantLight
        )
    };
    match get().appearance {
        Appearance::Light => true,
        Appearance::Dark => false,
        Appearance::System => system(),
        // Without a readable Omarchy theme, behave like "System".
        Appearance::Omarchy => crate::omarchy::palette().map_or_else(system, |p| p.light),
    }
}

/// The theme in effect right now.
pub fn active_theme(cx: &App) -> String {
    let p = get();
    if p.appearance == Appearance::Omarchy && crate::omarchy::palette().is_some() {
        crate::omarchy::NAME.to_string()
    } else if is_light(cx) {
        p.light_theme.clone()
    } else {
        p.dark_theme.clone()
    }
}

/// Pick a theme from the palette: it goes into its own mode's slot, and the
/// appearance switches to that mode if it isn't showing already — so the
/// choice is always visible.
pub fn choose_theme(cx: &mut App, name: &str) {
    let light = themes::names(true).contains(&name);
    let showing_light = is_light(cx);
    update(cx, |p| {
        if light {
            p.light_theme = name.to_string();
        } else {
            p.dark_theme = name.to_string();
        }
        if light != showing_light || p.appearance == Appearance::Omarchy {
            p.appearance = if light {
                Appearance::Light
            } else {
                Appearance::Dark
            };
        }
    });
}

pub fn ui_font() -> SharedString {
    FONTS.read().map(|f| f.0.clone()).unwrap_or_default()
}

/// Row text in the chrome (sidebar rows, tab labels).
pub fn ui_text() -> f32 {
    // Editor label scale: 0.875 of the UI size (14 → 12.25).
    with(|p| p.ui_font_size) * 0.875
}

/// Compact density: one formula for every list row
/// and bar outside the data grid, scaling with the UI font size.
/// At the default 13px row text: row 22, bar 26, tab strip 30.
pub fn row_h() -> f32 {
    // Heights keep the compact density they were tuned for.
    ((with(|p| p.ui_font_size) - 1.) * 1.7).round()
}

/// Toolbars / status bars / segmented controls around lists.
pub fn bar_h() -> f32 {
    row_h() + 4.
}

pub fn tab_h() -> f32 {
    row_h() + 8.
}

pub fn table_font() -> SharedString {
    FONTS.read().map(|f| f.1.clone()).unwrap_or_default()
}

/// A shared text line box keeps grid display and in-cell editors aligned.
pub fn table_line_height() -> f32 {
    (table_text() * 1.5).ceil()
}
pub fn table_row_height() -> f32 {
    (table_line_height() + 8.).max(32.)
}

pub fn table_text() -> f32 {
    with(|p| p.table_font_size)
}

/// The sidebar lists every saved connection (grouped), not just the
/// connected database's objects.
pub fn connections_tree() -> bool {
    with(|p| p.sidebar_layout == SIDEBAR_LAYOUTS[1])
}

/// Is this connection group folded in the connections-tree sidebar?
pub fn group_collapsed(name: &str) -> bool {
    with(|p| p.sidebar_collapsed_groups.iter().any(|g| g == name))
}

/// Fold / unfold a connection group in the connections-tree sidebar.
pub fn toggle_group_collapsed(cx: &mut App, name: &str) {
    update(cx, |p| {
        if let Some(i) = p.sidebar_collapsed_groups.iter().position(|g| g == name) {
            p.sidebar_collapsed_groups.remove(i);
        } else {
            p.sidebar_collapsed_groups.push(name.to_string());
        }
    });
}

// ---------------------------------------------------------------------------
// Settings window
// ---------------------------------------------------------------------------

#[derive(Default)]
struct OpenSettings(Option<AnyWindowHandle>);
impl Global for OpenSettings {}

type FontSelect = Entity<SelectState<SearchableVec<SharedString>>>;

pub struct SettingsWindow {
    focus: FocusHandle,
    /// Searchable pickers: a system has hundreds of families, too many for the
    /// kit's (non-virtualized) dropdown menu.
    ui_font: FontSelect,
    table_font: FontSelect,
    _subs: Vec<Subscription>,
}

impl SettingsWindow {
    /// Open (or focus) the settings window.
    pub fn open(cx: &mut App) {
        if let Some(handle) = cx.default_global::<OpenSettings>().0
            && cx
                .update_window(handle, |_, window, _| window.activate_window())
                .is_ok()
        {
            return;
        }
        let bounds = Bounds::centered(None, size(px(1040.), px(760.)), cx);
        let result = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(760.), px(480.))),
                focus: !crate::background(),
                kind: crate::theme::secondary_window_kind(),
                ..TitleBar::window_options()
            },
            |window, cx| {
                let view = cx.new(|cx| SettingsWindow::new(window, cx));
                view.read(cx).focus.clone().focus(window, cx);
                cx.new(|cx| Root::new(view, window, cx))
            },
        );
        match result {
            Ok(handle) => cx.set_global(OpenSettings(Some(handle.into()))),
            Err(e) => log::warn!("settings window failed to open: {e}"),
        }
    }

    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        crate::tool_status::refresh(cx);
        let fonts = font_options(cx);
        let p = get();
        let mut picker = |current: &str, set: fn(&mut Prefs, String)| {
            let ix = fonts.iter().position(|f| f.as_ref() == current);
            let state = cx.new(|cx| {
                SelectState::new(
                    SearchableVec::new(fonts.clone()),
                    ix.map(IndexPath::new),
                    window,
                    cx,
                )
                .searchable(true)
            });
            let sub = cx.subscribe(
                &state,
                move |_, _, ev: &SelectEvent<SearchableVec<SharedString>>, cx| {
                    let SelectEvent::Confirm(Some(v)) = ev else {
                        return;
                    };
                    let v = v.to_string();
                    update(cx, |p| set(p, v));
                },
            );
            (state, sub)
        };
        let (ui_font, s1) = picker(&p.ui_font_family, |p, v| p.ui_font_family = v);
        let (table_font, s2) = picker(&p.table_font_family, |p, v| p.table_font_family = v);
        Self {
            focus: cx.focus_handle(),
            ui_font,
            table_font,
            _subs: vec![s1, s2],
        }
    }

    /// A font picker row: the Select plus a reset to the default family.
    fn font_field(state: &FontSelect, is_ui: bool) -> SettingField<SharedString> {
        let family = move || {
            let p = get();
            if is_ui {
                p.ui_font_family.clone()
            } else {
                p.table_font_family.clone()
            }
        };
        let default = if is_ui {
            Prefs::default().ui_font_family
        } else {
            Prefs::default().table_font_family
        };
        let render_state = state.clone();
        let reset_state = state.clone();
        let dirty_default = default.clone();
        SettingField::render(move |_, _, _| Select::new(&render_state).w(px(260.))).on_reset(
            move |_| family() != dirty_default,
            move |window, cx| {
                let v = default.clone();
                update(cx, |p| {
                    if is_ui {
                        p.ui_font_family = v.clone();
                    } else {
                        p.table_font_family = v.clone();
                    }
                });
                reset_state.update(cx, |s, cx| {
                    s.set_selected_value(&SharedString::from(v), window, cx)
                });
            },
        )
    }

    fn font_preview(ui: bool) -> SettingItem {
        SettingItem::new(
            "Live Preview",
            SettingField::render(move |_, _, _| {
                let p = get();
                let (family, size) = if ui {
                    (p.ui_font_family.clone(), p.ui_font_size)
                } else {
                    (p.table_font_family.clone(), p.table_font_size)
                };
                v_flex()
                    .w_full()
                    .min_w_0()
                    .font_family(family)
                    .text_size(px(size))
                    .line_height(px((size * 1.6).ceil()))
                    .child(div().child("SELECT café, price"))
                    .child(div().child("FROM orders WHERE id = 123;"))
                    .child(div().child("0O 1Il {} [] !? +-*/ 123.45"))
                    .child(div().child("Français — العربية 日本語"))
            }),
        )
        .layout(Axis::Vertical)
        .description("Updates as you change the font family and size.")
    }

    fn pages(&self) -> Vec<SettingPage> {
        let d = Prefs::default();
        let options = |light: bool| -> Vec<(SharedString, SharedString)> {
            themes::names(light)
                .into_iter()
                .map(|n| (SharedString::from(n), SharedString::from(n)))
                .collect()
        };
        let modes: Vec<(SharedString, SharedString)> = Appearance::available()
            .into_iter()
            .map(|a| (SharedString::from(a.label()), SharedString::from(a.label())))
            .collect();
        let size_opts = |(min, max): (f32, f32)| NumberFieldOptions {
            min: min as f64,
            max: max as f64,
            step: 1.,
        };

        let overview = SettingPage::new("Overview")
            .icon(IconName::Info)
            .description("Your current preferences at a glance. Changes are saved automatically; search to find a parameter.")
            .resettable(false)
            .group(SettingGroup::new().title("Current Configuration").item(
                SettingItem::render(|_, _, cx| {
                    let p = get();
                    let rows = [
                        ("Appearance", format!("{} · {}", p.appearance.label(), active_theme(cx))),
                        ("Interface", format!("{} · {} px · {}", p.ui_font_family, p.ui_font_size, p.sidebar_layout)),
                        ("Query and grid font", format!("{} · {} px", p.table_font_family, p.table_font_size)),
                        ("Query defaults", format!("{} spaces per indent · {}", p.editor_tab_size, if p.query_timeout_secs == 0 { "No timeout".into() } else { format!("{} second timeout", p.query_timeout_secs) })),
                        ("Database selection", if p.group_database_types { "Grouped by database type" } else { "All engines together" }.into()),
                        ("AI provider", if p.agent.is_empty() { "Choose a provider in the assistant".into() } else { p.agent.clone() }),
                        ("Write confirmations", format!("Dangerous queries: {} · All writes: {}", if p.confirm_destructive { "On" } else { "Off" }, if p.confirm_save { "On" } else { "Off" })),
                    ];
                    v_flex().w_full().gap_4().children(rows.into_iter().map(|(label, value)| {
                        v_flex().gap_1().child(div().font_weight(FontWeight::MEDIUM).child(label))
                            .child(div().text_sm().text_color(cx.theme().muted_foreground).child(value))
                    }))
                }).keywords(["summary", "configuration", "fonts", "provider", "timeout"])
            ));
        let appearance = SettingPage::new("Appearance")
            .icon(IconName::Palette)
            .description("Choose the desktop appearance and colors used throughout Tusk.")
            .group(
            SettingGroup::new()
                .title("Theme")
                .item(
                    SettingItem::new(
                        "Mode",
                        SettingField::dropdown(
                            modes,
                            |_| get().appearance.label().into(),
                            |v: SharedString, cx| {
                                update(cx, |p| p.appearance = Appearance::from_label(&v))
                            },
                        )
                        .default_value(SharedString::from(d.appearance.label())),
                    )
                    .description("System follows the desktop light / dark appearance."),
                )
                .item(
                    SettingItem::new(
                        "Accent Color",
                        SettingField::dropdown(
                            ACCENTS
                                .iter()
                                .map(|(n, _, _)| (SharedString::from(*n), SharedString::from(*n)))
                                .collect(),
                            |_| get().accent.clone().into(),
                            |v: SharedString, cx| update(cx, |p| p.accent = v.to_string()),
                        )
                        .default_value(SharedString::from(d.accent.clone())),
                    )
                    .description(
                        "Buttons, focus, caret, selection frames. \"Theme\" keeps the theme's own.",
                    ),
                )
                .item(
                    SettingItem::new(
                        "Light Theme",
                        SettingField::scrollable_dropdown(
                            options(true),
                            |_| get().light_theme.clone().into(),
                            |v: SharedString, cx| update(cx, |p| p.light_theme = v.to_string()),
                        )
                        .default_value(SharedString::from(d.light_theme.clone())),
                    )
                    .description("Used in light mode."),
                )
                .item(
                    SettingItem::new(
                        "Dark Theme",
                        SettingField::scrollable_dropdown(
                            options(false),
                            |_| get().dark_theme.clone().into(),
                            |v: SharedString, cx| update(cx, |p| p.dark_theme = v.to_string()),
                        )
                        .default_value(SharedString::from(d.dark_theme.clone())),
                    )
                    .description("Used in dark mode."),
                ),
        );
        let interface = SettingPage::new("Interface")
            .icon(IconName::PanelLeft)
            .description(
                "Adjust interface typography and navigation. Font changes apply immediately.",
            )
            .group(
                SettingGroup::new()
                    .title("UI Font")
                    .item(
                        SettingItem::new("Font Family", Self::font_field(&self.ui_font, true))
                            .description("Sidebar, tabs, title bar, menus and dialogs."),
                    )
                    .item(
                        SettingItem::new(
                            "Font Size",
                            SettingField::number_input(
                                size_opts(UI_SIZE_RANGE),
                                |_| get().ui_font_size as f64,
                                |v, cx| update(cx, |p| p.ui_font_size = v as f32),
                            )
                            .default_value(d.ui_font_size as f64),
                        )
                        .description("Scales the whole interface."),
                    )
                    .item(Self::font_preview(true)),
            )
            .group(
                SettingGroup::new().title("Layout").item(
                    SettingItem::new(
                        "Sidebar Layout",
                        SettingField::dropdown(
                            SIDEBAR_LAYOUTS
                                .iter()
                                .map(|n| (SharedString::from(*n), SharedString::from(*n)))
                                .collect(),
                            |_| get().sidebar_layout.clone().into(),
                            |v: SharedString, cx| update(cx, |p| p.sidebar_layout = v.to_string()),
                        )
                        .default_value(SharedString::from(d.sidebar_layout.clone())),
                    )
                    .description(
                        "Objects lists the connected database only. Connections tree lists \
                         every saved connection by group, with the connected one's objects \
                         nested under it.",
                    ),
                ),
            );
        let data = SettingPage::new("Data Grid")
            .icon(IconName::Table2)
            .description("Configure grid display and the shared font used by query editors.")
            .group(
                SettingGroup::new()
                    .title("Table Font")
                    .item(
                        SettingItem::new("Font Family", Self::font_field(&self.table_font, false))
                            .description("Data grid, structure view and the SQL editor."),
                    )
                    .item(
                        SettingItem::new(
                            "Font Size",
                            SettingField::number_input(
                                size_opts(TABLE_SIZE_RANGE),
                                |_| get().table_font_size as f64,
                                |v, cx| update(cx, |p| p.table_font_size = v as f32),
                            )
                            .default_value(d.table_font_size as f64),
                        )
                        .description("Column widths scale with it."),
                    )
                    .item(Self::font_preview(false)),
            );
        let switch = |title: &'static str,
                      desc: &'static str,
                      get_v: fn(&Prefs) -> bool,
                      set_v: fn(&mut Prefs, bool),
                      default: bool| {
            SettingItem::new(
                title,
                SettingField::switch(
                    move |_| get_v(&get()),
                    move |v, cx| update(cx, |p| set_v(p, v)),
                )
                .default_value(default),
            )
            .description(desc)
        };
        let general_group = SettingGroup::new()
            .title("Startup & Connections")
            .item(switch(
                "Automatic Update Checks",
                "Check GitHub daily on Windows and Linux. Updates are installed manually.",
                |p| p.automatic_update_checks,
                |p, v| p.automatic_update_checks = v,
                d.automatic_update_checks,
            ))
            .item(switch(
                "Group Database Types",
                "Separate SQL, analytical, and NoSQL engines in the new-connection menu.",
                |p| p.group_database_types,
                |p, v| p.group_database_types = v,
                d.group_database_types,
            ))
            .item(switch(
                "Reopen Last Connection",
                "Connect to the last used database when the app starts.",
                |p| p.reopen_last,
                |p, v| p.reopen_last = v,
                d.reopen_last,
            ))
            .item(
                SettingItem::new(
                    "Query Timeout",
                    SettingField::number_input(
                        NumberFieldOptions {
                            min: 0.,
                            max: 86_400.,
                            step: 30.,
                        },
                        |_| get().query_timeout_secs as f64,
                        |v, cx| update(cx, |p| p.query_timeout_secs = v as u32),
                    )
                    .default_value(d.query_timeout_secs as f64),
                )
                .description(
                    "Seconds before a statement is cancelled (0 = never). New connections.",
                ),
            );
        #[cfg(target_os = "linux")]
        let general_group = general_group.item(
            SettingItem::new(
                "Graphics Device",
                SettingField::dropdown(
                    gpu_options(),
                    |_| get().gpu_device.clone().into(),
                    |v: SharedString, cx| update(cx, |p| p.gpu_device = v.to_string()),
                )
                .default_value(SharedString::default()),
            )
            .description("GPU used for rendering. Restart Tusk to apply."),
        );
        let general = SettingPage::new("General")
            .icon(IconName::Settings2)
            .group(general_group)
            .description("Set startup behavior, connection defaults, and discover installed tools.")
            .group(SettingGroup::new().title("Tools").description("SQL assistance uses postgres-language-server or sqls. Backups require PostgreSQL client tools. Override discovery with TUSK_PGLS, TUSK_SQLS, or TUSK_PG_BIN.").item(
                SettingItem::render(|_, _, cx| {
                    use gpui_kit::component::button::Button;
                    let state = cx.default_global::<crate::tool_status::Tools>().clone();
                    v_flex().gap_4().w_full().min_w_0()
                        .children(state.rows.into_iter().map(|(name, status)| {
                            v_flex().w_full().min_w_0().gap_1()
                                .child(div().font_weight(FontWeight::MEDIUM).child(name.clone()))
                                .child(div().id(SharedString::from(format!("tool-status-{name}")))
                                    .w_full().overflow_x_scroll().whitespace_nowrap().text_sm().child(status))
                        }))
                        .child(Button::new("refresh-tools")
                            .label(if state.scanning { "Scanning…" } else { "Refresh Tools" })
                            .disabled(state.scanning)
                            .on_click(|_, _, cx| crate::tool_status::refresh(cx)))
                }).keywords(["installed tools", "paths", "postgres", "backup", "restore", "sqls", "language server"])
            ));
        let editor = SettingPage::new("SQL Editor")
            .icon(IconName::SquareTerminal)
            .description("Control query editing, indentation, and keyword completions.")
            .group(
                SettingGroup::new()
                    .title("Editor")
                    .item(switch(
                        "Line Numbers",
                        "Show the gutter with line numbers.",
                        |p| p.editor_line_numbers,
                        |p, v| p.editor_line_numbers = v,
                        d.editor_line_numbers,
                    ))
                    .item(switch(
                        "Soft Wrap",
                        "Wrap long lines instead of scrolling sideways.",
                        |p| p.editor_soft_wrap,
                        |p, v| p.editor_soft_wrap = v,
                        d.editor_soft_wrap,
                    ))
                    .item(
                        SettingItem::new(
                            "Tab Size",
                            SettingField::number_input(
                                NumberFieldOptions {
                                    min: 1.,
                                    max: 8.,
                                    step: 1.,
                                },
                                |_| get().editor_tab_size as f64,
                                |v, cx| update(cx, |p| p.editor_tab_size = v as u32),
                            )
                            .default_value(d.editor_tab_size as f64),
                        )
                        .description("Spaces per indent level."),
                    )
                    .item(switch(
                        "Uppercase Keywords",
                        "Completions insert SQL keywords in UPPERCASE.",
                        |p| p.editor_uppercase_keywords,
                        |p, v| p.editor_uppercase_keywords = v,
                        d.editor_uppercase_keywords,
                    )),
            );
        let data = data.group(
            SettingGroup::new()
                .title("Display")
                .item(switch(
                    "Alternating Row Colors",
                    "Stripe every other row.",
                    |p| p.grid_stripes,
                    |p, v| p.grid_stripes = v,
                    d.grid_stripes,
                ))
                .item(
                    SettingItem::new(
                        "NULL Display",
                        SettingField::input(
                            |_| get().null_text.clone().into(),
                            |v: SharedString, cx| update(cx, |p| p.null_text = v.to_string()),
                        )
                        .default_value(SharedString::from(d.null_text.clone())),
                    )
                    .description("How NULL values are shown in grids."),
                ),
        );
        let choices = |values: &[&str]| -> Vec<(SharedString, SharedString)> {
            values
                .iter()
                .map(|v| (SharedString::from(*v), SharedString::from(*v)))
                .collect()
        };
        let data = data.group(SettingGroup::new().title("Ordering").item(
            SettingItem::new("Default Table Sort", SettingField::dropdown(
                choices(&TABLE_SORTS), |_| get().default_table_sort.clone().into(),
                |v: SharedString, cx| update(cx, |p| p.default_table_sort = v.to_string()),
            ).default_value(SharedString::from(d.default_table_sort.clone())))
            .description("Used when opening a grid without an explicit column sort. Requires a primary key.")
        ));
        let filters = SettingPage::new("Filters").icon(IconName::Settings2)
            .description("Defaults for new grid filters. Available operators depend on the database engine.")
            .group(SettingGroup::new().title("New Filter Defaults")
                .item(SettingItem::new("Column Picker Order", SettingField::dropdown(
                    choices(&FILTER_COLUMN_SORTS), |_| get().filter_column_sort.clone().into(),
                    |v: SharedString, cx| update(cx, |p| p.filter_column_sort = v.to_string()),
                ).default_value(SharedString::from(d.filter_column_sort.clone())))
                    .description("Keep columns in table order or list them alphabetically."))
                .item(SettingItem::new("Default Column", SettingField::dropdown(
                    choices(&FILTER_DEFAULT_COLUMNS), |_| get().filter_default_column.clone().into(),
                    |v: SharedString, cx| update(cx, |p| p.filter_default_column = v.to_string()),
                ).default_value(SharedString::from(d.filter_default_column.clone())))
                    .description("Choose the first column, the primary key, or a raw SQL expression."))
                .item(SettingItem::new("Default Operator", SettingField::dropdown(
                    crate::filter::FilterOp::ALL.iter().map(|op| (SharedString::from(op.label()), SharedString::from(op.label()))).collect(),
                    |_| get().filter_default_operator.clone().into(),
                    |v: SharedString, cx| update(cx, |p| p.filter_default_operator = v.to_string()),
                ).default_value(SharedString::from(d.filter_default_operator.clone())))
                    .description("Initial comparison for each new filter row."))
                .item(switch("Enable New Filters", "Apply new filter rows immediately; turn off to prepare them before enabling.",
                    |p| p.filter_default_enabled, |p, v| p.filter_default_enabled = v, d.filter_default_enabled))
            );
        let ai = SettingPage::new("AI Providers").icon(IconName::Sparkles)
            .description("Connect local model servers or ACP agents. Select a provider and model in the assistant.")
            .group(SettingGroup::new().title("Prompt Context").item(switch(
                "Attach Open Query Automatically",
                "Include the open query text with assistant prompts. Agents can still request it through tools. Result rows and credentials are never attached automatically.",
                |p| p.ai_include_active_query, |p, v| p.ai_include_active_query = v, d.ai_include_active_query,
            )))
            .group(SettingGroup::new().title("Agents and Model Servers")
                .description("Claude, Codex, Gemini, and registry agents are available in the assistant. OpenCode uses its native opencode acp command. Configure model discovery, connection tests, credentials, and custom commands below.")
                .item(SettingItem::render(|_, _, cx| {
                    use gpui_kit::component::button::Button;
                    let prefs = get();
                    v_flex().gap_4().w_full().min_w_0()
                        .children(prefs.ai_http.iter().map(|(id, p)| {
                            let id=id.clone();
                            v_flex().gap_1().w_full().min_w_0()
                                .child(div().font_weight(FontWeight::MEDIUM).child(p.name.clone()))
                                .child(div().text_sm().text_color(cx.theme().muted_foreground).child(format!("{} · Model: {}", p.base_url, if p.model.is_empty() { "Not selected" } else { &p.model })))
                                .child(Button::new(SharedString::from(format!("configure-{id}"))).label("Configure / Test Connection").on_click(move|_,_,cx|crate::ai_settings::ProviderEditor::open(id.clone(),false,cx)))
                        }))
                        .children(prefs.agent_servers.iter().map(|(id, p)| {
                            let id=id.clone();
                            v_flex().gap_1().w_full().min_w_0()
                                .child(div().font_weight(FontWeight::MEDIUM).child(p.name.clone()))
                                .child(div().text_sm().text_color(cx.theme().muted_foreground).child(format!("ACP executable: {}", p.command)))
                                .child(Button::new(SharedString::from(format!("configure-acp-{id}"))).label("Configure Agent").on_click(move|_,_,cx|crate::ai_settings::ProviderEditor::open(id.clone(),true,cx)))
                        }))
                        .child(Button::new("add-http-provider").label("Add Custom Model Endpoint").on_click(|_,_,cx| {
                            let id=format!("custom-{}", chrono::Utc::now().timestamp_millis());
                            crate::ai_settings::ProviderEditor::open(id,false,cx);
                        }))
                        .child(Button::new("add-acp-provider").label("Add Custom ACP Agent").on_click(|_,_,cx| {
                            let id=format!("custom-{}", chrono::Utc::now().timestamp_millis());
                            crate::ai_settings::ProviderEditor::open(id,true,cx);
                        }))
                }).keywords(["providers", "ollama", "lm studio", "claude", "codex", "opencode", "endpoint", "model", "custom agent"]))
            );
        let safety = SettingPage::new("Safe Mode")
            .icon(IconName::ShieldCheck)
            .group(
            SettingGroup::new()
                .title("Confirmations")
                .item(switch(
                    "Confirm Dangerous Queries",
                    "Ask before DROP, TRUNCATE, ALTER, or UPDATE / DELETE without WHERE.",
                    |p| p.confirm_destructive,
                    |p, v| p.confirm_destructive = v,
                    d.confirm_destructive,
                ))
                .item(switch(
                    "Confirm Before Saving",
                    "Ask before saving pending changes, or before a query writes to the database.",
                    |p| p.confirm_save,
                    |p, v| p.confirm_save = v,
                    d.confirm_save,
                )),
        );
        vec![
            overview, general, appearance, interface, editor, data, filters, ai, safety,
        ]
    }
}

/// Installed font families for the pickers (Pravka, the embedded default,
/// first). Hidden system families (`.SF…`) are skipped.
fn font_options(cx: &App) -> Vec<SharedString> {
    let mut names: Vec<String> = cx
        .text_system()
        .all_font_names()
        .into_iter()
        .filter(|n| !n.starts_with('.') && !n.is_empty())
        .collect();
    names.sort_by_key(|n| n.to_lowercase());
    names.dedup();
    let default = crate::theme::UI_FONT.to_string();
    names.retain(|n| *n != default);
    std::iter::once(default)
        .chain(names)
        .map(SharedString::from)
        .collect()
}

impl Render for SettingsWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        div()
            .track_focus(&self.focus)
            .key_context(crate::dialog_keys::CONTEXT)
            .on_action(crate::dialog_keys::close)
            .size_full()
            .flex()
            .flex_col()
            .bg(t.background)
            .text_color(t.foreground)
            .font_family(ui_font())
            .child(
                TitleBar::new()
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .justify_center()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .child("Settings"),
                    )
                    .child(div().w(px(60.))),
            )
            .child(
                div().flex_1().min_h_0().child(
                    Settings::new("tusk-settings")
                        .sidebar_width(px(220.))
                        .pages(self.pages()),
                ),
            )
    }
}
