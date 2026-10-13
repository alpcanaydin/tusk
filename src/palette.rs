//! Command palette (cmd+shift+p) + table quick-open (cmd+p).
//!
//! Built on gpui-kit's `Command` component (verified:
//! component-0.6.6/src/command/{command,item,state}.rs). The palette dispatches
//! nothing by itself here — items carry no `Action`; `on_confirm` maps the
//! confirmed row back to a `RunFn` through the [`TuskHandle`] global, because
//! the callback only receives `&mut App`.

use gpui_kit::assets::IconName;
use gpui_kit::component::Icon;
use gpui_kit::component::command::{Command, CommandItem, CommandState};
use gpui_kit::component::theme::ActiveTheme as _;
use gpui_kit::*;

use crate::actions::*;
use crate::app::{TableRef, TuskApp, TuskHandle};
use crate::tr;

#[derive(Clone, Copy, PartialEq)]
pub enum PaletteMode {
    Commands,
    Tables,
    /// ⌘K: open another database of the connection.
    Databases,
    /// ⇧⌘K: open another saved connection.
    Connections,
    /// ⇧⌘\: jump to an open tab.
    Tabs,
}

/// What a confirmed palette row runs. Rows that carry an `Action` get a
/// no-op here (the kit dispatches the action itself and shows its binding).
pub type RunFn = std::rc::Rc<dyn Fn(&mut TuskApp, &mut Window, &mut Context<TuskApp>)>;

/// Snapshot of app state the palette needs to build dynamic rows.
/// A palette row's action before it is shared (see [`RunFn`]).
type BoxedRun = Box<dyn Fn(&mut TuskApp, &mut Window, &mut Context<TuskApp>)>;

pub struct PaletteContext {
    pub connected: bool,
    pub saved: Vec<String>,
    pub active_connection: String,
    pub databases: Vec<String>,
    pub current_database: String,
    pub schemas: Vec<String>,
    pub current_schema: String,
    /// The table / view the object actions apply to: the active tab's, else
    /// the sidebar selection.
    pub focus_object: Option<(crate::app::TableKind, String)>,
    pub sql_tab_active: bool,
}

pub struct PaletteOverlay {
    pub mode: PaletteMode,
    pub state: Entity<CommandState>,
    pub table_rows: Vec<TableRef>,
    /// Databases / Connections modes: the names listed, in order.
    pub names: Vec<String>,
    /// The rows, built once per open / mode (not on every frame).
    pub items: std::cell::RefCell<Option<(PaletteMode, Vec<CommandItem>)>>,
}

impl PaletteOverlay {
    pub fn open(mode: PaletteMode, window: &mut Window, cx: &mut Context<TuskApp>) -> Self {
        let state = cx.new(|cx| CommandState::new(window, cx));
        let handle = state.read(cx).focus_handle(cx);
        handle.focus(window, cx);
        Self {
            mode,
            state,
            table_rows: Vec::new(),
            items: Default::default(),
            names: Vec::new(),
        }
    }
}

