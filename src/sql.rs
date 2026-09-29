//! SQL editor tabs (Phase 7): Tree-Sitter editor + row_to_json results.
//!
//! SELECT-like statements render in the shared cell renderer (grid::render_value)
//! inside an in-memory QueryDelegate. Mutations/DDL report affected/OK.
//! Errors show the real Postgres message (db::pg_error_message), diagnostic
//! style — never a modal.
//!
//! Results are editable like a table grid when they come from one base table
//! with its primary key selected ([`db::EditSource`]): double-click edits a
//! cell (row orange), Delete marks the row (red), ⌘S writes UPDATE/DELETE by
//! primary key in one transaction and re-runs the query.

use std::collections::{BTreeMap, BTreeSet};

use crate::cell_edit::{self, CellEditHost, CellEditor};
use crate::db::{EditSource, Stmt};
use crate::grid;
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::table::{Column, ColumnSort, DataTable, TableDelegate, TableState};
use gpui_kit::component::theme::ActiveTheme as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde_json::Value;

/// Max rows fetched for one SELECT (100k-row acceptance query fits).
pub const QUERY_ROW_LIMIT: i64 = 100_000;

#[derive(Clone)]
pub struct QCol {
    pub name: String,
    pub right: bool,
}

#[derive(Clone)]
pub struct QueryOutput {
    pub columns: Vec<QCol>,
    pub rows: Vec<Vec<Value>>,
    pub truncated: bool,
    pub message: Option<String>,
    pub error: Option<String>,
    pub ms: u128,
    /// Why the result is read-only (None = editable or not a result set).
    pub edit_note: Option<String>,
}

impl QueryOutput {
    pub fn empty() -> Self {
        Self {
            columns: Vec::new(),
            rows: Vec::new(),
            truncated: false,
            message: Some("Run a query with [cmd-enter]".to_string()),
            error: None,
            ms: 0,
            edit_note: None,
        }
    }

    pub fn running() -> Self {
        Self {
            message: Some("Running…".to_string()),
            ..Self::empty_message()
        }
    }

    fn empty_message() -> Self {
        Self {
            columns: Vec::new(),
            rows: Vec::new(),
            truncated: false,
            message: None,
            error: None,
            ms: 0,
            edit_note: None,
        }
    }
}

/// What ⌘↵ / ⇧⌘↵ run: a selection always wins.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RunScope {
    /// The statement under the cursor.
    Current,
    /// The whole editor.
    All,
    /// Re-run the last executed SQL (refresh, after ⌘S).
    Last,
}

/// One SELECT's rows from a run; a script yields one per SELECT
/// .
#[derive(Clone)]
pub struct ResultSet {
    pub output: QueryOutput,
    pub source: Option<EditSource>,
}

pub struct SqlTab {
    pub title: String,
    pub editor: Entity<gpui_kit::component::input::EditorState>,
    pub result: Entity<TableState<QueryDelegate>>,
    pub output: QueryOutput,
    /// Every result set of the last run; `active_result` is in the grid.
    pub results: Vec<ResultSet>,
    pub active_result: usize,
    /// The SQL the last run executed (refresh re-runs exactly this).
    pub last_sql: Option<String>,
    /// New View being designed: its name field (⌘S runs CREATE VIEW).
    pub view_draft: Option<Entity<gpui_kit::component::input::InputState>>,
    /// Editor pane height (drag the edge above the run bar).
    pub editor_h: f32,
    pub running: bool,
    /// Language-server document (completions + diagnostics); None when the
    /// server couldn't start.
    pub doc: Option<crate::lsp::SqlDocument>,
    pub subs: Vec<Subscription>,
    /// File ▸ Open / Save As: the .sql file this tab is tied to.
    pub file: Option<std::path::PathBuf>,
}

impl SqlTab {
    pub fn new(
        title: String,
        editor: Entity<gpui_kit::component::input::EditorState>,
        result: Entity<TableState<QueryDelegate>>,
    ) -> Self {
        Self {
            title,
            editor,
            result,
            output: QueryOutput::empty(),
            results: Vec::new(),
            active_result: 0,
            last_sql: None,
            view_draft: None,
            editor_h: 220.,
            running: false,
            doc: None,
            subs: Vec::new(),
            file: None,
        }
    }
}

