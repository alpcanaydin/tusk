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
    let mut action = |label: &str, icon: IconName, action: Box<dyn Action>, keywords: &[&str]| {
        items.push(
            CommandItem::new()
                .label(label.to_string())
                .keywords(keywords.iter().map(|k| k.to_string()))
                .icon(Icon::new(icon))
                .action(action),
        );
        runs.push(noop.clone());
    };

    // ---- connection ----
    action(
        "connection: new…",
        IconName::Plus,
        Box::new(NewConnection),
        &["add", "create"],
    );
    action(
        "connection: open…",
        IconName::FolderOpen,
        Box::new(OpenConnections),
        &["manager", "groups", "saved"],
    );
    action(
        "connection: import from Docker Compose…",
        IconName::ArrowDownToLine,
        Box::new(ImportDockerCompose),
        &["docker", "compose", "yaml", "container"],
    );
    if ctx.connected {
        action(
            "connection: disconnect",
            IconName::LogOut,
            Box::new(Disconnect),
            &["close"],
        );
    } else {
        action(
            "connection: quick connect",
            IconName::Zap,
            Box::new(QuickConnect),
            &["docker"],
        );
    }
    // Backup / Restore work from anywhere (the window picks the database).
    action(
        "database: backup…",
        IconName::HardDriveDownload,
        Box::new(BackupDatabase),
        &["dump", "pg_dump", "export"],
    );
    action(
        "database: restore…",
        IconName::HardDriveUpload,
        Box::new(RestoreDatabase),
        &["pg_restore", "import", "load"],
    );
    let mut dynamic: Vec<(String, IconName, RunFn)> = Vec::new();
    if crate::migrate::available() {
        dynamic.push((
            "connection: migrate from TablePlus…".into(),
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
            format!("connection: connect to {name}"),
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
            "query: new SQL tab",
            IconName::SquareTerminal,
            Box::new(NewSqlTab),
            &["editor", "sql"],
        );
        action(
            "query: run current",
            IconName::Play,
            Box::new(RunCurrent),
            &["execute", "statement"],
        );
        action(
            "query: run all",
            IconName::ListVideo,
            Box::new(RunAll),
            &["execute", "script"],
        );
        action(
            "table: open…",
            IconName::Table,
            Box::new(QuickOpenTables),
            &["quick", "find", "go"],
        );
        action("tab: close", IconName::X, Box::new(CloseTab), &[]);
        action(
            "tab: refresh",
            IconName::RefreshCw,
            Box::new(RefreshActive),
            &["reload"],
        );
        // ---- edits ----
        action(
            "changes: save",
            IconName::Save,
            Box::new(SaveChanges),
            &["commit", "write"],
        );
        action(
            "changes: delete selection",
            IconName::Delete,
            Box::new(DeleteSelection),
            &["remove", "drop"],
        );
        action(
            "editor: toggle line comment",
            IconName::Hash,
            Box::new(ToggleLineComment),
            &["comment"],
        );
        action(
            "editor: toggle block comment",
            IconName::Hash,
            Box::new(ToggleBlockComment),
            &["comment"],
        );
        action(
            "editor: make upper case",
            IconName::CaseUpper,
            Box::new(MakeUpperCase),
            &["transform"],
        );
        action(
            "editor: make lower case",
            IconName::CaseLower,
            Box::new(MakeLowerCase),
            &["transform"],
        );
        action(
            "editor: capitalize",
            IconName::CaseSensitive,
            Box::new(Capitalize),
            &["transform"],
        );
        action(
            "view: increase text size",
            IconName::ZoomIn,
            Box::new(IncreaseFontSize),
            &["font", "bigger"],
        );
        action(
            "view: decrease text size",
            IconName::ZoomOut,
            Box::new(DecreaseFontSize),
            &["font", "smaller"],
        );
        action(
            "view: reset text size",
            IconName::RotateCcw,
            Box::new(ResetFontSize),
            &["font"],
        );
        action(
            "grid: copy selected cells",
            IconName::Copy,
            Box::new(GridCopyCells),
            &["clipboard"],
        );
        action(
            "grid: paste to selected cells",
            IconName::ClipboardPaste,
            Box::new(GridPaste),
            &["clipboard"],
        );
        action(
            "file: open .sql file…",
            IconName::FolderOpen,
            Box::new(OpenSqlFile),
            &["load"],
        );
        action(
            "file: save query as…",
            IconName::Save,
            Box::new(SaveQueryAs),
            &["export", "sql"],
        );
        action(
            "export: tables…",
            IconName::Download,
            Box::new(ExportTables),
            &["csv", "json", "sql"],
        );
        action(
            "export: this table (column selection)…",
            IconName::Download,
            Box::new(ExportActiveTable),
            &["csv", "json", "sql", "columns"],
        );
        action(
            "import: from CSV…",
            IconName::FileSpreadsheet,
            Box::new(ImportCsv),
            &["load"],
        );
        action(
            "import: from JSON…",
            IconName::Braces,
            Box::new(ImportJson),
            &["load"],
        );
        action(
            "import: from SQL dump…",
            IconName::FileCode,
            Box::new(ImportSqlDump),
            &["load", "restore", "psql"],
        );
        action(
            "tools: user management",
            IconName::Users,
            Box::new(UserManagement),
            &["roles", "grant", "password"],
        );
        action(
            "view: toggle query results pane",
            IconName::PanelBottom,
            Box::new(ToggleResultsPane),
            &["result", "editor", "full"],
        );
        action(
            "view: split pane right",
            IconName::Columns2,
            Box::new(SplitPaneRight),
            &["side by side", "pane"],
        );
        action(
            "view: select next pane",
            IconName::ArrowRightLeft,
            Box::new(NextPane),
            &["pane"],
        );
        action(
            "view: show all tabs",
            IconName::PanelTop,
            Box::new(ShowAllTabs),
            &["tabs", "switch"],
        );
        action(
            "tools: search in database…",
            IconName::Search,
            Box::new(SearchInDatabase),
            &["find", "grep", "text"],
        );
        action(
            "tools: process list",
            IconName::Activity,
            Box::new(ProcessList),
            &["sessions", "kill", "pg_stat_activity"],
        );
        action(
            "changes: preview SQL",
            IconName::Eye,
            Box::new(PreviewChanges),
            &["review", "sql"],
        );
        action(
            "changes: discard",
            IconName::Undo2,
            Box::new(DiscardChanges),
            &["revert", "cancel"],
        );
        action(
            "connection: open a database…",
            IconName::Database,
            Box::new(OpenDatabase),
            &["switch"],
        );
        action(
            "connection: open a connection…",
            IconName::Server,
            Box::new(OpenConnection),
            &["switch"],
        );
        action(
            "connection: reload workspace",
            IconName::RefreshCw,
            Box::new(ReloadWorkspace),
            &["refresh"],
        );
        action(
            "connection: reconnect",
            IconName::RefreshCw,
            Box::new(Reconnect),
            &[],
        );
        action("tab: next", IconName::ChevronRight, Box::new(NextTab), &[]);
        action(
            "tab: previous",
            IconName::ChevronLeft,
            Box::new(PrevTab),
            &[],
        );
        action("tab: close all", IconName::X, Box::new(CloseAllTabs), &[]);
        action(
            "view: toggle ai chat",
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
            "ai: send to chat",
            IconName::Sparkles,
            Box::new(SendToChat),
            &["context", "attach", "selection", "query", "row", "agent"],
        );
        action(
            "view: toggle row detail",
            IconName::PanelRight,
            Box::new(ToggleRowDetail),
            &["row", "record", "json", "form", "right sidebar"],
        );
        action(
            "view: toggle console",
            IconName::SquareTerminal,
            Box::new(ToggleConsole),
            &["log", "debug", "queries"],
        );
        action(
            "view: toggle problems",
            IconName::TriangleAlert,
            Box::new(ToggleProblems),
            &["lint", "errors", "warnings", "diagnostics"],
        );
        action(
            "view: query history",
            IconName::Clock,
            Box::new(ShowHistory),
            &["recent", "queries", "log"],
        );
        action(
            "grid: toggle filters",
            IconName::ListFilter,
            Box::new(ToggleFilters),
            &["where", "search"],
        );
        action(
            "grid: show data",
            IconName::Table,
            Box::new(ShowData),
            &["rows"],
        );
        action(
            "grid: show structure",
            IconName::Columns3,
            Box::new(ShowStructure),
            &["columns", "schema", "ddl"],
        );
        // ---- view ----
        action(
            "view: toggle sidebar",
            IconName::PanelLeft,
            Box::new(ToggleSidebar),
            &["hide", "show"],
        );
        action(
            "view: tables",
            IconName::Table,
            Box::new(ShowTablesPanel),
            &["panel"],
        );
        action(
            "view: views",
            IconName::Eye,
            Box::new(ShowViewsPanel),
            &["panel"],
        );
        action(
            "view: functions",
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
                format!("database: switch to {db}"),
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
                format!("schema: switch to {schema}"),
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
            let noun = if is_table { "table" } else { "view" };
            let mut add = |label: String, icon: IconName, f: BoxedRun| {
                dynamic.push((label, icon, rc(f)));
            };
            let (k, n) = (kind.clone(), name.clone());
            add(
                format!("{noun}: open structure of {name}"),
                IconName::Columns3,
                Box::new(move |a, w, cx| a.open_structure(k.clone(), n.clone(), w, cx)),
            );
            let (k, n) = (kind.clone(), name.clone());
            add(
                format!("{noun}: export {name}…"),
                IconName::Download,
                Box::new(move |a, w, cx| a.export_object(k.clone(), n.clone(), w, cx)),
            );
            let (k, n) = (kind.clone(), name.clone());
            add(
                format!("{noun}: copy CREATE script of {name}"),
                IconName::Copy,
                Box::new(move |a, _, cx| a.copy_script(k.clone(), n.clone(), Script::Create, cx)),
            );
            let (k, n) = (kind.clone(), name.clone());
            add(
                format!("{noun}: open SELECT script of {name}"),
                IconName::SquareTerminal,
                Box::new(move |a, w, cx| {
                    a.script_to_editor(k.clone(), n.clone(), Script::Select, w, cx)
                }),
            );
            let (k, n) = (kind.clone(), name.clone());
            add(
                format!("{noun}: rename {name}…"),
                IconName::Pencil,
                Box::new(move |a, w, cx| a.start_rename(k.clone(), n.clone(), w, cx)),
            );
            let (k, n) = (kind.clone(), name.clone());
            add(
                format!("{noun}: delete {name}"),
                IconName::Trash,
                Box::new(move |a, _, cx| a.toggle_drop(k.clone(), n.clone(), cx)),
            );
            if is_table {
                let n = name.clone();
                add(
                    format!("table: import CSV into {name}…"),
                    IconName::Upload,
                    Box::new(move |a, w, cx| a.import_csv_into(n.clone(), w, cx)),
                );
                let n = name.clone();
                add(
                    format!("table: duplicate {name} (structure)"),
                    IconName::CopyPlus,
                    Box::new(move |a, _, cx| a.duplicate_object(n.clone(), false, cx)),
                );
                let n = name.clone();
                add(
                    format!("table: duplicate {name} (structure + data)"),
                    IconName::CopyPlus,
                    Box::new(move |a, _, cx| a.duplicate_object(n.clone(), true, cx)),
                );
                let n = name.clone();
                add(
                    format!("table: truncate {name}…"),
                    IconName::Eraser,
                    Box::new(move |a, w, cx| a.truncate_object(n.clone(), false, w, cx)),
                );
                let n = name.clone();
                add(
                    format!("table: truncate {name} cascade…"),
                    IconName::Eraser,
                    Box::new(move |a, w, cx| a.truncate_object(n.clone(), true, w, cx)),
                );
            }
        }
        dynamic.push((
            "table: new table…".into(),
            IconName::Plus,
            rc(Box::new(|a, w, cx| a.new_table_editor(w, cx))),
        ));
        dynamic.push((
            "view: new view…".into(),
            IconName::Plus,
            rc(Box::new(|a, w, cx| a.new_view_editor(w, cx))),
        ));
        dynamic.push((
            "sidebar: new group…".into(),
            IconName::FolderPlus,
            rc(Box::new(|a, w, cx| a.new_obj_group(None, w, cx))),
        ));
        dynamic.push((
            "sidebar: refresh".into(),
            IconName::RefreshCw,
            rc(Box::new(|a, _, cx| {
                let schema = a.palette_context().current_schema;
                a.fetch_objects_for(&schema, cx);
            })),
        ));
        if ctx.sql_tab_active {
            dynamic.push((
                "query: export result…".into(),
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
            let mode = if light { "light" } else { "dark" };
            dynamic.push((
                format!("theme: {name} ({mode})"),
                IconName::Palette,
                std::rc::Rc::new(
                    move |_: &mut TuskApp, _: &mut Window, cx: &mut Context<TuskApp>| {
                        crate::settings::choose_theme(cx, name)
                    },
                ),
            ));
        }
    }
    for mode in crate::settings::Appearance::ALL {
        if mode == current_theme.appearance {
            continue;
        }
        dynamic.push((
            format!("theme: appearance {}", mode.label().to_lowercase()),
            if mode == crate::settings::Appearance::Light {
                IconName::Sun
            } else {
                IconName::Moon
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
    let mut action = |label: &str, icon: IconName, action: Box<dyn Action>| {
        items.push(
            CommandItem::new()
                .label(label.to_string())
                .icon(Icon::new(icon))
                .action(action),
        );
        runs.push(std::rc::Rc::new(
            |_: &mut TuskApp, _: &mut Window, _: &mut Context<TuskApp>| {},
        ));
    };
    action("app: settings", IconName::Settings, Box::new(OpenSettings));
    action(
        "app: check for updates…",
        IconName::ArrowDownToLine,
        Box::new(CheckForUpdates),
    );
    action("app: hide", IconName::EyeOff, Box::new(HideApp));
    action("app: quit", IconName::Power, Box::new(Quit));
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
                format!("{n}  (current)")
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
            PaletteMode::Commands => "Type a command…",
            PaletteMode::Tables => "Search tables…",
            PaletteMode::Databases => "Open a database…",
            PaletteMode::Connections => "Open a connection…",
            PaletteMode::Tabs => "Go to tab…",
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
                    .rounded(px(8.))
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