/// Command-palette rows. Static commands
/// carry their `Action`, so the kit renders the live keybinding next to
/// them; dynamic rows (switch database / schema / connection) run closures.
/// Returns (items, runs) in the same order.
pub fn command_rows(app: &TuskApp) -> (Vec<CommandItem>, Vec<RunFn>) {
    let ctx = app.palette_context();
    let mut items = Vec::new();
    let mut runs: Vec<RunFn> = Vec::new();
    let noop: RunFn = std::rc::Rc::new(|_, _, _| {});
    let mut action = |label: String, icon: IconName, action: Box<dyn Action>, keywords: &[&str]| {
        items.push(
            CommandItem::new()
                .label(label)
                .keywords(keywords.iter().map(|k| k.to_string()))
                .icon(Icon::new(icon))
                .action(action),
        );
        runs.push(noop.clone());
    };

    // ---- connection ----
    action(
        tr!("palette.connection.new"),
        IconName::Plus,
        Box::new(NewConnection),
        &["add", "create"],
    );
    action(
        tr!("palette.connection.open"),
        IconName::FolderOpen,
        Box::new(OpenConnections),
        &["manager", "groups", "saved"],
    );
    action(
        tr!("palette.connection.import_compose"),
        IconName::ArrowDownToLine,
        Box::new(ImportDockerCompose),
        &["docker", "compose", "yaml", "container"],
    );
    if ctx.connected {
        action(
            tr!("palette.connection.disconnect"),
            IconName::LogOut,
            Box::new(Disconnect),
            &["close"],
        );
    } else {
        action(
            tr!("palette.connection.quick_connect"),
            IconName::Zap,
            Box::new(QuickConnect),
            &["docker"],
        );
    }
    // Backup / Restore work from anywhere (the window picks the database).
    action(
        tr!("palette.database.backup"),
        IconName::HardDriveDownload,
        Box::new(BackupDatabase),
        &["dump", "pg_dump", "export"],
    );
    action(
        tr!("palette.database.restore"),
        IconName::HardDriveUpload,
        Box::new(RestoreDatabase),
        &["pg_restore", "import", "load"],
    );
    let mut dynamic: Vec<(String, IconName, RunFn)> = Vec::new();
    if crate::migrate::available() {
        dynamic.push((
            tr!("palette.connection.migrate"),
            IconName::ArrowDownToLine,
            std::rc::Rc::new(
                |this: &mut TuskApp, w: &mut Window, cx: &mut Context<TuskApp>| {
                    this.migrate_from_tableplus(w, cx)
                },
            ),
        ));
    }
    for (ix, name) in ctx.saved.iter().enumerate() {
        if ctx.connected && *name == ctx.active_connection {
            continue;
        }
        dynamic.push((
            tr!("palette.connection.connect_to", name = name),
            IconName::Plug,
            std::rc::Rc::new(
                move |this: &mut TuskApp, w: &mut Window, cx: &mut Context<TuskApp>| {
                    this.palette_switch_connection(ix, w, cx)
                },
            ),
        ));
    }

    if ctx.connected {
        // ---- query / tabs ----
        action(
            tr!("palette.query.new_sql"),
            IconName::SquareTerminal,
            Box::new(NewSqlTab),
            &["editor", "sql"],
        );
        action(
            tr!("palette.query.run_current"),
            IconName::Play,
            Box::new(RunCurrent),
            &["execute", "statement"],
        );
        action(
            tr!("palette.query.run_all"),
            IconName::ListVideo,
            Box::new(RunAll),
            &["execute", "script"],
        );
        action(
            tr!("palette.table.open"),
            IconName::Table,
            Box::new(QuickOpenTables),
            &["quick", "find", "go"],
        );
        action(
            tr!("palette.tab.close"),
            IconName::X,
            Box::new(CloseTab),
            &[],
        );
        action(
            tr!("palette.tab.refresh"),
            IconName::RefreshCw,
            Box::new(RefreshActive),
            &["reload"],
        );
        // ---- edits ----
        action(
            tr!("palette.changes.save"),
            IconName::Save,
            Box::new(SaveChanges),
            &["commit", "write"],
        );
        action(
            tr!("palette.changes.delete_selection"),
            IconName::Delete,
            Box::new(DeleteSelection),
            &["remove", "drop"],
        );
        action(
            tr!("palette.editor.line_comment"),
            IconName::Hash,
            Box::new(ToggleLineComment),
            &["comment"],
        );
        action(
            tr!("palette.editor.block_comment"),
            IconName::Hash,
            Box::new(ToggleBlockComment),
            &["comment"],
        );
        action(
            tr!("palette.editor.upper"),
            IconName::CaseUpper,
            Box::new(MakeUpperCase),
            &["transform"],
        );
        action(
            tr!("palette.editor.lower"),
            IconName::CaseLower,
            Box::new(MakeLowerCase),
            &["transform"],
        );
        action(
            tr!("palette.editor.capitalize"),
            IconName::CaseSensitive,
            Box::new(Capitalize),
            &["transform"],
        );
        action(
            tr!("palette.view.increase_text"),
            IconName::ZoomIn,
            Box::new(IncreaseFontSize),
            &["font", "bigger"],
        );
        action(
            tr!("palette.view.decrease_text"),
            IconName::ZoomOut,
            Box::new(DecreaseFontSize),
            &["font", "smaller"],
        );
        action(
            tr!("palette.view.reset_text"),
            IconName::RotateCcw,
            Box::new(ResetFontSize),
            &["font"],
        );
        action(
            tr!("palette.grid.copy_cells"),
            IconName::Copy,
            Box::new(GridCopyCells),
            &["clipboard"],
        );
        action(
            tr!("palette.grid.paste_cells"),
            IconName::ClipboardPaste,
            Box::new(GridPaste),
            &["clipboard"],
        );
        action(
            tr!("palette.file.open_sql"),
            IconName::FolderOpen,
            Box::new(OpenSqlFile),
            &["load"],
        );
        action(
            tr!("palette.file.save_as"),
            IconName::Save,
            Box::new(SaveQueryAs),
            &["export", "sql"],
        );
        action(
            tr!("palette.export.tables"),
            IconName::Download,
            Box::new(ExportTables),
            &["csv", "json", "sql"],
        );
        action(
            tr!("palette.export.active_table"),
            IconName::Download,
            Box::new(ExportActiveTable),
            &["csv", "json", "sql", "columns"],
        );
        action(
            tr!("palette.import.csv"),
            IconName::FileSpreadsheet,
            Box::new(ImportCsv),
            &["load"],
        );
        action(
            tr!("palette.import.json"),
            IconName::Braces,
            Box::new(ImportJson),
            &["load"],
        );
        action(
            tr!("palette.import.sql"),
            IconName::FileCode,
            Box::new(ImportSqlDump),
            &["load", "restore", "psql"],
        );
        action(
            tr!("palette.tools.users"),
            IconName::Users,
            Box::new(UserManagement),
            &["roles", "grant", "password"],
        );
        action(
            tr!("palette.view.toggle_results"),
            IconName::PanelBottom,
            Box::new(ToggleResultsPane),
            &["result", "editor", "full"],
        );
        action(
            tr!("palette.view.split_right"),
            IconName::Columns2,
            Box::new(SplitPaneRight),
            &["side by side", "pane"],
        );
        action(
            tr!("palette.view.next_pane"),
            IconName::ArrowRightLeft,
            Box::new(NextPane),
            &["pane"],
        );
        action(
            tr!("palette.view.show_all_tabs"),
            IconName::PanelTop,
            Box::new(ShowAllTabs),
            &["tabs", "switch"],
        );
        action(
            tr!("palette.tools.search"),
            IconName::Search,
            Box::new(SearchInDatabase),
            &["find", "grep", "text"],
        );
        action(
            tr!("palette.tools.processes"),
            IconName::Activity,
            Box::new(ProcessList),
            &["sessions", "kill", "pg_stat_activity"],
        );
        action(
            tr!("palette.changes.preview"),
            IconName::Eye,
            Box::new(PreviewChanges),
            &["review", "sql"],
        );
        action(
            tr!("palette.changes.discard"),
            IconName::Undo2,
            Box::new(DiscardChanges),
            &["revert", "cancel"],
        );
        action(
            tr!("palette.connection.open_database"),
            IconName::Database,
            Box::new(OpenDatabase),
            &["switch"],
        );
        action(
            tr!("palette.connection.open_connection"),
            IconName::Server,
            Box::new(OpenConnection),
            &["switch"],
        );
        action(
            tr!("palette.connection.reload_workspace"),
            IconName::RefreshCw,
            Box::new(ReloadWorkspace),
            &["refresh"],
        );
        action(
            tr!("palette.connection.reconnect"),
            IconName::RefreshCw,
            Box::new(Reconnect),
            &[],
        );
        action(
            tr!("palette.tab.next"),
            IconName::ChevronRight,
            Box::new(NextTab),
            &[],
        );
        action(
            tr!("palette.tab.previous"),
            IconName::ChevronLeft,
            Box::new(PrevTab),
            &[],
        );
        action(
            tr!("palette.tab.close_all"),
            IconName::X,
            Box::new(CloseAllTabs),
            &[],
        );
        action(
            tr!("palette.view.toggle_ai"),
            IconName::Sparkles,
            Box::new(ToggleAiPanel),
            &[
                "ai",
                "agent",
                "assistant",
                "claude",
                "codex",
                "gemini",
                "acp",
                "chat",
            ],
        );
        action(
            tr!("palette.ai.send_to_chat"),
            IconName::Sparkles,
            Box::new(SendToChat),
            &["context", "attach", "selection", "query", "row", "agent"],
        );
        action(
            tr!("palette.view.toggle_row_detail"),
            IconName::PanelRight,
            Box::new(ToggleRowDetail),
            &["row", "record", "json", "form", "right sidebar"],
        );
        action(
            tr!("palette.view.toggle_console"),
            IconName::SquareTerminal,
            Box::new(ToggleConsole),
            &["log", "debug", "queries"],
        );
        action(
            tr!("palette.view.toggle_problems"),
            IconName::TriangleAlert,
            Box::new(ToggleProblems),
            &["lint", "errors", "warnings", "diagnostics"],
        );
        action(
            tr!("palette.view.history"),
            IconName::Clock,
            Box::new(ShowHistory),
            &["recent", "queries", "log"],
        );
        action(
            tr!("palette.grid.filters"),
            IconName::ListFilter,
            Box::new(ToggleFilters),
            &["where", "search"],
        );
        action(
            tr!("palette.grid.data"),
            IconName::Table,
            Box::new(ShowData),
            &["rows"],
        );
        action(
            tr!("palette.grid.structure"),
            IconName::Columns3,
            Box::new(ShowStructure),
            &["columns", "schema", "ddl"],
        );
        // ---- view ----
        action(
            tr!("palette.view.toggle_sidebar"),
            IconName::PanelLeft,
            Box::new(ToggleSidebar),
            &["hide", "show"],
        );
        action(
            tr!("palette.view.tables"),
            IconName::Table,
            Box::new(ShowTablesPanel),
            &["panel"],
        );
        action(
            tr!("palette.view.views"),
            IconName::Eye,
            Box::new(ShowViewsPanel),
            &["panel"],
        );
        action(
            tr!("palette.view.functions"),
            IconName::Code,
            Box::new(ShowFunctionsPanel),
            &["panel"],
        );
        // ---- database / schema switching ----
        for db in &ctx.databases {
            if *db == ctx.current_database {
                continue;
            }
            let go = db.clone();
            dynamic.push((
                tr!("palette.database.switch_to", name = db),
                IconName::Database,
                std::rc::Rc::new(
                    move |this: &mut TuskApp, w: &mut Window, cx: &mut Context<TuskApp>| {
                        this.switch_database(go.clone(), w, cx)
                    },
                ),
            ));
        }
        for schema in &ctx.schemas {
            if *schema == ctx.current_schema {
                continue;
            }
            let go = schema.clone();
            dynamic.push((
                tr!("palette.schema.switch_to", name = schema),
                IconName::Layers,
                std::rc::Rc::new(
                    move |this: &mut TuskApp, _w: &mut Window, cx: &mut Context<TuskApp>| {
                        this.switch_schema(go.clone(), cx)
                    },
                ),
            ));
        }
    }
    // ---- the focused table / view (active tab or sidebar selection) ----
    if ctx.connected {
        type Run = RunFn;
        let rc = |f: BoxedRun| -> Run { std::rc::Rc::from(f) };
        if let Some((kind, name)) = ctx.focus_object.clone() {
            use crate::app::TableKind;
            use crate::objects::Script;
            let is_table = kind == TableKind::Table;
            let mut add = |label: String, icon: IconName, f: BoxedRun| {
                dynamic.push((label, icon, rc(f)));
            };
            // table / view actions: one key set each, the noun lives in the
            // string so every language can put it where it reads best.
            let (k, n) = (kind.clone(), name.clone());
            add(
                if is_table {
                    tr!("palette.table.open_structure", name = name)
                } else {
                    tr!("palette.view.open_structure", name = name)
                },
                IconName::Columns3,
                Box::new(move |a, w, cx| a.open_structure(k.clone(), n.clone(), w, cx)),
            );
            let (k, n) = (kind.clone(), name.clone());
            add(
                if is_table {
                    tr!("palette.table.export", name = name)
                } else {
                    tr!("palette.view.export", name = name)
                },
                IconName::Download,
                Box::new(move |a, w, cx| a.export_object(k.clone(), n.clone(), w, cx)),
            );
            let (k, n) = (kind.clone(), name.clone());
            add(
                if is_table {
                    tr!("palette.table.copy_create", name = name)
                } else {
                    tr!("palette.view.copy_create", name = name)
                },
                IconName::Copy,
                Box::new(move |a, _, cx| a.copy_script(k.clone(), n.clone(), Script::Create, cx)),
            );
            let (k, n) = (kind.clone(), name.clone());
            add(
                if is_table {
                    tr!("palette.table.open_select", name = name)
                } else {
                    tr!("palette.view.open_select", name = name)
                },
                IconName::SquareTerminal,
                Box::new(move |a, w, cx| {
                    a.script_to_editor(k.clone(), n.clone(), Script::Select, w, cx)
                }),
            );
            let (k, n) = (kind.clone(), name.clone());
            add(
                if is_table {
                    tr!("palette.table.rename", name = name)
                } else {
                    tr!("palette.view.rename", name = name)
                },
                IconName::Pencil,
                Box::new(move |a, w, cx| a.start_rename(k.clone(), n.clone(), w, cx)),
            );
            let (k, n) = (kind.clone(), name.clone());
            add(
                if is_table {
                    tr!("palette.table.delete", name = name)
                } else {
                    tr!("palette.view.delete", name = name)
                },
                IconName::Trash,
                Box::new(move |a, _, cx| a.toggle_drop(k.clone(), n.clone(), cx)),
            );
            if is_table {
                let n = name.clone();
                add(
                    tr!("palette.table.import_into", name = name),
                    IconName::Upload,
                    Box::new(move |a, w, cx| a.import_csv_into(n.clone(), w, cx)),
                );
                let n = name.clone();
                add(
                    tr!("palette.table.duplicate", name = name),
                    IconName::CopyPlus,
                    Box::new(move |a, _, cx| a.duplicate_object(n.clone(), false, cx)),
                );
                let n = name.clone();
                add(
                    tr!("palette.table.duplicate_data", name = name),
                    IconName::CopyPlus,
                    Box::new(move |a, _, cx| a.duplicate_object(n.clone(), true, cx)),
                );
                let n = name.clone();
                add(
                    tr!("palette.table.truncate", name = name),
                    IconName::Eraser,
                    Box::new(move |a, w, cx| a.truncate_object(n.clone(), false, w, cx)),
                );
                let n = name.clone();
                add(
                    tr!("palette.table.truncate_cascade", name = name),
                    IconName::Eraser,
                    Box::new(move |a, w, cx| a.truncate_object(n.clone(), true, w, cx)),
                );
            }
        }
        dynamic.push((
            tr!("palette.table.new"),
            IconName::Plus,
            rc(Box::new(|a, w, cx| a.new_table_editor(w, cx))),
        ));
        dynamic.push((
            tr!("palette.view.new"),
            IconName::Plus,
            rc(Box::new(|a, w, cx| a.new_view_editor(w, cx))),
        ));
        dynamic.push((
            tr!("palette.sidebar.new_group"),
            IconName::FolderPlus,
            rc(Box::new(|a, w, cx| a.new_obj_group(None, w, cx))),
        ));
        dynamic.push((
            tr!("palette.sidebar.refresh"),
            IconName::RefreshCw,
            rc(Box::new(|a, _, cx| {
                let schema = a.palette_context().current_schema;
                a.fetch_objects_for(&schema, cx);
            })),
        ));
        if ctx.sql_tab_active {
            dynamic.push((
                tr!("palette.query.export_result"),
                IconName::Download,
                rc(Box::new(|a, _, cx| a.export_result(cx))),
            ));
        }
    }
    // ---- theme ----
    let current_theme = crate::settings::get();
    for light in [false, true] {
        for name in crate::themes::names(light) {
            let current = if light {
                &current_theme.light_theme
            } else {
                &current_theme.dark_theme
            };
            if name == current {
                continue;
            }
            let mode = if light {
                tr!("palette.theme.mode_light")
            } else {
                tr!("palette.theme.mode_dark")
            };
            dynamic.push((
                tr!("palette.theme.apply", name = name, mode = mode),
                IconName::Palette,
                std::rc::Rc::new(
                    move |_: &mut TuskApp, _: &mut Window, cx: &mut Context<TuskApp>| {
                        crate::settings::choose_theme(cx, name)
                    },
                ),
            ));
        }
    }
    for mode in crate::settings::Appearance::available() {
        if mode == current_theme.appearance {
            continue;
        }
        let mode_label = match mode {
            crate::settings::Appearance::System => tr!("palette.theme.appearance_system"),
            crate::settings::Appearance::Light => tr!("palette.theme.appearance_light"),
            crate::settings::Appearance::Dark => tr!("palette.theme.appearance_dark"),
            // A desktop brand name: kept as-is, lowercased like the rest.
            crate::settings::Appearance::Omarchy => mode.label().to_lowercase(),
        };
        dynamic.push((
            tr!("palette.theme.appearance", mode = mode_label),
            match mode {
                crate::settings::Appearance::Light => IconName::Sun,
                crate::settings::Appearance::Omarchy => IconName::Palette,
                _ => IconName::Moon,
            },
            std::rc::Rc::new(
                move |_: &mut TuskApp, _: &mut Window, cx: &mut Context<TuskApp>| {
                    crate::settings::update(cx, |p| p.appearance = mode)
                },
            ),
        ));
    }
    for (label, icon, run) in dynamic {
        items.push(CommandItem::new().label(label).icon(Icon::new(icon)));
        runs.push(run);
    }
    // ---- app ----
    let mut action = |label: String, icon: IconName, action: Box<dyn Action>| {
        items.push(
            CommandItem::new()
                .label(label)
                .icon(Icon::new(icon))
                .action(action),
        );
        runs.push(std::rc::Rc::new(
            |_: &mut TuskApp, _: &mut Window, _: &mut Context<TuskApp>| {},
        ));
    };
    action(
        tr!("palette.app.settings"),
        IconName::Settings,
        Box::new(OpenSettings),
    );
    action(
        tr!("palette.app.check_updates"),
        IconName::ArrowDownToLine,
        Box::new(CheckForUpdates),
    );
    action(
        tr!("palette.app.about"),
        IconName::Info,
        Box::new(ShowAbout),
    );
    action(
        tr!("palette.app.release_notes"),
        IconName::ExternalLink,
        Box::new(ShowReleaseNotes),
    );
    action(tr!("palette.app.hide"), IconName::EyeOff, Box::new(HideApp));
    action(tr!("palette.app.quit"), IconName::Power, Box::new(Quit));
    (items, runs)
}