/// In-memory result grid. Sorting stays off v1 (SIMPLIFIED: result sets are
/// already ordered by the query; client-side re-sort is a follow-up).
pub struct QueryDelegate {
    columns: Vec<QCol>,
    rows: Vec<Vec<Value>>,
    /// Set when the result can be written back (see module docs).
    pub source: Option<EditSource>,
    /// Pending cell changes by row → column (`None` = NULL).
    pub edits: BTreeMap<usize, BTreeMap<usize, Option<String>>>,
    pub deleted: BTreeSet<usize>,
    editing: Option<CellEditor>,
    history: crate::undo::History<ResultSnapshot>,
    /// Content-fitted column widths (set with the rows).
    widths: Vec<Pixels>,
}

type ResultSnapshot = (
    BTreeMap<usize, BTreeMap<usize, Option<String>>>,
    BTreeSet<usize>,
);

impl QueryDelegate {
    pub fn empty() -> Self {
        Self {
            columns: Vec::new(),
            rows: Vec::new(),
            source: None,
            edits: BTreeMap::new(),
            deleted: BTreeSet::new(),
            editing: None,
            history: Default::default(),
            widths: Vec::new(),
        }
    }

    pub fn set_result(
        &mut self,
        columns: Vec<QCol>,
        rows: Vec<Vec<Value>>,
        source: Option<EditSource>,
    ) {
        self.widths = columns
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let texts: Vec<String> = rows
                    .iter()
                    .take(200)
                    .map(|r| r.get(i).map(grid::cell_text).unwrap_or_default())
                    .collect();
                crate::widths::auto_width(&c.name, texts.iter().map(String::as_str))
            })
            .collect();
        self.columns = columns;
        self.rows = rows;
        self.source = source;
        self.discard_changes();
    }

    pub fn discard_changes(&mut self) {
        self.edits.clear();
        self.deleted.clear();
        self.editing = None;
        self.history.clear();
    }

    fn snapshot(&self) -> ResultSnapshot {
        (self.edits.clone(), self.deleted.clone())
    }

    pub fn undo(&mut self, redo: bool) -> bool {
        let current = self.snapshot();
        let target = if redo {
            self.history.redo(current)
        } else {
            self.history.undo(current)
        };
        let Some((edits, deleted)) = target else {
            return false;
        };
        self.editing = None;
        self.edits = edits;
        self.deleted = deleted;
        true
    }

    pub fn pending_count(&self) -> usize {
        self.deleted.len()
            + self
                .edits
                .keys()
                .filter(|r| !self.deleted.contains(r))
                .count()
    }

    fn editable_col(&self, col_ix: usize) -> bool {
        self.source
            .as_ref()
            .is_some_and(|s| s.columns.get(col_ix).is_some_and(Option::is_some))
    }

    fn display_value(&self, row_ix: usize, col_ix: usize) -> Option<Value> {
        if let Some(change) = self.edits.get(&row_ix).and_then(|c| c.get(&col_ix)) {
            return Some(match change {
                Some(s) => Value::String(s.clone()),
                None => Value::Null,
            });
        }
        self.rows.get(row_ix).and_then(|r| r.get(col_ix)).cloned()
    }

    pub fn is_row_deleted(&self, row_ix: usize) -> bool {
        self.deleted.contains(&row_ix)
    }

    pub fn begin_edit(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        if !self.editable_col(col_ix) || row_ix >= self.rows.len() || self.deleted.contains(&row_ix)
        {
            return;
        }
        if self
            .editing
            .as_ref()
            .is_some_and(|e| e.row == row_ix && e.col == col_ix)
        {
            return;
        }
        self.commit_edit(cx);
        let current = self.display_value(row_ix, col_ix);
        let meta = self
            .source
            .as_ref()
            .and_then(|s| s.columns.get(col_ix).cloned().flatten());
        self.editing = Some(CellEditor::new(
            row_ix,
            col_ix,
            meta.as_ref(),
            current.as_ref(),
            window,
            cx,
        ));
        cx.notify();
    }

    pub fn commit_edit(&mut self, cx: &mut Context<TableState<Self>>) {
        let Some(ed) = self.editing.take() else {
            return;
        };
        let orig = self.rows.get(ed.row).and_then(|r| r.get(ed.col)).cloned();
        let change = cell_edit::change_for(ed.value(cx), orig.as_ref());
        self.set_change(ed.row, ed.col, change);
        cx.notify();
    }

    pub fn cancel_edit(&mut self, cx: &mut Context<TableState<Self>>) -> bool {
        if self.editing.take().is_some() {
            cx.notify();
            true
        } else {
            false
        }
    }

    fn set_change(&mut self, row: usize, col: usize, change: Option<Option<String>>) {
        let before = self.snapshot();
        self.apply_change(row, col, change);
        let after = self.snapshot();
        self.history.record(before, &after);
    }

    fn apply_change(&mut self, row: usize, col: usize, change: Option<Option<String>>) {
        match change {
            Some(v) => {
                self.edits.entry(row).or_default().insert(col, v);
            }
            None => {
                if let Some(cols) = self.edits.get_mut(&row) {
                    cols.remove(&col);
                    if cols.is_empty() {
                        self.edits.remove(&row);
                    }
                }
            }
        }
    }

    pub fn set_null(&mut self, row_ix: usize, col_ix: usize) {
        if !self.editable_col(col_ix) {
            return;
        }
        let was_null = self
            .rows
            .get(row_ix)
            .and_then(|r| r.get(col_ix))
            .is_none_or(Value::is_null);
        self.set_change(row_ix, col_ix, (!was_null).then_some(None));
    }

    pub fn toggle_delete(&mut self, row_ix: usize) {
        if self.source.is_none() || row_ix >= self.rows.len() {
            return;
        }
        if self.editing.as_ref().is_some_and(|e| e.row == row_ix) {
            self.editing = None;
        }
        let before = self.snapshot();
        if !self.deleted.remove(&row_ix) {
            self.deleted.insert(row_ix);
        }
        let after = self.snapshot();
        self.history.record(before, &after);
    }

    /// UPDATE/DELETE by primary key for every pending change (deletes first).
    pub fn save_statements(&self) -> Vec<Stmt> {
        let Some(src) = &self.source else {
            return Vec::new();
        };
        // The connected engine's quoting / parameters (Postgres casts its
        // text parameters to the column type, `?` elsewhere).
        let d = src.engine.dialect();
        let target = crate::ddl::target(src.engine, &src.schema, &src.table);
        let key_pred = |row: usize, params: &mut Vec<Option<String>>| {
            src.key
                .iter()
                .filter_map(|ix| {
                    let meta = src.columns.get(*ix)?.as_ref()?;
                    params.push(
                        self.rows
                            .get(row)
                            .and_then(|r| r.get(*ix))
                            .map(grid::cell_text),
                    );
                    Some(format!(
                        "{} = {}",
                        d.quote(&meta.name),
                        d.param(params.len(), Some(&meta.sql_type))
                    ))
                })
                .collect::<Vec<_>>()
                .join(" AND ")
        };
        let mut out = Vec::new();
        for row in &self.deleted {
            let mut params = Vec::new();
            let pred = key_pred(*row, &mut params);
            out.push(Stmt {
                sql: format!("DELETE FROM {target} WHERE {pred}"),
                params,
            });
        }
        for (row, cols) in &self.edits {
            if self.deleted.contains(row) || cols.is_empty() {
                continue;
            }
            let mut params = Vec::new();
            let sets: Vec<String> = cols
                .iter()
                .filter_map(|(col, v)| {
                    let meta = src.columns.get(*col)?.as_ref()?;
                    params.push(v.clone());
                    Some(format!(
                        "{} = {}",
                        d.quote(&meta.name),
                        d.param(params.len(), Some(&meta.sql_type))
                    ))
                })
                .collect();
            let pred = key_pred(*row, &mut params);
            out.push(Stmt {
                sql: format!("UPDATE {target} SET {} WHERE {pred}", sets.join(", ")),
                params,
            });
        }
        out
    }
}

