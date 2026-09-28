//! File menu: open a .sql file into a query tab, save the query as a file,
//! and Import ▸ CSV / JSON (into the active table) / SQL dump.

use std::path::PathBuf;

use super::*;

impl TuskApp {
    /// The table File ▸ Import CSV / JSON loads into: the active table tab,
    /// else the table selected in the sidebar.
    fn import_target(&self) -> Option<String> {
        match self.active_tab.and_then(|ix| self.tabs.get(ix)) {
            Some(WorkspaceTab::Grid(g))
                if g.table.kind == TableKind::Table && g.draft.is_none() =>
            {
                Some(g.table.name.clone())
            }
            _ => self
                .selected_object
                .as_ref()
                .filter(|(k, _)| *k == TableKind::Table)
                .map(|(_, n)| n.clone()),
        }
    }

    /// ⌘O: a .sql file into a new query tab.
    pub(super) fn open_sql_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.screen != AppScreen::Workspace {
            return;
        }
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Open".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let text = std::fs::read_to_string(&path);
            let _ = this.update_in(cx, |this, window, cx| match text {
                Ok(text) => {
                    this.open_sql_tab_with(Some(text), window, cx);
                    if let Some(WorkspaceTab::Sql(t)) =
                        this.active_tab.and_then(|ix| this.tabs.get_mut(ix))
                    {
                        t.title = file_title(&path);
                        t.file = Some(path);
                    }
                    cx.notify();
                }
                Err(e) => this.toast(false, format!("{}: {e}", path.display())),
            });
        })
        .detach();
    }

    /// ⇧⌘S: the active query tab's text to a .sql file (asks where).
    pub(super) fn save_query_as(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.active_tab else { return };
        let Some(WorkspaceTab::Sql(t)) = self.tabs.get(ix) else {
            self.toast_info("Open a query tab to save it as a file.");
            cx.notify();
            return;
        };
        let text = t.editor.read(cx).text().to_string();
        let (dir, name) = match &t.file {
            Some(p) => (
                p.parent().map(PathBuf::from).unwrap_or_default(),
                p.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default(),
            ),
            None => (
                dirs::document_dir()
                    .or_else(dirs::home_dir)
                    .unwrap_or_default(),
                format!("{}.sql", t.title.replace(['/', ':'], "-")),
            ),
        };
        let rx = cx.prompt_for_new_path(&dir, Some(&name));
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(path))) = rx.await else { return };
            let result = std::fs::write(&path, text);
            let _ = this.update_in(cx, |this, _, cx| {
                match result {
                    Ok(()) => {
                        if let Some(WorkspaceTab::Sql(t)) = this.tabs.get_mut(ix) {
                            t.title = file_title(&path);
                            t.file = Some(path.clone());
                        }
                        this.toast(true, format!("Saved {}", path.display()));
                    }
                    Err(e) => this.toast(false, format!("{}: {e}", path.display())),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// File ▸ Import ▸ From CSV / From JSON into the active table.
    pub(super) fn import_file(&mut self, json: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.import_target() else {
            self.toast_info("Open or select the table to import into first.");
            cx.notify();
            return;
        };
        if !json {
            self.import_csv_into(name, window, cx);
            return;
        }
        let Some(pool) = self.pool.clone() else {
            return;
        };
        let schema = self.current_schema.clone();
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import".into()),
        });
        cx.spawn(async move |weak, cx: &mut AsyncApp| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let result = match std::fs::read(&path) {
                Ok(data) => crate::objects::import_json(&pool, &schema, &name, data).await,
                Err(e) => Err(format!("{}: {e}", path.display())),
            };
            let _ = weak.update(cx, |this: &mut TuskApp, cx| {
                match result {
                    Ok(n) => this.toast(true, format!("Imported {n} rows into {name}")),
                    Err(e) => this.toast(false, format!("Import failed: {e}")),
                }
                this.reload_tabs_of(&schema, &name, cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// File ▸ Import ▸ From SQL Dump: run a .sql file on this database
    /// (asks first; one transaction, stops at the first error).
    pub(super) fn import_sql_dump(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some((conn, password)), Some((host, port))) =
            (self.active_conn.clone(), self.reach.clone())
        else {
            return;
        };
        let ep = crate::backup::Endpoint {
            host,
            port,
            user: conn.user.clone(),
            password,
            ssl: conn.ssl,
        };
        let db = conn.database.clone();
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let Ok(answer) = this.update_in(cx, |_, window, cx| {
                window.prompt(
                    PromptLevel::Warning,
                    &format!("Run {} on {db}?", file_title(&path)),
                    Some("Every statement in the file runs in one transaction."),
                    &["Run", "Cancel"],
                    cx,
                )
            }) else {
                return;
            };
            if answer.await != Ok(0) {
                return;
            }
            let file = path.clone();
            let result = cx
                .background_executor()
                .spawn(async move { crate::backup::run_sql_file(&ep, &db, &file) })
                .await;
            let _ = this.update_in(cx, |this, _, cx| {
                match result {
                    Ok(_) => {
                        this.toast(true, format!("Ran {}", file_title(&path)));
                        let schema = this.current_schema.clone();
                        this.fetch_objects_for(&schema, cx);
                        this.refresh_active_tab(cx);
                    }
                    Err(e) => {
                        this.toast(false, format!("Import failed (rolled back): {}", e.trim()))
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
}

impl TuskApp {
    /// File ▸ Export ▸ Export Tables…: every table / view of the schema to
    /// pick from (none picked).
    pub(super) fn export_tables(&mut self, cx: &mut Context<Self>) {
        if self.pool.is_none() {
            return;
        }
        let mut all = self.objects.tables.clone();
        all.extend(self.objects.views.iter().cloned());
        all.extend(self.objects.matviews.iter().cloned());
        crate::export::ExportWindow::open(
            self.pool.clone(),
            crate::export::Source::Tables {
                schema: self.current_schema.clone(),
                all,
                picked: Vec::new(),
                filter: None,
            },
            cx,
        );
    }

    /// File ▸ Export ▸ Export this Table with Column Selection…: the active
    /// table (with its filter) — its fields picker chooses the columns.
    pub(super) fn export_active_table(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(WorkspaceTab::Grid(g)) = self.active_tab.and_then(|ix| self.tabs.get(ix)) else {
            self.toast_info("Open a table to export it.");
            cx.notify();
            return;
        };
        let (kind, name) = (g.table.kind.clone(), g.table.name.clone());
        self.export_object(kind, name, window, cx);
    }
}

fn file_title(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string())
}
