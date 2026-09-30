//! Tab / connection commands: next / previous tab, tab by number, reload,
//! reconnect, close all, discard and preview of pending changes.

use super::*;

impl TuskApp {
    /// ⌘] / ⌘[: the next / previous tab, wrapping around.
    pub(super) fn cycle_tab(&mut self, step: isize, cx: &mut Context<Self>) {
        let n = self.tabs.len();
        if n == 0 {
            return;
        }
        let cur = self.active_tab.unwrap_or(0) as isize;
        let next = (cur + step).rem_euclid(n as isize) as usize;
        self.activate_tab(next, cx);
    }

    /// ⌘1–⌘9 in the workspace: that tab (⌘9 = the last one).
    pub(super) fn tab_at(&mut self, n: usize, cx: &mut Context<Self>) {
        if self.tabs.is_empty() {
            return;
        }
        let ix = if n >= 8 {
            self.tabs.len() - 1
        } else {
            n.min(self.tabs.len() - 1)
        };
        self.activate_tab(ix, cx);
    }

    /// ⌘R: re-list the schema's objects and reload the active tab.
    pub(super) fn reload_workspace(&mut self, cx: &mut Context<Self>) {
        let schema = self.current_schema.clone();
        self.fetch_objects_for(&schema, cx);
        self.refresh_active_tab(cx);
        self.toast_info("Reloaded");
    }