/// Right-click menu of one editable result cell.
fn result_cell_menu(
    entity: &Entity<TableState<QueryDelegate>>,
    row_ix: usize,
    col_ix: usize,
    deleted: bool,
    menu: PopupMenu,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    use crate::copy_as::{CopyFormat, Target, format_rows};
    let d = entity.read(cx).delegate();
    let editable = d.editable_col(col_ix);
    let col_name = d
        .columns
        .get(col_ix)
        .map(|c| c.name.clone())
        .unwrap_or_default();
    let e = entity.clone();
    let copy_menu = PopupMenu::build(window, cx, move |mut m, _, _| {
        for fmt in CopyFormat::ALL {
            let e = e.clone();
            m = m.item(PopupMenuItem::new(fmt.label()).on_click(move |_, _, cx| {
                let d = e.read(cx).delegate();
                let cols: Vec<String> = d.columns.iter().map(|c| c.name.clone()).collect();
                let row: Vec<Value> = (0..cols.len())
                    .map(|c| d.display_value(row_ix, c).unwrap_or(Value::Null))
                    .collect();
                let target = d.source.as_ref().map(|src| Target {
                    schema: &src.schema,
                    table: &src.table,
                    key: &src.key,
                });
                let out = format_rows(fmt, &cols, &[row], target.as_ref(), fmt != CopyFormat::Json);
                cx.write_to_clipboard(ClipboardItem::new_string(out));
            }));
        }
        m
    });
    let mut menu = menu;
    if editable {
        let e = entity.clone();
        menu = menu.item(
            PopupMenuItem::new("Edit Cell").on_click(move |_, window, cx| {
                e.update(cx, |st, cx| {
                    st.set_selected_cell(row_ix, col_ix, cx);
                    st.delegate_mut().begin_edit(row_ix, col_ix, window, cx);
                });
            }),
        );
        let e = entity.clone();
        menu = menu
            .item(PopupMenuItem::new("Set NULL").on_click(move |_, _, cx| {
                e.update(cx, |st, cx| {
                    st.delegate_mut().set_null(row_ix, col_ix);
                    cx.notify();
                });
            }))
            .separator();
    }
    let e = entity.clone();
    menu = menu
        .item(PopupMenuItem::new("Copy Value").on_click(move |_, _, cx| {
            let text = e.read(cx).delegate().cell_text_at(row_ix, col_ix);
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }))
        .item(
            PopupMenuItem::new("Copy Column Name").on_click(move |_, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(col_name.clone()));
            }),
        )
        .item(PopupMenuItem::submenu("Copy Row As", copy_menu))
        .item({
            let e = entity.clone();
            PopupMenuItem::new("Send Row to Chat")
                .icon(gpui_kit::assets::IconName::Sparkles)
                .on_click(move |_, window, cx| {
                    let d = e.read(cx).delegate();
                    let cols: Vec<String> = d.columns.iter().map(|c| c.name.clone()).collect();
                    let row: Vec<Value> = (0..cols.len())
                        .map(|c| d.display_value(row_ix, c).unwrap_or(Value::Null))
                        .collect();
                    let json =
                        crate::copy_as::format_rows(CopyFormat::Json, &cols, &[row], None, false);
                    let view = cx.global::<crate::app::TuskHandle>().0.clone();
                    view.update(cx, |app, cx| {
                        app.send_to_chat("Result row", json, "json", window, cx)
                    });
                })
        });
    if editable {
        let e = entity.clone();
        menu = menu.separator().item(
            PopupMenuItem::new(if deleted {
                "Undo Delete Row"
            } else {
                "Delete Row"
            })
            .on_click(move |_, _, cx| {
                e.update(cx, |st, cx| {
                    st.delegate_mut().toggle_delete(row_ix);
                    cx.notify();
                });
            }),
        );
    }
    menu
}

