//! macOS menu bar. Every item is an action that also has a keybinding
//! (actions.rs), so the menu shows the shortcut next to it.

use gpui_kit::component::input::{Copy, Cut, Paste, Redo, SelectAll, Undo};
use gpui_kit::*;

use crate::actions::*;
use crate::tr;

const REPO: &str = "https://github.com/alpcanaydin/tusk";

/// Window-menu commands act on the key window (a main or a secondary one).
/// On macOS they go through AppKit's own responder actions, the path the
/// system's View ▸ Enter Full Screen takes: GPUI's minimize / zoom /
/// fullscreen calls were ignored when sent from a menu or shortcut.
#[cfg(target_os = "macos")]
fn window_command(cx: &mut App, selector: &'static str, _fallback: fn(&Window)) {
    cx.defer(move |_| {
        use objc2::runtime::{AnyObject, Sel};
        use objc2::{MainThreadMarker, msg_send};
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let app = objc2_app_kit::NSApplication::sharedApplication(mtm);
        let sel = Sel::register(&std::ffi::CString::new(selector).unwrap());
        let nil: *const AnyObject = std::ptr::null();
        // Target nil: the first responder chain, i.e. the key window.
        let _: bool = unsafe { msg_send![&*app, sendAction: sel, to: nil, from: nil] };
    });
}

#[cfg(not(target_os = "macos"))]
fn window_command(cx: &mut App, _selector: &'static str, f: fn(&Window)) {
    cx.defer(move |cx| {
        if let Some(w) = cx.active_window() {
            let _ = w.update(cx, |_, window, _| f(window));
        }
    });
}

pub fn install(cx: &mut App) {
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &HideApp, cx| cx.hide());
    cx.on_action(|_: &OpenSettings, cx| crate::settings::SettingsWindow::open(cx));
    cx.on_action(|_: &ShowAbout, cx| crate::about::AboutWindow::open(cx));
    cx.on_action(|_: &ShowReleaseNotes, cx| crate::whats_new::open_release_notes(cx));
    cx.on_action(|_: &CheckForUpdates, _| crate::updater::check_for_updates());
    cx.on_action(|_: &RestartToUpdate, _| crate::updater::restart_to_update());
    cx.on_action(|_: &MinimizeWindow, cx| {
        window_command(cx, "performMiniaturize:", Window::minimize_window)
    });
    cx.on_action(|_: &ZoomWindow, cx| window_command(cx, "performZoom:", Window::zoom_window));
    cx.on_action(|_: &ToggleFullScreen, cx| {
        window_command(cx, "toggleFullScreen:", Window::toggle_fullscreen)
    });
    cx.on_action(|_: &BringAllToFront, cx| {
        for w in cx.windows() {
            let _ = w.update(cx, |_, window, _| window.activate_window());
        }
        cx.activate(true);
    });
    cx.on_action(|_: &OpenHelp, cx| cx.open_url(REPO));
    cx.on_action(|_: &ReportIssue, cx| cx.open_url(&format!("{REPO}/issues/new")));
    // The standard macOS Window-menu shortcuts.
    #[cfg(target_os = "macos")]
    cx.bind_keys([
        KeyBinding::new("cmd-m", MinimizeWindow, None),
        KeyBinding::new("ctrl-cmd-f", ToggleFullScreen, None),
    ]);
    refresh(cx);
}