    /// ⇧⌘R: open a fresh connection with the same profile; tabs stay.
    pub(super) fn reconnect(&mut self, cx: &mut Context<Self>) {
        let Some((conn, password)) = self.active_conn.clone() else {
            return;
        };
        self.status_line = format!("Reconnecting to {}…", conn.name);
        cx.notify();
        cx.spawn(async move |weak, cx: &mut AsyncApp| {
            let result = db::connect(conn.clone(), password, None).await;
            let _ = weak.update(cx, |this: &mut TuskApp, cx| {
                this.status_line.clear();
                match result {
                    Ok(c) => {
                        db::set_engine(c.pool.engine());
                        this.pool = Some(c.pool.clone());
                        this.tunnel = c.tunnel;
                        // Tabs keep their own pool handle: point them at the new one.
                        for tab in &this.tabs {
                            if let WorkspaceTab::Grid(g) = tab {
                                g.state
                                    .update(cx, |st, _| st.delegate_mut().pool = c.pool.clone());
                            }
                        }
                        let schema = this.current_schema.clone();
                        this.fetch_objects_for(&schema, cx);
                        this.refresh_active_tab(cx);
                        this.toast(true, format!("Reconnected to {}", conn.name));
                    }
                    Err(e) => this.toast(false, format!("Reconnect failed: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// ⇧⌘D: split the tab area (the other pane takes the previous tab, or a
    /// new query); again: back to one pane with the focused tab.
    pub(super) fn toggle_split(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.split.take().is_some() {
            cx.notify();
            return;
        }
        let Some(cur) = self.active_tab else { return };
        let other = self
            .nav_back
            .iter()
            .rev()
            .copied()
            .find(|&i| i != cur && i < self.tabs.len())
            .or_else(|| (0..self.tabs.len()).find(|&i| i != cur));
        let other = match other {
            Some(o) => o,
            None => {
                self.open_sql_tab(window, cx);
                let Some(new) = self.active_tab.filter(|&n| n != cur) else {
                    return;
                };
                new
            }
        };
        self.split = Some((cur, other));
        self.split_focus = 1;
        self.active_tab = Some(other);
        cx.notify();
    }

    /// Clicking into a pane (or ⌥⌘] / ⌥⌘[) makes its tab the active one.
    pub(super) fn focus_pane(&mut self, p: usize, cx: &mut Context<Self>) {
        let Some((l, r)) = self.split else { return };
        if self.split_focus == p && self.active_tab == Some(if p == 0 { l } else { r }) {
            return;
        }
        self.split_focus = p;
        self.active_tab = Some(if p == 0 { l } else { r });
        cx.notify();
    }

    /// Close every tab.
    pub(super) fn close_all_tabs(&mut self, cx: &mut Context<Self>) {
        self.split = None;
        self.tabs.clear();
        self.active_tab = None;
        self.nav_back.clear();
        self.nav_fwd.clear();
        self.row_panel.detail = None;
        cx.notify();
    }

    /// ⇧⌘⌫: drop every pending change of the active tab and the sidebar.
    pub(super) fn discard_changes(&mut self, cx: &mut Context<Self>) {
        let mut n = self.pending_drops.len() + self.pending_renames.len();
        self.pending_drops.clear();
        self.pending_renames.clear();
        match self.active_tab.and_then(|ix| self.tabs.get(ix)) {
            Some(WorkspaceTab::Grid(g)) => {
                n += self.active_tab.map_or(0, |ix| self.tab_pending(ix, cx));
                g.state.update(cx, |st, cx| {
                    st.delegate_mut().cancel_edit(cx);
                    st.delegate_mut().discard_changes();
                    cx.notify();
                });
                if let Some(st) = &g.structure {
                    st.update(cx, |st, cx| {
                        st.delegate_mut().cancel_edit(cx);
                        st.delegate_mut().discard_changes();
                        cx.notify();
                    });
                }
                if let Some(st) = &g.indexes {
                    st.update(cx, |st, cx| {
                        st.delegate_mut().cancel_edit(cx);
                        st.delegate_mut().discard_changes();
                        cx.notify();
                    });
                }
            }
            Some(WorkspaceTab::Sql(t)) => {
                n += t.result.read(cx).delegate().pending_count();
                t.result.update(cx, |st, cx| {
                    st.delegate_mut().discard_changes();
                    cx.notify();
                });
            }
            None => {}
        }
        if n == 0 {
            self.toast_info("No changes to discard.");
        } else {
            self.toast_info(format!(
                "Discarded {n} change{}",
                if n == 1 { "" } else { "s" }
            ));
        }
        cx.notify();
    }

    /// The SQL ⌘S would run for the active tab (and the sidebar), without
    /// running it.
    pub(super) fn pending_sql(&self, cx: &App) -> Vec<String> {
        let q = db::quote_ident;
        let schema = &self.current_schema;
        let mut out: Vec<String> = Vec::new();
        for (kind, name) in &self.pending_drops {
            out.push(format!(
                "DROP {} {}.{}",
                kind.ddl_keyword(),
                q(schema),
                q(name)
            ));
        }
        for ((kind, old), new) in &self.pending_renames {
            out.push(format!(
                "ALTER {} {}.{} RENAME TO {}",
                kind.ddl_keyword(),
                q(schema),
                q(old),
                q(new)
            ));
        }
        let fmt = |s: &Stmt| {
            if s.params.is_empty() {
                s.sql.clone()
            } else {
                let vals: Vec<String> = s
                    .params
                    .iter()
                    .map(|p| {
                        p.as_deref()
                            .map(db::quote_literal)
                            .unwrap_or_else(|| "NULL".into())
                    })
                    .collect();
                format!("{}  -- {}", s.sql, vals.join(", "))
            }
        };
        match self.active_tab.and_then(|ix| self.tabs.get(ix)) {
            Some(WorkspaceTab::Grid(g)) => {
                out.extend(
                    g.state
                        .read(cx)
                        .delegate()
                        .save_statements()
                        .iter()
                        .map(fmt),
                );
                if let Some(st) = &g.structure {
                    out.extend(st.read(cx).delegate().save_statements().iter().map(fmt));
                }
                if let Some(st) = &g.indexes {
                    out.extend(st.read(cx).delegate().save_statements().iter().map(fmt));
                }
            }
            Some(WorkspaceTab::Sql(t)) => {
                out.extend(
                    t.result
                        .read(cx)
                        .delegate()
                        .save_statements()
                        .iter()
                        .map(fmt),
                );
            }
            None => {}
        }
        out
    }

    /// ⌥⌘P: a sheet with the pending SQL; Commit runs it (⌘S).
    pub(super) fn preview_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let sql = self.pending_sql(cx);
        if sql.is_empty() {
            self.toast_info("No changes to preview.");
            cx.notify();
            return;
        }
        let text = sql
            .iter()
            .map(|s| format!("{};", s.trim_end_matches(';')))
            .collect::<Vec<_>>()
            .join("\n\n");
        let editor = cx.new(|cx| {
            let mut st = gpui_kit::component::input::EditorState::new(window, cx)
                .language("sql")
                .line_number(true)
                .soft_wrap(true)
                .default_value(text);
            st.set_readonly(true, cx);
            st
        });
        // Focus the sheet so Escape closes it (the grid would take Escape).
        editor.read(cx).focus_handle(cx).focus(window, cx);
        self.preview = Some(editor);
        cx.notify();
    }

    pub(super) fn render_preview(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(editor) = self.preview.clone() else {
            return div().into_any_element();
        };
        let t = cx.theme();
        let (border, bg, fg) = (t.border, t.popover, t.foreground);
        let backdrop = gpui_kit::black().opacity(if t.is_dark() { 0.45 } else { 0.2 });
        div()
            .id("preview-backdrop")
            .absolute()
            .inset_0()
            .bg(backdrop)
            .flex()
            .items_start()
            .justify_center()
            .pt(px(64.))
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.preview = None;
                    cx.notify();
                }),
            )
            .child(
                div()
                    .id("preview-card")
                    .key_context("PreviewSheet")
                    .w(px(720.))
                    .h(px(440.))
                    .flex()
                    .flex_col()
                    .rounded(crate::theme::RADIUS_LG)
                    .border_1()
                    .border_color(border)
                    .bg(bg)
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .h(px(40.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .px_4()
                            .border_b_1()
                            .border_color(border)
                            .text_sm()
                            .text_color(fg)
                            .child("Preview changes"),
                    )
                    .child(
                        div().flex_1().min_h_0().child(
                            gpui_kit::component::input::Editor::new(&editor)
                                .bordered(false)
                                .h_full(),
                        ),
                    )
                    .child(
                        div()
                            .h(px(48.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_end()
                            .gap_2()
                            .px_4()
                            .border_t_1()
                            .border_color(border)
                            .child(
                                Button::new("preview-cancel")
                                    .label("Cancel")
                                    .small()
                                    .outline()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.preview = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("preview-commit")
                                    .label("Commit")
                                    .small()
                                    .primary()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.preview = None;
                                        this.save_changes(window, cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }
}
