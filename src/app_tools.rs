//! Tools: Process List (the server's sessions, from each engine's own
//! catalog, with cancel / kill of the selected one) and Search in Database (a term in
//! every row of every table of the schema).

use gpui_kit::component::input::InputEvent;

use super::*;

impl TuskApp {
    /// ⌘.: open (or refresh) the process list sheet.
    /// Tools panels are one at a time: opening one closes the others (they
    /// used to stack, the new one hidden behind the old).
    pub(super) fn close_tool_panels(&mut self) {
        self.processes = None;
        self.users = None;
        self.db_search = None;
    }

    pub(super) fn open_process_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pool.is_none() {
            return;
        }
        if self.users.is_some() || self.db_search.is_some() {
            self.close_tool_panels();
        }
        let st = match self.processes.clone() {
            Some(st) => st,
            None => {
                let st = crate::sql::new_result_state(window, cx);
                self.processes = Some(st.clone());
                st
            }
        };
        self.load_processes(st, cx);
        cx.notify();
    }

    fn load_processes(
        &mut self,
        st: Entity<TableState<crate::sql::QueryDelegate>>,
        cx: &mut Context<Self>,
    ) {
        let Some(pool) = self.pool.clone() else {
            return;
        };
        cx.spawn(async move |weak, cx: &mut AsyncApp| {
            let rows = pool.driver().sessions().await;
            let _ = weak.update(cx, |this: &mut TuskApp, cx| {
                match rows {
                    Ok(rows) => {
                        let cols = crate::sql::infer_columns(&rows);
                        st.update(cx, |s, cx| {
                            s.delegate_mut()
                                .set_result(cols, crate::sql::rows_to_vec(rows), None);
                            s.refresh(cx);
                            cx.notify();
                        });
                    }
                    Err(e) => this.toast(false, format!("Process list: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Cancel the selected session's query, or terminate the session.
    fn signal_process(&mut self, kill: bool, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(pool), Some(st)) = (self.pool.clone(), self.processes.clone()) else {
            return;
        };
        let row = st
            .read(cx)
            .selected_cell()
            .map(|c| c.0)
            .or(st.read(cx).selected_row());
        let Some(pid) = row
            .map(|r| st.read(cx).delegate().cell_text_at(r, 0))
            .filter(|p| !p.is_empty())
        else {
            self.toast(false, "Select a process first.");
            cx.notify();
            return;
        };
        let what = if kill {
            "Kill session"
        } else {
            "Cancel the query of session"
        };
        let answer = window.prompt(
            if kill {
                PromptLevel::Critical
            } else {
                PromptLevel::Warning
            },
            &format!("{what} {pid}?"),
            None,
            &[if kill { "Kill" } else { "Cancel Query" }, "Keep"],
            cx,
        );
        cx.spawn(async move |weak, cx: &mut AsyncApp| {
            if answer.await != Ok(0) {
                return;
            }
            let r = pool.driver().signal_session(pid.clone(), kill).await;
            let _ = weak.update(cx, |this: &mut TuskApp, cx| {
                match r {
                    Ok(_) => this.toast(true, format!("Signalled session {pid}")),
                    Err(e) => this.toast(false, format!("{what} {pid}: {e}")),
                }
                this.load_processes(st.clone(), cx);
            });
        })
        .detach();
    }

    pub(super) fn render_process_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(st) = self.processes.clone() else {
            return div().into_any_element();
        };
        let t = cx.theme();
        let (border, bg, fg) = (t.border, t.popover, t.foreground);
        let backdrop = gpui_kit::black().opacity(if t.is_dark() { 0.45 } else { 0.2 });
        div()
            .id("process-backdrop")
            .absolute()
            .inset_0()
            .bg(backdrop)
            .flex()
            .items_start()
            .justify_center()
            .pt(px(56.))
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.processes = None;
                    cx.notify();
                }),
            )
            .child(
                div()
                    .id("process-card")
                    .key_context("ProcessList")
                    .w(px(960.))
                    .h(px(520.))
                    .flex()
                    .flex_col()
                    .rounded(crate::theme::RADIUS_LG)
                    .border_1()
                    .border_color(border)
                    .bg(bg)
                    .shadow_lg()
                    .overflow_hidden()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .h(px(44.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_4()
                            .border_b_1()
                            .border_color(border)
                            .child(
                                div()
                                    .flex_1()
                                    .text_sm()
                                    .text_color(fg)
                                    .child("Process List"),
                            )
                            .child(
                                Button::new("proc-refresh")
                                    .label("Refresh")
                                    .small()
                                    .outline()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_process_list(window, cx)
                                    })),
                            )
                            .child(
                                Button::new("proc-cancel")
                                    .label("Cancel Query")
                                    .small()
                                    .outline()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.signal_process(false, window, cx)
                                    })),
                            )
                            .child(
                                Button::new("proc-kill")
                                    .label("Kill")
                                    .small()
                                    .danger()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.signal_process(true, window, cx)
                                    })),
                            )
                            .child(
                                Button::new("proc-close")
                                    .icon(IconName::Close)
                                    .small()
                                    .ghost()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.processes = None;
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .child(crate::sql::result_element(&st)),
                    ),
            )
            .into_any_element()
    }
}