impl QueryDelegate {
    pub fn cell_text_at(&self, row_ix: usize, col_ix: usize) -> String {
        self.rows
            .get(row_ix)
            .and_then(|r| r.get(col_ix))
            .map(grid::cell_text)
            .unwrap_or_default()
    }
}

impl CellEditHost for QueryDelegate {
    fn commit_edit(&mut self, cx: &mut Context<TableState<Self>>) {
        QueryDelegate::commit_edit(self, cx);
    }
}

impl TableDelegate for QueryDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _cx: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _cx: &App) -> Column {
        let col = &self.columns[col_ix];
        let mut c = Column::new(col.name.clone(), col.name.clone());
        // Cells pad themselves (px_2): tints / editors / frames fill edge to edge.
        c = c.p_0();
        if col.right {
            c.align = gpui_kit::TextAlign::Right;
        }
        c.width = self.widths.get(col_ix).copied().unwrap_or(px(180.));
        c.resizable = true;
        c.sort = None;
        c
    }

    fn render_last_empty_col(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        // No filler column after the last one.
        div()
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        div()
            .when(self.columns[col_ix].right, |this| {
                this.w_full().text_right()
            })
            .px_2()
            .text_size(px(crate::settings::table_text()))
            .font_weight(FontWeight::MEDIUM)
            .font_family(crate::settings::table_font())
            .text_color(cx.theme().foreground)
            .child(self.columns[col_ix].name.clone())
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        let tr = div().id(("result-row", row_ix));
        if self.deleted.contains(&row_ix) {
            tr.bg(rgb(crate::theme::DELETED).opacity(0.28))
        } else if self.edits.contains_key(&row_ix) {
            tr.bg(rgb(crate::theme::EDITED).opacity(0.22))
        } else {
            tr
        }
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        if let Some(ed) = self
            .editing
            .as_ref()
            .filter(|e| e.row == row_ix && e.col == col_ix)
        {
            return div()
                .size_full()
                .flex()
                .items_center()
                // Editor sits exactly where the text was: same inset, the
                // row tint + red frame show through (no black box).
                // px_1 + the input's own 4px ≈ the cells' 8px text inset.
                .px_1()
                .child(ed.render())
                .into_any_element();
        }
        let value = self.display_value(row_ix, col_ix);
        let changed = self
            .edits
            .get(&row_ix)
            .is_some_and(|c| c.contains_key(&col_ix));
        let right = self.columns.get(col_ix).is_some_and(|c| c.right);
        let pg_type = self
            .source
            .as_ref()
            .and_then(|s| s.columns.get(col_ix)?.as_ref().map(|m| m.pg_type.clone()))
            .unwrap_or_default();
        let cell = div()
            .id(("rcell", row_ix * self.columns.len().max(1) + col_ix))
            .size_full()
            .flex()
            .items_center()
            .px_2()
            .when(changed, |this| {
                this.bg(rgb(crate::theme::EDITED).opacity(0.35))
            })
            .child(grid::render_value(value.as_ref(), &pg_type, right, cx));
        // Menu on the cell itself: cell-selection mode stops right-click
        // propagation at the cell (see grid.rs). Read-only results get
        // the copy part.
        let entity = cx.entity();
        cell.context_menu(move |menu, window, cx| {
            let deleted = entity.read(cx).delegate().is_row_deleted(row_ix);
            result_cell_menu(&entity, row_ix, col_ix, deleted, menu, window, cx)
        })
        .into_any_element()
    }

    fn render_empty(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .text_color(cx.theme().muted_foreground)
            .child(
                div()
                    .text_sm()
                    .font_family(crate::settings::table_font())
                    .child("No rows"),
            )
            .into_any_element()
    }

    fn cell_text(&self, row_ix: usize, col_ix: usize, _cx: &App) -> String {
        self.rows
            .get(row_ix)
            .and_then(|r| r.get(col_ix))
            .map(grid::cell_text)
            .unwrap_or_default()
    }

    fn perform_sort(
        &mut self,
        _col_ix: usize,
        _sort: ColumnSort,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) {
    }
}

