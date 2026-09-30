//! Sidebar object right-click menu: open / structure, copy name
//! and scripts, new query / table, export, import CSV, rename, duplicate,
//! truncate, delete, refresh. Child module of `app`.

use gpui_kit::component::input::InputEvent;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};

use super::*;
use crate::objects::{self, ObjKind, Script};

fn obj_kind(kind: &TableKind) -> ObjKind {
    match kind {
        TableKind::Table => ObjKind::Table,
        TableKind::View => ObjKind::View,
        TableKind::MaterializedView => ObjKind::MatView,
        TableKind::Function => ObjKind::Function,
    }
}

/// Run `f` on the app from a menu click (menus only get `&mut App`).
fn with_app(cx: &mut App, f: impl FnOnce(&mut TuskApp, &mut Context<TuskApp>)) {
    let view = cx.global::<TuskHandle>().0.clone();
    view.update(cx, f);
}

impl TuskApp {
    pub(super) fn object_menu(
        kind: TableKind,
        name: String,
        menu: PopupMenu,
        window: &mut Window,
        cx: &mut Context<PopupMenu>,
    ) -> PopupMenu {
        let is_table = kind == TableKind::Table;
        let is_fn = kind == TableKind::Function;
        let ok = obj_kind(&kind);

        let (k, n) = (kind.clone(), name.clone());
        let script_menu = PopupMenu::build(window, cx, move |mut m, _, _| {
            for &which in Script::for_kind(ok) {
                let (k, n) = (k.clone(), n.clone());
                m = m.item(PopupMenuItem::new(which.label()).on_click(move |_, _, cx| {
                    let (k, n) = (k.clone(), n.clone());
                    with_app(cx, |app, cx| app.copy_script(k, n, which, cx));
                }));
            }
            m
        });
        let (k, n) = (kind.clone(), name.clone());
        let open_script_menu = PopupMenu::build(window, cx, move |mut m, _, _| {
            for &which in Script::for_kind(ok) {
                let (k, n) = (k.clone(), n.clone());
                m = m.item(
                    PopupMenuItem::new(which.label()).on_click(move |_, window, cx| {
                        let (k, n) = (k.clone(), n.clone());
                        with_app(cx, |app, cx| app.script_to_editor(k, n, which, window, cx));
                    }),
                );
            }
            m
        });

        let item = |label: &'static str,
                    f: fn(&mut TuskApp, TableKind, String, &mut Window, &mut Context<TuskApp>)| {
            let (k, n) = (kind.clone(), name.clone());
            PopupMenuItem::new(label).on_click(move |_, window, cx| {
                let (k, n) = (k.clone(), n.clone());
                with_app(cx, |app, cx| f(app, k, n, window, cx));
            })
        };

        let danger = |label: &'static str,
                      f: fn(
            &mut TuskApp,
            TableKind,
            String,
            &mut Window,
            &mut Context<TuskApp>,
        )| {
            let (k, n) = (kind.clone(), name.clone());
            crate::theme::danger_item(label, move |_, window, cx| {
                let (k, n) = (k.clone(), n.clone());
                with_app(cx, |app, cx| f(app, k, n, window, cx));
            })
        };

        let mut menu = menu.item(item(
            if is_fn { "Select" } else { "Open" },
            |app, k, n, w, cx| {
                app.sidebar_focus.focus(w, cx);
                app.on_pick_object(k, n, w, cx);
            },
        ));
        if !is_fn {
            menu = menu.item(item("Open Structure", |app, k, n, w, cx| {
                app.open_structure(k, n, w, cx)
            }));
        }
        menu = menu
            .separator()
            .item(item("Copy Name", |_, _, n, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(n));
            }))
            .item(
                item("Send to Chat", |app, k, n, w, cx| {
                    let schema = app.current_schema.clone();
                    app.send_table_to_chat(k, schema, n, w, cx);
                })
                .action(Box::new(SendToChat)),
            )
            .item(PopupMenuItem::submenu("Copy Script As", script_menu))
            .item(PopupMenuItem::submenu(
                "Open Script in Editor",
                open_script_menu,
            ))
            .separator()
            .item(
                item("New Query", |app, _, _, w, cx| app.open_sql_tab(w, cx))
                    .action(Box::new(NewSqlTab)),
            )
            .item(item("New Table…", |app, _, _, w, cx| {
                app.new_table_editor(w, cx)
            }))
            .item(item("New View…", |app, _, _, w, cx| {
                app.new_view_editor(w, cx)
            }));
        if !is_fn {
            menu = menu.separator().item(item("Export…", |app, k, n, w, cx| {
                app.export_object(k, n, w, cx)
            }));
        }
        if is_table {
            menu = menu.item(item("Import from CSV…", |app, _, n, w, cx| {
                app.import_csv_into(n, w, cx)
            }));
        }
        let member = format!("{}:{name}", kind.label());
        let group_menu = Self::move_to_group_menu(member, window, cx);
        menu = menu
            .separator()
            .item(PopupMenuItem::submenu("Move to Group", group_menu))
            .item(item("Rename…", |app, k, n, w, cx| {
                app.start_rename(k, n, w, cx)
            }));
        if is_table {
            let (n1, n2) = (name.clone(), name.clone());
            let dup = PopupMenu::build(window, cx, move |m, _, _| {
                let (a, b) = (n1.clone(), n2.clone());
                m.item(
                    PopupMenuItem::new("Structure Only").on_click(move |_, _, cx| {
                        let a = a.clone();
                        with_app(cx, |app, cx| app.duplicate_object(a, false, cx));
                    }),
                )
                .item(
                    PopupMenuItem::new("Structure and Data").on_click(move |_, _, cx| {
                        let b = b.clone();
                        with_app(cx, |app, cx| app.duplicate_object(b, true, cx));
                    }),
                )
            });
            menu = menu.item(PopupMenuItem::submenu("Duplicate", dup));
        }
        menu = menu.item(
            item("Refresh", |app, _, _, _, cx| {
                let schema = app.current_schema.clone();
                app.fetch_objects_for(&schema, cx);
            })
            .action(Box::new(RefreshActive)),
        );
        // Destructive actions last, in red.
        menu = menu.separator();
        if is_table {
            menu = menu
                .item(danger("Truncate…", |app, _, n, w, cx| {
                    app.truncate_object(n, false, w, cx)
                }))
                .item(danger("Truncate Cascade…", |app, _, n, w, cx| {
                    app.truncate_object(n, true, w, cx)
                }));
        }
        menu.item(danger("Delete", |app, k, n, _, cx| {
            app.toggle_drop(k, n, cx)
        }))
    }

    /// Backup / Restore window, the current connection + database picked.
    pub(crate) fn open_backup(&mut self, mode: crate::backup::Mode, cx: &mut Context<Self>) {
        let pre = self
            .active_conn
            .as_ref()
            .map(|(c, _)| (c.name.clone(), c.database.clone()));
        crate::backup::BackupWindow::open(mode, pre, cx);
    }

    /// Export window for a table / view; an open, filtered grid of it
    /// exports only the matching rows.
    pub(crate) fn export_object(
        &mut self,
        _kind: TableKind,
        name: String,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let schema = self.current_schema.clone();
        let filter = self.tabs.iter().find_map(|t| match t {
            WorkspaceTab::Grid(g) if g.table.schema == schema && g.table.name == name => {
                g.filters.applied.clone()
            }
            _ => None,
        });
        let mut all = self.objects.tables.clone();
        all.extend(self.objects.views.iter().cloned());
        all.extend(self.objects.matviews.iter().cloned());
        crate::export::ExportWindow::open(
            self.pool.clone(),
            crate::export::Source::Tables {
                schema,
                all,
                picked: vec![name],
                filter,
            },
            cx,
        );
    }

    pub(crate) fn open_structure(
        &mut self,
        kind: TableKind,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.on_pick_object(kind, name, window, cx);
        let Some(ix) = self.active_tab else { return };
        // A freshly opened tab has no columns yet, and the Structure view is
        // built from them: switch once they've loaded (≤ 5 s).
        let handle = window.window_handle();
        cx.spawn(async move |weak, cx: &mut AsyncApp| {
            for _ in 0..100 {
                let ready = weak
                    .update(cx, |this: &mut TuskApp, cx| {
                        this.grid_tab(ix)
                            .is_some_and(|t| !t.state.read(cx).delegate().metas.is_empty())
                    })
                    .unwrap_or(true);
                if ready {
                    break;
                }
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(50))
                    .await;
            }
            let _ = handle.update(cx, |_, window, cx| {
                let _ = weak.update(cx, |this: &mut TuskApp, cx| {
                    this.set_tab_view(ix, TabView::Structure, window, cx)
                });
            });
        })
        .detach();
    }

    fn fetch_script(
        &mut self,
        kind: TableKind,
        name: String,
        which: Script,
        cx: &mut Context<Self>,
        done: impl FnOnce(&mut TuskApp, String, &mut Context<TuskApp>) + 'static,
    ) {
        let Some(pool) = self.pool.clone() else {
            return;
        };
        let schema = self.current_schema.clone();
        cx.spawn(async move |weak, cx: &mut AsyncApp| {
            let result = objects::script(&pool, obj_kind(&kind), &schema, &name, which).await;
            let _ = weak.update(cx, |this: &mut TuskApp, cx| match result {
                Ok(text) => done(this, text, cx),
                Err(e) => {
                    this.toast(false, format!("Script failed: {e}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(crate) fn copy_script(
        &mut self,
        kind: TableKind,
        name: String,
        which: Script,
        cx: &mut Context<Self>,
    ) {
        let label = which.label();
        self.fetch_script(kind, name.clone(), which, cx, move |this, text, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            this.toast(true, format!("Copied {label} script of {name}"));
            cx.notify();
        });
    }

    pub(crate) fn script_to_editor(
        &mut self,
        kind: TableKind,
        name: String,
        which: Script,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let handle = window.window_handle();
        self.fetch_script(kind, name, which, cx, move |_this, text, cx| {
            let view = cx.entity();
            cx.defer(move |cx| {
                let _ = handle.update(cx, |_, window, cx| {
                    view.update(cx, |app, cx| app.open_sql_tab_with(Some(text), window, cx));
                });
            });
        });
    }

    pub(crate) fn duplicate_object(
        &mut self,
        name: String,
        with_data: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(pool) = self.pool.clone() else {
            return;
        };
        let schema = self.current_schema.clone();
        self.status_line = format!("Duplicating {name}…");
        cx.notify();
        cx.spawn(async move |weak, cx: &mut AsyncApp| {
            let result = objects::duplicate_table(&pool, &schema, &name, with_data).await;
            let _ = weak.update(cx, |this: &mut TuskApp, cx| {
                this.status_line.clear();
                match result {
                    Ok(copy) => this.toast(true, format!("Created {copy}")),
                    Err(e) => this.toast(false, format!("Duplicate failed: {e}")),
                }
                this.fetch_objects_for(&schema, cx);
            });
        })
        .detach();
    }

    /// Asks first; TRUNCATE runs right away (not a pending ⌘S
    /// change) because it can't be previewed.
    pub(crate) fn truncate_object(
        &mut self,
        name: String,
        cascade: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pool) = self.pool.clone() else {
            return;
        };
        let schema = self.current_schema.clone();
        let answer = crate::dialog_keys::confirm(
            window,
            PromptLevel::Critical,
            &format!("Truncate “{name}”?"),
            Some(if cascade {
                "Deletes every row, and every row referencing it in other tables (CASCADE). This can't be undone."
            } else {
                "Deletes every row of the table. This can't be undone."
            }),
            "Truncate",
            cx,
        );
        cx.spawn(async move |weak, cx: &mut AsyncApp| {
            if !answer.await {
                return;
            }
            let sql = format!(
                "TRUNCATE TABLE {}.{}{}",
                db::quote_ident(&schema),
                db::quote_ident(&name),
                if cascade { " CASCADE" } else { "" }
            );
            let result = db::run_exec(&pool, &sql).await;
            let _ = weak.update(cx, |this: &mut TuskApp, cx| {
                match result {
                    Ok(_) => this.toast(true, format!("Truncated {name}")),
                    Err(e) => this.toast(false, format!("Truncate failed: {e}")),
                }
                this.reload_tabs_of(&schema, &name, cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Reload open grid tabs showing `schema.name` (after truncate / import).
    pub(super) fn reload_tabs_of(&mut self, schema: &str, name: &str, cx: &mut Context<Self>) {
        for tab in &self.tabs {
            if let WorkspaceTab::Grid(g) = tab
                && g.table.schema == schema
                && g.table.name == name
            {
                crate::grid::reload(&g.state, cx);
            }
        }
    }

    pub(crate) fn import_csv_into(
        &mut self,
        name: String,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
                Ok(data) => objects::import_csv(&pool, &schema, &name, data).await,
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
}

// ---- sidebar "+" menu and folders ----

impl TuskApp {
    fn group_scope(&self) -> String {
        format!(
            "{}/{}/{}",
            self.active_name,
            self.current_database(),
            self.current_schema
        )
    }

    pub(super) fn reload_obj_groups(&mut self) {
        self.obj_groups = objects::load_object_groups(&self.group_scope());
    }

    fn persist_obj_groups(&mut self, cx: &mut Context<Self>) {
        objects::save_object_groups(&self.group_scope(), &self.obj_groups);
        cx.notify();
    }

    /// Sidebar `+`: New Table / View / Function-Procedure / Group.
    /// Sidebar `+ ⌄`: "+" creates right away (a table, or a view in the
    /// Views panel); the chevron lists every kind of object.
    pub(super) fn sidebar_new_button(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let views = self.sidebar_panel == super::SidebarPanel::Views;
        let plus = Button::new("sidebar-new")
            .ghost()
            .xsmall()
            .child(Icon::new(IconName::Plus).size(px(13.)).text_color(muted))
            .tooltip(if views { "New View" } else { "New Table" })
            .on_click(cx.listener(move |this, _, window, cx| {
                if views {
                    this.new_view_editor(window, cx);
                } else {
                    this.new_table_editor(window, cx);
                }
            }));
        let more = Button::new("sidebar-new-menu")
            .ghost()
            .xsmall()
            .child(Icon::new(IconName::ChevronDown).size(px(11.)).text_color(muted))
            .dropdown_menu(|menu, _, _| {
                let function = PopupMenuItem::new("New Function/Procedure").on_click(|_, window, cx| {
                    with_app(cx, |app, cx| {
                        let s = db::quote_ident(&app.current_schema);
                        let text = format!(
                            "CREATE OR REPLACE FUNCTION {s}.\"new_function\"()\nRETURNS integer\nLANGUAGE plpgsql\nAS $$\nBEGIN\n    RETURN 1;\nEND;\n$$;\n\n-- Procedure:\n-- CREATE OR REPLACE PROCEDURE {s}.\"new_procedure\"()\n-- LANGUAGE plpgsql\n-- AS $$\n-- BEGIN\n-- END;\n-- $$;"
                        );
                        app.open_sql_tab_with(Some(text), window, cx);
                    });
                });
                menu.item(PopupMenuItem::new("New Table").on_click(|_, window, cx| {
                    with_app(cx, |app, cx| app.new_table_editor(window, cx));
                }))
                .item(PopupMenuItem::new("New View").on_click(|_, window, cx| {
                    with_app(cx, |app, cx| app.new_view_editor(window, cx));
                }))
                .item(function)
                    .separator()
                    .item(PopupMenuItem::new("New Group…").on_click(|_, window, cx| {
                        with_app(cx, |app, cx| app.new_obj_group(None, window, cx));
                    }))
            });
        div()
            .flex()
            .items_center()
            .child(plus)
            .child(more)
            .into_any_element()
    }

    /// New folder (optionally holding `member` right away), renamed inline.
    pub(crate) fn new_obj_group(
        &mut self,
        member: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut name = "New Group".to_string();
        let mut n = 2;
        while self.obj_groups.iter().any(|g| g.name == name) {
            name = format!("New Group {n}");
            n += 1;
        }
        if let Some(m) = &member {
            for g in &mut self.obj_groups {
                g.members.retain(|x| x != m);
            }
        }
        self.obj_groups.push(objects::ObjectGroup {
            name: name.clone(),
            members: member.into_iter().collect(),
        });
        self.persist_obj_groups(cx);
        self.start_obj_group_rename(name, window, cx);
    }

    fn start_obj_group_rename(
        &mut self,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| {
            let mut st = InputState::new(window, cx);
            st.set_value(name.clone(), window, cx);
            st
        });
        let sub = cx.subscribe_in(&input, window, |this, _, ev: &InputEvent, _, cx| match ev {
            InputEvent::PressEnter { .. } | InputEvent::Blur => this.commit_obj_group_rename(cx),
            _ => {}
        });
        input.read(cx).focus_handle(cx).focus(window, cx);
        self.obj_group_renaming = Some((name, input, sub));
        cx.notify();
    }

    fn commit_obj_group_rename(&mut self, cx: &mut Context<Self>) {
        let Some((old, input, _)) = self.obj_group_renaming.take() else {
            return;
        };
        let new = input.read(cx).value().trim().to_string();
        if !new.is_empty() && new != old && !self.obj_groups.iter().any(|g| g.name == new) {
            if let Some(g) = self.obj_groups.iter_mut().find(|g| g.name == old) {
                g.name = new.clone();
            }
            if self.obj_groups_collapsed.remove(&old) {
                self.obj_groups_collapsed.insert(new);
            }
        }
        self.persist_obj_groups(cx);
    }

    fn delete_obj_group(&mut self, name: String, cx: &mut Context<Self>) {
        self.obj_groups.retain(|g| g.name != name);
        self.persist_obj_groups(cx);
    }

    /// "Move to Group ▸": `group` None = back to the top level.
    fn move_obj_to_group(&mut self, member: String, group: Option<String>, cx: &mut Context<Self>) {
        for g in &mut self.obj_groups {
            g.members.retain(|x| *x != member);
        }
        if let Some(target) = group
            && let Some(g) = self.obj_groups.iter_mut().find(|g| g.name == target)
        {
            g.members.push(member);
        }
        self.persist_obj_groups(cx);
    }

    pub(super) fn obj_group_header(
        &self,
        g: &objects::ObjectGroup,
        count: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.theme();
        let (muted, fg) = (t.muted_foreground, t.foreground);
        let collapsed = self.obj_groups_collapsed.contains(&g.name);
        let editing = self
            .obj_group_renaming
            .as_ref()
            .filter(|(n, _, _)| *n == g.name)
            .map(|(_, input, _)| input.clone());
        let name = g.name.clone();
        let title: AnyElement = match editing {
            Some(input) => div()
                .flex_1()
                .child(
                    Input::new(&input)
                        .xsmall()
                        .h(px(crate::settings::row_h() - 2.)),
                )
                .into_any_element(),
            None => div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(crate::settings::ui_text()))
                .text_color(fg)
                .child(name.clone())
                .into_any_element(),
        };
        let (toggle, menu_name) = (name.clone(), name.clone());
        div()
            .id(SharedString::from(format!("obj-group-{name}")))
            .flex()
            .items_center()
            .gap_1p5()
            .px_2()
            .h(px(crate::settings::row_h()))
            .rounded(crate::theme::RADIUS_SM)
            .hover(|this| this.bg(muted.opacity(0.08)))
            .child(
                Icon::new(if collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .size(px(11.))
                .text_color(muted),
            )
            .child(Icon::new(IconName::Folder).size(px(13.)).text_color(muted))
            .child(title)
            .child(
                div()
                    .text_caption()
                    .text_color(muted)
                    .child(count.to_string()),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.obj_groups_collapsed.remove(&toggle) {
                    this.obj_groups_collapsed.insert(toggle.clone());
                }
                cx.notify();
            }))
            .context_menu(move |menu, _, _| {
                let (r, d) = (menu_name.clone(), menu_name.clone());
                menu.item(
                    PopupMenuItem::new("Rename Group").on_click(move |_, window, cx| {
                        let r = r.clone();
                        with_app(cx, |app, cx| app.start_obj_group_rename(r, window, cx));
                    }),
                )
                .item(crate::theme::danger_item(
                    "Delete Group",
                    move |_, _, cx| {
                        let d = d.clone();
                        with_app(cx, |app, cx| app.delete_obj_group(d, cx));
                    },
                ))
            })
            .into_any_element()
    }

    /// "Move to Group ▸" submenu for an object.
    pub(super) fn move_to_group_menu(
        member: String,
        window: &mut Window,
        cx: &mut Context<PopupMenu>,
    ) -> Entity<PopupMenu> {
        let groups: Vec<String> = {
            let view = cx.global::<TuskHandle>().0.clone();
            view.read(cx)
                .obj_groups
                .iter()
                .map(|g| g.name.clone())
                .collect()
        };
        PopupMenu::build(window, cx, move |mut m, _, _| {
            let m0 = member.clone();
            m = m.item(PopupMenuItem::new("No Group").on_click(move |_, _, cx| {
                let m0 = m0.clone();
                with_app(cx, |app, cx| app.move_obj_to_group(m0, None, cx));
            }));
            if !groups.is_empty() {
                m = m.separator();
            }
            for g in &groups {
                let (mm, gg) = (member.clone(), g.clone());
                m = m.item(PopupMenuItem::new(g.clone()).on_click(move |_, _, cx| {
                    let (mm, gg) = (mm.clone(), gg.clone());
                    with_app(cx, |app, cx| app.move_obj_to_group(mm, Some(gg), cx));
                }));
            }
            let mn = member.clone();
            m.separator().item(
                PopupMenuItem::new("New Group…").on_click(move |_, window, cx| {
                    let mn = mn.clone();
                    with_app(cx, |app, cx| app.new_obj_group(Some(mn), window, cx));
                }),
            )
        })
    }
}

// ---- New Table / New View designers ----

impl TuskApp {
    /// Structure / Index header: `Name [table]  Primary [id] [..]`. The name
    /// field names a new table or renames an existing one on ⌘S; for a
    /// table being designed the Primary chips open a column menu to pick
    /// the key.
    pub(super) fn render_table_header(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(tab) = self.grid_tab(ix) else {
            return div().into_any_element();
        };
        let t = cx.theme();
        let (muted, fg, border) = (t.muted_foreground, t.foreground, t.border);
        let chip_bg = t.accent.opacity(0.22);
        let designing = tab.draft.is_some();
        let (pk, columns): (Vec<String>, Vec<(usize, String, bool)>) = match &tab.structure {
            Some(st) => {
                let d = st.read(cx).delegate();
                let cols = d
                    .rows
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| !r.deleted && !r.name.is_empty())
                    .map(|(i, r)| (i, r.name.clone(), r.pk))
                    .collect();
                (d.pk_columns(), cols)
            }
            None => (
                tab.state
                    .read(cx)
                    .delegate()
                    .metas
                    .iter()
                    .filter(|m| m.is_pk)
                    .map(|m| m.name.clone())
                    .collect(),
                Vec::new(),
            ),
        };
        let label = |s: &'static str| div().flex_none().text_caption().text_color(muted).child(s);
        let chips = div()
            .flex()
            .items_center()
            .gap_1()
            .children(pk.iter().map(|c| {
                div()
                    .px_1p5()
                    .rounded(crate::theme::RADIUS_SM)
                    .bg(chip_bg)
                    .text_caption()
                    .font_family(crate::settings::table_font())
                    .text_color(fg)
                    .child(c.clone())
            }))
            .when(pk.is_empty(), |d| {
                d.child(
                    div()
                        .text_caption()
                        .italic()
                        .text_color(muted)
                        .child("none"),
                )
            });
        let primary: AnyElement = if designing {
            let st = tab.structure.clone();
            Button::new(("pk-menu", ix))
                .ghost()
                .xsmall()
                .child(chips)
                .dropdown_menu(move |mut menu, _, _| {
                    for (row, name, on) in columns.clone() {
                        let st = st.clone();
                        menu = menu.item(PopupMenuItem::new(name).checked(on).on_click(
                            move |_, _, cx| {
                                if let Some(st) = &st {
                                    st.update(cx, |st, cx| {
                                        st.delegate_mut().toggle_pk(row);
                                        cx.notify();
                                    });
                                }
                                with_app(cx, |_, cx| cx.notify());
                            },
                        ));
                    }
                    menu
                })
                .into_any_element()
        } else {
            chips.into_any_element()
        };
        let name_input = tab.draft.clone().or(tab.rename.clone());
        div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .h(px(crate::settings::bar_h() + 8.))
            .border_b_1()
            .border_color(border)
            .child(label("Name"))
            .children(name_input.map(|input| {
                div().w(px(240.)).child(
                    Input::new(&input)
                        .xsmall()
                        .font_family(crate::settings::table_font()),
                )
            }))
            .child(div().w(px(10.)))
            .child(label("Primary"))
            .child(primary)
            .when(designing, |d| {
                // One line, never squeezed: the hint yields to the name field.
                d.child(div().flex_1().min_w_0()).child(
                    div()
                        .flex_none()
                        .whitespace_nowrap()
                        .text_caption()
                        .text_color(muted)
                        .child(
                            crate::kbd::rich_colored("[cmd-s] creates the table", muted)
                                .flex_nowrap(),
                        ),
                )
            })
            .into_any_element()
    }

    /// `Label [name…]  hint` bar above a draft's editor.
    pub(super) fn draft_name_bar(
        label: &'static str,
        input: &Entity<InputState>,
        hint: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.theme();
        div()
            .flex()
            .items_center()
            .gap_3()
            .px_3()
            .h(px(crate::settings::bar_h() + 10.))
            .border_b_1()
            .border_color(t.border)
            .child(div().text_sm().text_color(t.muted_foreground).child(label))
            .child(
                div().w(px(260.)).child(
                    Input::new(input)
                        .small()
                        .font_family(crate::settings::table_font()),
                ),
            )
            .child(
                div()
                    .text_caption()
                    .text_color(t.muted_foreground)
                    .child(crate::kbd::rich_colored(hint, t.muted_foreground)),
            )
            .into_any_element()
    }

    fn draft_input(name: &str, window: &mut Window, cx: &mut Context<Self>) -> Entity<InputState> {
        let input = cx.new(|cx| {
            let mut st = InputState::new(window, cx);
            st.set_value(name.to_string(), window, cx);
            st
        });
        // Focused with the name selected: typing replaces it.
        input.read(cx).focus_handle(cx).focus(window, cx);
        input.update(cx, |st, cx| st.select_all(window, cx));
        input
    }

    /// New Table: a structure tab for a table that doesn't exist yet —
    /// name it, add / edit columns, ⌘S runs `CREATE TABLE` and it becomes a
    /// normal table tab.
    pub(crate) fn new_table_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pool) = self.pool.clone() else {
            return;
        };
        if !pool.caps().edit_structure {
            self.toast_info(format!(
                "Designing tables isn't available for {} yet — use CREATE TABLE in a query tab.",
                pool.engine().label()
            ));
            cx.notify();
            return;
        }
        let schema = self.current_schema.clone();
        // `untitled_table_N`: the first number no table or open draft uses.
        let drafts: Vec<String> = self
            .tabs
            .iter()
            .filter_map(|t| match t {
                WorkspaceTab::Grid(g) if g.draft.is_some() => Some(g.table.name.clone()),
                _ => None,
            })
            .collect();
        let name = (1..)
            .map(|n| format!("untitled_table_{n}"))
            .find(|n| !self.objects.tables.contains(n) && !drafts.contains(n))
            .unwrap_or_default();
        let table = TableRef {
            schema: schema.clone(),
            name: name.clone(),
            kind: TableKind::Table,
        };
        let id_type = pool.engine().new_table_id_type();
        let state = crate::grid::new_state(
            GridDelegate::new(pool, schema.clone(), name.clone(), true),
            window,
            cx,
        );
        let st = crate::structure::new_state(
            StructureDelegate::new_table(schema, name.clone(), id_type),
            window,
            cx,
        );
        let sub = cx.subscribe_in(&st, window, |_this, st, ev: &TableEvent, window, cx| {
            if let TableEvent::DoubleClickedCell(r, c) = ev {
                let (r, c) = (*r, *c);
                st.update(cx, |st, cx| st.delegate_mut().begin_edit(r, c, window, cx));
            }
        });
        let draft = Self::draft_input(&name, window, cx);
        // The sidebar's pending row and the tab title follow the name field.
        let name_sub = cx.subscribe(&draft, |_, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Change) {
                cx.notify();
            }
        });
        self.tabs.push(WorkspaceTab::Grid(DataTab {
            table,
            state,
            view: TabView::Structure,
            structure: Some(st),
            filters: FilterBar::default(),
            subs: vec![sub, name_sub],
            draft: Some(draft),
            rename: None,
            indexes: None,
            index_count: None,
            triggers: None,
            trigger_count: None,
            ddl: None,
            page_limit_input: Self::page_input(crate::grid::PAGE_LIMIT, window, cx),
            page_offset_input: Self::page_input(0, window, cx),
        }));
        self.activate_tab(self.tabs.len() - 1, cx);
        self.load_user_types(self.tabs.len() - 1, cx);
        cx.notify();
    }

    /// New View: a query tab with a name field; ⌘S runs
    /// `CREATE VIEW <name> AS <query>`.
    pub(crate) fn new_view_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let first = self.objects.tables.first().cloned();
        let body = match first {
            Some(t) => format!(
                "SELECT *\nFROM {}.{}",
                db::quote_ident(&self.current_schema),
                db::quote_ident(&t)
            ),
            None => "SELECT 1 AS \"one\"".to_string(),
        };
        self.open_sql_tab_with(Some(body), window, cx);
        let draft = Self::draft_input("new_view", window, cx);
        if let Some(ix) = self.active_tab
            && let Some(WorkspaceTab::Sql(tab)) = self.tabs.get_mut(ix)
        {
            tab.title = "New View".into();
            tab.view_draft = Some(draft);
        }
        cx.notify();
    }
}
