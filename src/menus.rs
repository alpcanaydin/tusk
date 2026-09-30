//! macOS menu bar. Every item is an action that also has a keybinding
//! (actions.rs), so the menu shows the shortcut next to it.

use gpui_kit::component::input::{Copy, Cut, Paste, Redo, SelectAll, Undo};
use gpui_kit::*;

use crate::actions::*;

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
pub fn refresh(cx: &mut App) {
    let update = if crate::updater::ready(cx).is_some() {
        Some(MenuItem::action("Restart to Update…", RestartToUpdate))
    } else if crate::updater::enabled(cx) {
        Some(MenuItem::action("Check for Updates…", CheckForUpdates))
    } else {
        None
    };
    let mut app_menu = vec![
        MenuItem::action("About Tusk", ShowAbout),
        MenuItem::action("Release Notes", ShowReleaseNotes),
        MenuItem::separator(),
        MenuItem::action("Settings…", OpenSettings),
    ];
    app_menu.extend(update);
    app_menu.push(MenuItem::action("Command Palette…", TogglePalette));
    cx.set_menus([
        Menu::new("Tusk").items(app_menu.into_iter().chain([
            MenuItem::separator(),
            MenuItem::action("Hide Tusk", HideApp),
            MenuItem::action("Quit Tusk", Quit),
        ])),
        Menu::new("File").items([
            MenuItem::action("New Connection…", NewConnection),
            MenuItem::action("Open Connection…", OpenConnections),
            MenuItem::separator(),
            MenuItem::action("New SQL Query", NewSqlTab),
            MenuItem::action("Open…", OpenSqlFile),
            MenuItem::action("Save As…", SaveQueryAs),
            MenuItem::action("Open Table…", QuickOpenTables),
            MenuItem::separator(),
            MenuItem::action("Backup Database…", BackupDatabase),
            MenuItem::action("Restore Database…", RestoreDatabase),
            MenuItem::submenu(Menu::new("Export").items([
                MenuItem::action("Export Tables…", ExportTables),
                MenuItem::action(
                    "Export this Table with Column Selection…",
                    ExportActiveTable,
                ),
            ])),
            MenuItem::submenu(Menu::new("Import").items([
                MenuItem::action("From CSV…", ImportCsv),
                MenuItem::action("From JSON…", ImportJson),
                MenuItem::action("From SQL Dump…", ImportSqlDump),
                MenuItem::separator(),
                MenuItem::action("Connections from Docker Compose…", ImportDockerCompose),
            ])),
            MenuItem::separator(),
            MenuItem::action("Close Tab", CloseTab),
            MenuItem::action("Close All Tabs", CloseAllTabs),
        ]),
        Menu::new("Edit").items([
            MenuItem::os_action("Undo", Undo, OsAction::Undo),
            MenuItem::os_action("Redo", Redo, OsAction::Redo),
            MenuItem::separator(),
            MenuItem::action("Commit", SaveChanges),
            MenuItem::action("Discard", DiscardChanges),
            MenuItem::action("Preview", PreviewChanges),
            MenuItem::separator(),
            MenuItem::action("Add Row", GridInsertRow),
            MenuItem::action("Duplicate Row", GridDuplicateRow),
            MenuItem::separator(),
            MenuItem::os_action("Cut", Cut, OsAction::Cut),
            MenuItem::os_action("Copy", Copy, OsAction::Copy),
            MenuItem::os_action("Paste", Paste, OsAction::Paste),
            MenuItem::action("Copy Selected Cells", GridCopyCells),
            MenuItem::action("Paste to Selected Cells", GridPaste),
            MenuItem::action("Delete", DeleteSelection),
            MenuItem::os_action("Select All", SelectAll, OsAction::SelectAll),
            MenuItem::separator(),
            MenuItem::action("Toggle Line Comment", ToggleLineComment),
            MenuItem::action("Toggle Block Comment", ToggleBlockComment),
            MenuItem::action("Increase Font Size", IncreaseFontSize),
            MenuItem::action("Decrease Font Size", DecreaseFontSize),
            MenuItem::submenu(Menu::new("Transformations").items([
                MenuItem::action("Make Upper Case", MakeUpperCase),
                MenuItem::action("Make Lower Case", MakeLowerCase),
                MenuItem::action("Capitalize", Capitalize),
            ])),
            MenuItem::separator(),
            MenuItem::action("Filter…", ToggleFilters),
        ]),
        Menu::new("View").items([
            MenuItem::action("Tables", ShowTablesPanel),
            MenuItem::action("Views", ShowViewsPanel),
            MenuItem::action("Functions", ShowFunctionsPanel),
            MenuItem::separator(),
            MenuItem::action("Toggle Left Sidebar", ToggleSidebar),
            MenuItem::action("Toggle Right Sidebar (Row Detail)", ToggleRowDetail),
            MenuItem::action("Toggle AI Chat", ToggleAiPanel),
            MenuItem::action("Send to Chat", SendToChat),
            MenuItem::action("Toggle Console", ToggleConsole),
            MenuItem::action("Query History", ShowHistory),
            MenuItem::action("Problems", ToggleProblems),
            MenuItem::action("Toggle Query Results Pane", ToggleResultsPane),
            MenuItem::action("Show All Tabs", ShowAllTabs),
        ]),
        Menu::new("Tools").items([
            MenuItem::action("Process List", ProcessList),
            MenuItem::action("User Management", UserManagement),
            MenuItem::action("Search in Database…", SearchInDatabase),
        ]),
        Menu::new("Connection").items([
            MenuItem::action("New…", NewConnection),
            MenuItem::action("Open a Database…", OpenDatabase),
            MenuItem::action("Open a Connection…", OpenConnection),
            MenuItem::separator(),
            MenuItem::action("Run Current Query", RunCurrent),
            MenuItem::action("Run All Queries", RunAll),
            MenuItem::separator(),
            MenuItem::action("Reload Workspace", ReloadWorkspace),
            MenuItem::action("Reload Current Tab", RefreshActive),
            MenuItem::action("Reconnect", Reconnect),
            MenuItem::action("Disconnect", Disconnect),
        ]),
        Menu::new("Navigate").items([
            MenuItem::action("Open Anything", QuickOpenTables),
            MenuItem::action("Command Palette…", TogglePalette),
            MenuItem::action("New Tab", NewSqlTab),
            MenuItem::action("Close Tab", CloseTab),
            MenuItem::separator(),
            MenuItem::action("Show Table Data", ShowData),
            MenuItem::action("Show Table Structure", ShowStructure),
            MenuItem::separator(),
            MenuItem::action("Go Back", GoBack),
            MenuItem::action("Go Forward", GoForward),
            MenuItem::action("Select Next Tab", NextTab),
            MenuItem::action("Select Previous Tab", PrevTab),
            MenuItem::separator(),
            MenuItem::action("Split Pane Right", SplitPaneRight),
            MenuItem::action("Select Next Pane", NextPane),
        ]),
        Menu::new("Window").items([
            MenuItem::action("Minimize", MinimizeWindow),
            MenuItem::action("Zoom", ZoomWindow),
            // "Enter Full Screen" is in View: macOS adds it there itself.
            MenuItem::separator(),
            MenuItem::action("Bring All to Front", BringAllToFront),
        ]),
        Menu::new("Help").items([
            MenuItem::action("Tusk on GitHub", OpenHelp),
            MenuItem::action("Report an Issue…", ReportIssue),
        ]),
    ]);
}