/// Table rows for quick-open. Order matches `table_rows` stored on the overlay.
pub fn table_items(tables: &[TableRef]) -> Vec<CommandItem> {
    tables
        .iter()
        .map(|t| {
            // NOTE: CommandItem only takes monochrome `Icon`; the sidebar tree
            // uses the full-color DbIcon set, the palette stays Lucide.
            let icon = match t.kind {
                crate::app::TableKind::Table => Icon::new(IconName::Table),
                crate::app::TableKind::View => Icon::new(IconName::Eye),
                crate::app::TableKind::MaterializedView => Icon::new(IconName::Eye),
                crate::app::TableKind::Function => Icon::new(IconName::Code),
            };
            CommandItem::new()
                .label(format!("{}.{}", t.schema, t.name))
                .keywords([t.name.clone(), t.schema.clone(), t.kind.label().to_string()])
                .icon(icon)
        })
        .collect()
}

/// Plain name rows (databases / connections); the current one is marked.
fn name_items(names: &[String], icon: IconName, current: &str) -> Vec<CommandItem> {
    names
        .iter()
        .map(|n| {
            let label = if n == current {
                crate::tr!("palette.name_current", name = n)
            } else {
                n.clone()
            };
            CommandItem::new()
                .label(label)
                .keywords([n.clone()])
                .icon(Icon::new(icon))
        })
        .collect()
}