pub fn result_element(state: &Entity<TableState<QueryDelegate>>) -> DataTable<QueryDelegate> {
    DataTable::new(state)
        // The pane frames the grid; no second rounded border inside it.
        .bordered(false)
        .stripe(crate::settings::get().grid_stripes)
}

/// Result grid state: cell selection (double-click edits), no row gutter.
pub fn new_result_state(window: &mut Window, cx: &mut App) -> Entity<TableState<QueryDelegate>> {
    cx.new(|cx| {
        TableState::new(QueryDelegate::empty(), window, cx)
            .cell_selectable(true)
            .row_selectable(true)
            // No whole-column (vertical) selection highlight.
            .col_selectable(false)
            .row_header(false)
    })
}

/// Build typed columns from JSON rows: numeric-only columns right-align.
pub fn infer_columns(rows: &[Value]) -> Vec<QCol> {
    let first = match rows.first().and_then(|r| r.as_object()) {
        Some(m) => m,
        None => return Vec::new(),
    };
    first
        .keys()
        .map(|name| {
            // Numeric = every value is a number or NULL, and at least one is
            // a number (an all-NULL column stays left-aligned).
            let right = rows
                .iter()
                .all(|r| r.get(name).is_none_or(|v| v.is_null() || v.is_number()))
                && rows
                    .iter()
                    .any(|r| r.get(name).is_some_and(|v| v.is_number()));
            QCol {
                name: name.clone(),
                right,
            }
        })
        .collect()
}