/// Tools ▸ Search in Database state.
pub(super) struct DbSearch {
    pub input: Entity<InputState>,
    /// (table, matching rows), tables with matches only.
    pub hits: Vec<(String, i64)>,
    pub running: bool,
    pub searched: Option<String>,
    _sub: Subscription,
}

impl TuskApp {
    pub(super) fn open_db_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pool.is_none() {
            return;
        }
        if let Some(s) = &self.db_search {
            s.input.read(cx).focus_handle(cx).focus(window, cx);
            return;
        }
        self.processes = None;
        self.users = None;
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Search every table for…"));
        let sub = cx.subscribe_in(&input, window, |this, _, ev: &InputEvent, _, cx| {
            if let InputEvent::PressEnter { .. } = ev {
                this.run_db_search(cx);
            }
        });
        // Focus after the current dispatch (opened from the palette, whose
        // closing hands focus back to the app).
        let handle = input.read(cx).focus_handle(cx);
        window.defer(cx, move |window, cx| handle.focus(window, cx));
        self.db_search = Some(DbSearch {
            input,
            hits: Vec::new(),
            running: false,
            searched: None,
            _sub: sub,
        });
        cx.notify();
    }

    fn run_db_search(&mut self, cx: &mut Context<Self>) {
        let (Some(pool), Some(s)) = (self.pool.clone(), self.db_search.as_mut()) else {
            return;
        };
        let term = s.input.read(cx).value().trim().to_string();
        if term.is_empty() {
            return;
        }
        s.running = true;
        s.hits.clear();
        s.searched = Some(term.clone());
        let schema = self.current_schema.clone();
        let mut tables = self.objects.tables.clone();
        tables.extend(self.objects.views.iter().cloned());
        tables.extend(self.objects.matviews.iter().cloned());
        cx.notify();
        cx.spawn(async move |weak, cx: &mut AsyncApp| {
            let pattern = format!(
                "%{}%",
                term.replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_")
            );
            let mut hits = Vec::new();
            for t in tables {
                let Some(pred) = search_predicate(&pool, &schema, &t, &pattern).await else {
                    continue;
                };
                let sql = format!(
                    "SELECT COUNT(*) AS n FROM {} t WHERE {pred}",
                    pool.qualified(&schema, &t)
                );
                if let Ok(rows) = db::run_query_rows(&pool, &sql, 1).await
                    && let Some(n) = rows.first().and_then(|r| r.get("n")).and_then(|v| {
                        v.as_i64()
                            .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
                    })
                    && n > 0
                {
                    hits.push((t, n));
                }
            }
            let _ = weak.update(cx, |this: &mut TuskApp, cx| {
                if let Some(s) = this.db_search.as_mut() {
                    s.hits = hits;
                    s.running = false;
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// A hit: its matching rows in a query tab.
    fn open_search_hit(&mut self, table: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(term) = self.db_search.as_ref().and_then(|s| s.searched.clone()) else {
            return;
        };
        let Some(pool) = self.pool.clone() else {
            return;
        };
        let schema = self.current_schema.clone();
        self.db_search = None;
        cx.spawn_in(window, async move |this, cx| {
            let pattern = format!("%{term}%");
            let Some(pred) = search_predicate(&pool, &schema, &table, &pattern).await else {
                return;
            };
            let sql = format!(
                "SELECT *\nFROM {} t\nWHERE {pred};",
                pool.qualified(&schema, &table)
            );
            let _ = this.update_in(cx, |this, window, cx| {
                this.open_sql_tab_with(Some(sql), window, cx);
                if let Some(ix) = this.active_tab {
                    this.run_sql_in_tab(ix, crate::sql::RunScope::All, cx);
                }
            });
        })
        .detach();
    }

    pub(super) fn render_db_search(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(s) = &self.db_search else {
            return div().into_any_element();
        };
        let t = cx.theme();
        let (border, bg, fg, muted) = (t.border, t.popover, t.foreground, t.muted_foreground);
        let backdrop = gpui_kit::black().opacity(if t.is_dark() { 0.45 } else { 0.2 });
        let status = if s.running {
            "Searching…".to_string()
        } else {
            match &s.searched {
                None => "Enter searches the rows of every table and view in this schema.".into(),
                Some(q) if s.hits.is_empty() => format!("No rows contain “{q}”."),
                Some(q) => format!(
                    "“{q}” found in {} table{}",
                    s.hits.len(),
                    if s.hits.len() == 1 { "" } else { "s" }
                ),
            }
        };
        div()
            .id("dbsearch-backdrop")
            .absolute()
            .inset_0()
            .bg(backdrop)
            .flex()
            .items_start()
            .justify_center()
            .pt(px(56.))
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.db_search = None;
                    cx.notify();
                }),
            )
            .child(
                div()
                    .id("dbsearch-card")
                    .key_context("DbSearch")
                    .w(px(560.))
                    .max_h(px(520.))
                    .flex()
                    .flex_col()
                    .rounded(crate::theme::RADIUS_LG)
                    .border_1()
                    .border_color(border)
                    .bg(bg)
                    .shadow_lg()
                    .overflow_hidden()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        div().p_3().border_b_1().border_color(border).child(
                            Input::new(&s.input).small().prefix(
                                Icon::new(IconName::Search).size(px(12.)).text_color(muted),
                            ),
                        ),
                    )
                    .child(
                        div()
                            .px_3()
                            .py_2()
                            .text_caption()
                            .text_color(muted)
                            .child(status),
                    )
                    .child(
                        div()
                            .id("dbsearch-hits")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .pb_2()
                            .children(s.hits.iter().enumerate().map(|(i, (table, n))| {
                                let table_c = table.clone();
                                div()
                                    .id(("dbsearch-hit", i))
                                    .mx_2()
                                    .px_2()
                                    .h(px(crate::settings::row_h()))
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .rounded(crate::theme::RADIUS_SM)
                                    .hover(|d| d.bg(muted.opacity(0.1)))
                                    .child(DbIcon::Table.icon_px(14.))
                                    .child(
                                        div()
                                            .flex_1()
                                            .text_size(px(crate::settings::ui_text()))
                                            .text_color(fg)
                                            .child(table.clone()),
                                    )
                                    .child(div().text_caption().text_color(muted).child(format!(
                                        "{n} row{}",
                                        if *n == 1 { "" } else { "s" }
                                    )))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.open_search_hit(table_c.clone(), window, cx)
                                    }))
                            })),
                    ),
            )
            .into_any_element()
    }
}

/// "Contains `pattern`" over a whole row: Postgres casts the row to text,
/// other engines OR a case-insensitive match over every column.
async fn search_predicate(
    pool: &db::Db,
    schema: &str,
    table: &str,
    pattern: &str,
) -> Option<String> {
    let d = pool.dialect();
    if d == crate::engine::Dialect::Postgres && pool.pg().is_some() {
        return Some(format!("t::text ILIKE {}", d.literal(pattern)));
    }
    let cols = db::fetch_columns(pool, schema, table).await.ok()?;
    if cols.is_empty() {
        return None;
    }
    let lit = d.literal(pattern);
    Some(
        cols.iter()
            .map(|c| d.ilike(&d.text(&format!("t.{}", d.quote(&c.name))), &lit, false))
            .collect::<Vec<_>>()
            .join(" OR "),
    )
}