impl TuskApp {
    /// Render the floating palette overlay (top-centered,).
    pub fn render_palette(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let overlay = self.palette.as_ref().expect("palette open");
        let elevated = cx.theme().popover;
        let border = cx.theme().border;

        let placeholder = match overlay.mode {
            PaletteMode::Commands => tr!("palette.ph.commands"),
            PaletteMode::Tables => tr!("palette.ph.tables"),
            PaletteMode::Databases => tr!("palette.ph.databases"),
            PaletteMode::Connections => tr!("palette.ph.connections"),
            PaletteMode::Tabs => tr!("palette.ph.tabs"),
        };
        let cached = overlay
            .items
            .borrow()
            .as_ref()
            .filter(|(m, _)| *m == overlay.mode)
            .map(|(_, items)| items.clone());
        let items = match cached {
            Some(items) => items,
            None => {
                let items = match overlay.mode {
                    PaletteMode::Commands => command_rows(self).0,
                    PaletteMode::Tables => table_items(&overlay.table_rows),
                    PaletteMode::Databases => name_items(
                        &overlay.names,
                        IconName::Database,
                        &self.palette_context().current_database,
                    ),
                    PaletteMode::Connections => name_items(
                        &overlay.names,
                        IconName::Server,
                        &self.palette_context().active_connection,
                    ),
                    PaletteMode::Tabs => name_items(&overlay.names, IconName::PanelTop, ""),
                };
                *overlay.items.borrow_mut() = Some((overlay.mode, items.clone()));
                items
            }
        };

        div()
            // Full-window backdrop: a click outside the palette closes it.
            .id("palette-backdrop")
            .absolute()
            .inset_0()
            .pt(px(48.))
            .items_start()
            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                let view = cx.global::<TuskHandle>().0.clone();
                view.update(cx, |this, cx| this.close_palette(window, cx));
            })
            .flex()
            .flex_row()
            .justify_center()
            .child(
                div()
                    .id("palette-card")
                    .w(px(560.))
                    // Clicks inside stay inside.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .rounded(crate::theme::RADIUS_LG)
                    .border_1()
                    .border_color(border)
                    .bg(elevated)
                    .child(
                        Command::new(&overlay.state)
                            .items(items)
                            .placeholder(placeholder)
                            .on_confirm(|path, window, cx| {
                                let view = cx.global::<TuskHandle>().0.clone();
                                view.update(cx, |this, cx| {
                                    this.palette_confirm(path.row, window, cx);
                                });
                            })
                            .on_cancel(|window, cx| {
                                let view = cx.global::<TuskHandle>().0.clone();
                                view.update(cx, |this, cx| {
                                    this.close_palette(window, cx);
                                });
                            }),
                    ),
            )
    }
}