pub fn rows_to_vec(rows: Vec<Value>) -> Vec<Vec<Value>> {
    rows.into_iter()
        .map(|r| match r {
            Value::Object(m) => m.into_values().collect(),
            other => vec![other],
        })
        .collect()
}

/// Editing a query result in place, end to end on the local containers:
/// run a SELECT, work out its edit source, edit a cell as the result grid
/// does, save its statements and read the value back. Scratch tables only.
#[cfg(test)]
mod live_edit_tests {
    use std::collections::BTreeMap;

    use crate::drivers::live;
    use crate::engine::Engine as E;

    struct Case {
        engine: E,
        port: u16,
        user: &'static str,
        db: &'static str,
        pass: &'static str,
        schema: &'static str,
        setup: Vec<&'static str>,
        select: &'static str,
        check: &'static str,
    }

    fn case(
        engine: E,
        port: u16,
        user: &'static str,
        db: &'static str,
        pass: &'static str,
        schema: &'static str,
        setup: Vec<&'static str>,
    ) -> Case {
        let (select, check) = match engine {
            E::MongoDb => ("db.tusk_edit.find({_id: 1})", "db.tusk_edit.find({_id: 1})"),
            E::DynamoDb => (
                r#"SELECT * FROM "tusk_edit" WHERE id = 1"#,
                r#"SELECT * FROM "tusk_edit" WHERE id = 1"#,
            ),
            E::Postgres => (
                "SELECT id, v, upper(v) AS shout FROM tusk_scratch.tusk_edit WHERE id = 1",
                "SELECT v FROM tusk_scratch.tusk_edit WHERE id = 1",
            ),
            E::Cassandra => (
                "SELECT id, v, writetime(v) AS shout FROM tusk_edit WHERE id = 1",
                "SELECT v FROM tusk_edit WHERE id = 1",
            ),
            _ => (
                "SELECT id, v, upper(v) AS shout FROM tusk_edit WHERE id = 1",
                "SELECT v FROM tusk_edit WHERE id = 1",
            ),
        };
        Case {
            engine,
            port,
            user,
            db,
            pass,
            schema,
            setup,
            select,
            check,
        }
    }