/// (Re)build the menu bar; the app menu's update item follows the updater.
/// Rebuilt on every language switch (`i18n::set_language`) — `set_menus` is
/// not part of the window render pass.
pub fn refresh(cx: &mut App) {
    let update = if crate::updater::ready(cx).is_some() {
        Some(MenuItem::action(
            tr!("menu.app.restart_update"),
            RestartToUpdate,
        ))
    } else if crate::updater::enabled(cx) {
        Some(MenuItem::action(
            tr!("menu.app.check_updates"),
            CheckForUpdates,
        ))
    } else {
        None
    };
    let mut app_menu = vec![
        MenuItem::action(tr!("menu.app.about"), ShowAbout),
        MenuItem::action(tr!("menu.app.release_notes"), ShowReleaseNotes),
        MenuItem::separator(),
        MenuItem::action(tr!("menu.app.settings"), OpenSettings),
    ];
    app_menu.extend(update);
    app_menu.push(MenuItem::action(
        tr!("menu.app.command_palette"),
        TogglePalette,
    ));
    cx.set_menus([
        Menu::new("Tusk").items(app_menu.into_iter().chain([
            MenuItem::separator(),
            MenuItem::action(tr!("menu.app.hide"), HideApp),
            MenuItem::action(tr!("menu.app.quit"), Quit),
        ])),
        Menu::new(tr!("menu.title.file")).items([
            MenuItem::action(tr!("menu.file.new_connection"), NewConnection),
            MenuItem::action(tr!("menu.file.open_connection"), OpenConnections),
            MenuItem::separator(),
            MenuItem::action(tr!("menu.file.new_sql"), NewSqlTab),
            MenuItem::action(tr!("menu.file.open_sql"), OpenSqlFile),
            MenuItem::action(tr!("menu.file.save_as"), SaveQueryAs),
            MenuItem::action(tr!("menu.file.open_table"), QuickOpenTables),
            MenuItem::separator(),
            MenuItem::action(tr!("menu.file.backup"), BackupDatabase),
            MenuItem::action(tr!("menu.file.restore"), RestoreDatabase),
            MenuItem::submenu(Menu::new(tr!("menu.file.export")).items([
                MenuItem::action(tr!("menu.file.export_tables"), ExportTables),
                MenuItem::action(tr!("menu.file.export_active_table"), ExportActiveTable),
            ])),
            MenuItem::submenu(Menu::new(tr!("menu.file.import")).items([
                MenuItem::action(tr!("menu.file.import_csv"), ImportCsv),
                MenuItem::action(tr!("menu.file.import_json"), ImportJson),
                MenuItem::action(tr!("menu.file.import_sql"), ImportSqlDump),
                MenuItem::separator(),
                MenuItem::action(tr!("menu.file.import_compose"), ImportDockerCompose),
            ])),
            MenuItem::separator(),
            MenuItem::action(tr!("menu.file.close_tab"), CloseTab),
            MenuItem::action(tr!("menu.file.close_all_tabs"), CloseAllTabs),
        ]),
        Menu::new(tr!("menu.title.edit")).items([
            MenuItem::os_action(tr!("common.undo"), Undo, OsAction::Undo),
            MenuItem::os_action(tr!("common.redo"), Redo, OsAction::Redo),
            MenuItem::separator(),
            MenuItem::action(tr!("menu.edit.commit"), SaveChanges),
            MenuItem::action(tr!("menu.edit.discard"), DiscardChanges),
            MenuItem::action(tr!("menu.edit.preview"), PreviewChanges),
            MenuItem::separator(),
            MenuItem::action(tr!("menu.edit.add_row"), GridInsertRow),
            MenuItem::action(tr!("menu.edit.duplicate_row"), GridDuplicateRow),
            MenuItem::separator(),
            MenuItem::os_action(tr!("common.cut"), Cut, OsAction::Cut),
            MenuItem::os_action(tr!("common.copy"), Copy, OsAction::Copy),
            MenuItem::os_action(tr!("common.paste"), Paste, OsAction::Paste),
            MenuItem::action(tr!("menu.edit.copy_cells"), GridCopyCells),
            MenuItem::action(tr!("menu.edit.paste_cells"), GridPaste),
            MenuItem::action(tr!("menu.edit.delete"), DeleteSelection),
            MenuItem::os_action(tr!("common.select_all"), SelectAll, OsAction::SelectAll),
            MenuItem::separator(),
            MenuItem::action(tr!("menu.edit.line_comment"), ToggleLineComment),
            MenuItem::action(tr!("menu.edit.block_comment"), ToggleBlockComment),
            MenuItem::action(tr!("menu.edit.increase_font"), IncreaseFontSize),
            MenuItem::action(tr!("menu.edit.decrease_font"), DecreaseFontSize),
            MenuItem::submenu(Menu::new(tr!("menu.edit.transformations")).items([
                MenuItem::action(tr!("menu.edit.make_upper"), MakeUpperCase),
                MenuItem::action(tr!("menu.edit.make_lower"), MakeLowerCase),
                MenuItem::action(tr!("menu.edit.capitalize"), Capitalize),
            ])),
            MenuItem::separator(),
            MenuItem::action(tr!("menu.edit.filter"), ToggleFilters),
        ]),
        Menu::new(tr!("menu.title.view")).items([
            MenuItem::action(tr!("menu.view.tables"), ShowTablesPanel),
            MenuItem::action(tr!("menu.view.views"), ShowViewsPanel),
            MenuItem::action(tr!("menu.view.functions"), ShowFunctionsPanel),
            MenuItem::separator(),
            MenuItem::action(tr!("menu.view.toggle_sidebar"), ToggleSidebar),
            MenuItem::action(tr!("menu.view.toggle_row_detail"), ToggleRowDetail),
            MenuItem::action(tr!("menu.view.toggle_ai"), ToggleAiPanel),
            MenuItem::action(tr!("menu.view.send_to_chat"), SendToChat),
            MenuItem::action(tr!("menu.view.toggle_console"), ToggleConsole),
            MenuItem::action(tr!("menu.view.history"), ShowHistory),
            MenuItem::action(tr!("menu.view.problems"), ToggleProblems),
            MenuItem::action(tr!("menu.view.toggle_results"), ToggleResultsPane),
            MenuItem::action(tr!("menu.view.show_all_tabs"), ShowAllTabs),
        ]),
        Menu::new(tr!("menu.title.tools")).items([
            MenuItem::action(tr!("menu.tools.processes"), ProcessList),
            MenuItem::action(tr!("menu.tools.users"), UserManagement),
            MenuItem::action(tr!("menu.tools.search"), SearchInDatabase),
        ]),
        Menu::new(tr!("menu.title.connection")).items([
            MenuItem::action(tr!("menu.connection.new"), NewConnection),
            MenuItem::action(tr!("menu.connection.open_database"), OpenDatabase),
            MenuItem::action(tr!("menu.connection.open_connection"), OpenConnection),
            MenuItem::separator(),
            MenuItem::action(tr!("menu.connection.run_current"), RunCurrent),
            MenuItem::action(tr!("menu.connection.run_all"), RunAll),
            MenuItem::separator(),
            MenuItem::action(tr!("menu.connection.reload_workspace"), ReloadWorkspace),
            MenuItem::action(tr!("menu.connection.refresh_tab"), RefreshActive),
            MenuItem::action(tr!("menu.connection.reconnect"), Reconnect),
            MenuItem::action(tr!("menu.connection.disconnect"), Disconnect),
        ]),
        Menu::new(tr!("menu.title.navigate")).items([
            MenuItem::action(tr!("menu.navigate.open_anything"), QuickOpenTables),
            MenuItem::action(tr!("menu.navigate.command_palette"), TogglePalette),
            MenuItem::action(tr!("menu.navigate.new_tab"), NewSqlTab),
            MenuItem::action(tr!("menu.navigate.close_tab"), CloseTab),
            MenuItem::separator(),
            MenuItem::action(tr!("menu.navigate.show_data"), ShowData),
            MenuItem::action(tr!("menu.navigate.show_structure"), ShowStructure),
            MenuItem::separator(),
            MenuItem::action(tr!("menu.navigate.go_back"), GoBack),
            MenuItem::action(tr!("menu.navigate.go_forward"), GoForward),
            MenuItem::action(tr!("menu.navigate.next_tab"), NextTab),
            MenuItem::action(tr!("menu.navigate.prev_tab"), PrevTab),
            MenuItem::separator(),
            MenuItem::action(tr!("menu.navigate.split_right"), SplitPaneRight),
            MenuItem::action(tr!("menu.navigate.next_pane"), NextPane),
        ]),
        Menu::new(tr!("menu.title.window")).items([
            MenuItem::action(tr!("menu.window.minimize"), MinimizeWindow),
            MenuItem::action(tr!("menu.window.zoom"), ZoomWindow),
            // "Enter Full Screen" is in View: macOS adds it there itself.
            MenuItem::separator(),
            MenuItem::action(tr!("menu.window.bring_all"), BringAllToFront),
        ]),
        Menu::new(tr!("menu.title.help")).items([
            MenuItem::action(tr!("menu.help.github"), OpenHelp),
            MenuItem::action(tr!("menu.help.report"), ReportIssue),
        ]),
    ]);
}