    fn value_of(rows: &[serde_json::Value]) -> String {
        let r = rows[0].as_object().unwrap();
        r.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("v"))
            .map(|(_, v)| crate::grid::cell_text(v))
            .unwrap_or_default()
    }

    #[test]
    fn live_result_edits() {
        let rt = crate::db::runtime();
        let tmp = std::env::temp_dir().join(format!("tusk_result_edit_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        let std_setup = |create: &'static str, insert: &'static str| {
            vec!["DROP TABLE IF EXISTS tusk_edit", create, insert]
        };
        let cases = vec![
            case(
                E::MySql,
                33306,
                "root",
                "shop",
                "tusk",
                "shop",
                std_setup(
                    "CREATE TABLE tusk_edit (id INT PRIMARY KEY, v VARCHAR(50))",
                    "INSERT INTO tusk_edit VALUES (1, 'a'), (2, 'b')",
                ),
            ),
            case(
                E::MariaDb,
                33307,
                "root",
                "shop",
                "tusk",
                "shop",
                std_setup(
                    "CREATE TABLE tusk_edit (id INT PRIMARY KEY, v VARCHAR(50))",
                    "INSERT INTO tusk_edit VALUES (1, 'a'), (2, 'b')",
                ),
            ),
            case(
                E::MsSql,
                31433,
                "sa",
                "master",
                "Tusk_pass123",
                "dbo",
                vec![
                    "IF OBJECT_ID('dbo.tusk_edit') IS NOT NULL DROP TABLE dbo.tusk_edit",
                    "CREATE TABLE dbo.tusk_edit (id INT PRIMARY KEY, v NVARCHAR(50))",
                    "INSERT INTO dbo.tusk_edit VALUES (1, 'a'), (2, 'b')",
                ],
            ),
            case(
                E::Oracle,
                31521,
                "tusk",
                "FREEPDB1",
                "tusk",
                "TUSK",
                vec![
                    "BEGIN EXECUTE IMMEDIATE 'DROP TABLE tusk_edit'; EXCEPTION WHEN OTHERS THEN NULL; END;",
                    "CREATE TABLE tusk_edit (id NUMBER(10) PRIMARY KEY, v VARCHAR2(50))",
                    "INSERT INTO tusk_edit VALUES (1, 'a')",
                ],
            ),
            case(
                E::ClickHouse,
                38123,
                "default",
                "shop",
                "tusk",
                "shop",
                std_setup(
                    "CREATE TABLE tusk_edit (id UInt32, v String) ENGINE = MergeTree ORDER BY id",
                    "INSERT INTO tusk_edit VALUES (1, 'a'), (2, 'b')",
                ),
            ),
            case(
                E::Cockroach,
                26257,
                "root",
                "shop",
                "",
                "public",
                std_setup(
                    "CREATE TABLE tusk_edit (id INT PRIMARY KEY, v STRING)",
                    "INSERT INTO tusk_edit VALUES (1, 'a'), (2, 'b')",
                ),
            ),
            case(
                E::Postgres,
                55432,
                "tusk",
                "tusk_dev",
                "tusk",
                "tusk_scratch",
                vec![
                    "CREATE SCHEMA IF NOT EXISTS tusk_scratch",
                    "DROP TABLE IF EXISTS tusk_scratch.tusk_edit",
                    "CREATE TABLE tusk_scratch.tusk_edit (id int PRIMARY KEY, v text)",
                    "INSERT INTO tusk_scratch.tusk_edit VALUES (1, 'a'), (2, 'b')",
                ],
            ),
            case(
                E::Sqlite,
                0,
                "",
                "",
                "",
                "main",
                std_setup(
                    "CREATE TABLE tusk_edit (id INTEGER PRIMARY KEY, v TEXT)",
                    "INSERT INTO tusk_edit VALUES (1, 'a'), (2, 'b')",
                ),
            ),
            case(
                E::DuckDb,
                0,
                "",
                "",
                "",
                "main",
                std_setup(
                    "CREATE TABLE tusk_edit (id INTEGER PRIMARY KEY, v VARCHAR)",
                    "INSERT INTO tusk_edit VALUES (1, 'a'), (2, 'b')",
                ),
            ),
            case(
                E::Vertica,
                35433,
                "dbadmin",
                "docker",
                "",
                "public",
                std_setup(
                    "CREATE TABLE tusk_edit (id INT PRIMARY KEY, v VARCHAR(50))",
                    "INSERT INTO tusk_edit VALUES (1, 'a')",
                ),
            ),
            case(
                E::Cassandra,
                39042,
                "",
                "tusk_edit_ks",
                "",
                "tusk_edit_ks",
                vec![
                    "CREATE KEYSPACE IF NOT EXISTS tusk_edit_ks WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 1}",
                    "DROP TABLE IF EXISTS tusk_edit_ks.tusk_edit",
                    "CREATE TABLE tusk_edit_ks.tusk_edit (id int PRIMARY KEY, v text)",
                    "INSERT INTO tusk_edit_ks.tusk_edit (id, v) VALUES (1, 'a')",
                ],
            ),
            case(
                E::MongoDb,
                37017,
                "",
                "tusk_edit_db",
                "",
                "tusk_edit_db",
                vec![
                    "db.tusk_edit.drop()",
                    r#"db.tusk_edit.insertMany([{_id: 1, v: "a"}, {_id: 2, v: "b", extra: true}])"#,
                ],
            ),
        ];
        for c in cases {
            let mut conn = live::conn(c.engine, c.port, c.user, c.db);
            if c.port == 0 {
                conn.path = Some(
                    tmp.join(format!("{}.db", c.engine.label()))
                        .display()
                        .to_string(),
                );
            } else if !live::reachable(c.port) {
                continue;
            }
            let label = c.engine.label();
            let db = rt
                .block_on(crate::drivers::connect(
                    &conn,
                    conn.host.clone(),
                    conn.port,
                    c.pass.into(),
                ))
                .unwrap();
            let d = db.driver();
            for s in &c.setup {
                let r = rt.block_on(d.exec(s.to_string()));
                if !s.starts_with("DROP") && !s.ends_with(".drop()") {
                    r.unwrap_or_else(|e| panic!("{label} setup {s}: {e}"));
                }
            }
            // The sidebar opens the schema first (Cassandra / Mongo switch to it).
            let _ = rt.block_on(d.objects(c.schema.into()));
            // Run it the way the SQL tab does.
            let rows = rt
                .block_on(d.query_rows(c.select.into(), 100))
                .unwrap_or_else(|e| panic!("{label}: {e}"));
            let cols = super::infer_columns(&rows);
            let names: Vec<String> = cols.iter().map(|c| c.name.clone()).collect();
            let src = rt
                .block_on(crate::db::result_edit_source(
                    &db,
                    c.select,
                    c.schema,
                    names.clone(),
                ))
                .unwrap()
                .unwrap_or_else(|e| panic!("{label}: {e}"));
            let v_ix = names
                .iter()
                .position(|n| n.eq_ignore_ascii_case("v"))
                .unwrap();
            assert!(src.columns[v_ix].is_some(), "{label}: v not editable");
            if let Some(shout) = names.iter().position(|n| n == "shout") {
                assert!(
                    src.columns[shout].is_none(),
                    "{label}: an expression is editable"
                );
            }
            let mut g = super::QueryDelegate::empty();
            g.set_result(cols, super::rows_to_vec(rows), Some(src));
            g.edits
                .insert(0, BTreeMap::from([(v_ix, Some("it's edited".to_string()))]));
            let stmts = g.save_statements();
            rt.block_on(crate::db::execute_batch(&db, stmts.clone()))
                .unwrap_or_else(|e| {
                    panic!(
                        "{label} save {:?}: {e}",
                        stmts.iter().map(|s| &s.sql).collect::<Vec<_>>()
                    )
                });
            // ClickHouse mutations apply in the background.
            let mut got = String::new();
            for _ in 0..20 {
                got = value_of(&rt.block_on(d.query_rows(c.check.into(), 5)).unwrap());
                if got == "it's edited" {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            assert_eq!(got, "it's edited", "{label}");
            // Joins stay read-only (Postgres traces each column's origin
            // instead: a self-join's columns of one table stay editable).
            if !matches!(c.engine, E::MongoDb | E::Postgres) {
                let join = if c.engine == E::Postgres {
                    "SELECT a.id, a.v FROM tusk_scratch.tusk_edit a JOIN tusk_scratch.tusk_edit b ON a.id = b.id"
                } else {
                    "SELECT a.id, a.v FROM tusk_edit a JOIN tusk_edit b ON a.id = b.id"
                };
                let r = rt.block_on(crate::db::result_edit_source(
                    &db,
                    join,
                    c.schema,
                    vec!["id".into(), "v".into()],
                ));
                assert!(
                    r.map_or(true, |x| x.is_err()),
                    "{label}: a join is editable"
                );
            }
            let _ = rt.block_on(d.exec(match c.engine {
                E::MongoDb => "db.tusk_edit.drop()".into(),
                E::Cassandra => "DROP KEYSPACE tusk_edit_ks".into(),
                E::Postgres => "DROP TABLE tusk_scratch.tusk_edit".into(),
                _ => "DROP TABLE tusk_edit".into(),
            }));
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
