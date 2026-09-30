//! Data grid (Phase 6): virtualized DataTable over real Postgres windows.
//!
//! Strategy: `row_to_json` windows (`LIMIT/OFFSET`) cached in a BTreeMap, with
//! `rows_count` set to the real total once COUNT lands. Missing cells render a
//! placeholder and trigger a background fetch — memory stays bounded no matter
//! how big the table is (the 500k-row acceptance test).
//!
//! Editing: double-click a cell to edit in place — the
//! row turns orange; Delete marks the selected row red; nothing touches the
//! database until ⌘S, which runs every pending UPDATE/DELETE in one
//! transaction. Pending changes are keyed by the row's `ctid` (fetched with
//! every window for base tables), so they survive re-sorting and filtering;
//! the WHERE clause uses the primary key when the table has one.

use std::collections::{BTreeMap, HashSet};
use std::ops::Range;
use std::time::Instant;

use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::table::{Column, ColumnSort, DataTable, TableDelegate, TableState};
use gpui_kit::component::theme::ActiveTheme as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde_json::Value;

use crate::cell_edit::{self, CellEditHost, CellEditor};
use crate::db::{self, DbResult, GridColumnMeta, Stmt, WhereClause};

pub const WINDOW_ROWS: i64 = 200;
/// Rows per page (bottom bar ‹ › / Limit).
pub const PAGE_LIMIT: i64 = 1000;

/// One grid column with render policy derived from the Postgres type.
#[derive(Clone)]
pub struct GridColumn {
    pub name: String,
    pub pg_type: String,
    pub right_align: bool,
    pub width: Pixels,
}

impl GridColumn {
    fn from_meta(m: &GridColumnMeta) -> Self {
        let t = m.pg_type.as_str();
        let numeric = matches!(
            t,
            "int2" | "int4" | "int8" | "float4" | "float8" | "numeric" | "money"
        );
        // Widths are tuned for the 13px default; scale with the table font.
        let k = crate::settings::table_text() / 13.;
        let base = if t == "bool" {
            80.
        } else if numeric {
            130.
        } else if matches!(t, "timestamptz" | "timestamp" | "timetz") {
            210.
        } else if t == "date" {
            130.
        } else if t == "uuid" {
            300.
        } else if matches!(t, "json" | "jsonb") {
            260.
        } else if t == "bytea" {
            220.
        } else {
            180.
        };
        let width = px(base * k);
        Self {
            name: m.name.clone(),
            pg_type: t.to_string(),
            right_align: numeric,
            width,
        }
    }
}

/// Unsaved edits of one row: the row as loaded + changed cells
/// (`None` = set to NULL).
#[derive(Clone, Debug, PartialEq)]
pub struct RowEdit {
    pub original: Vec<Value>,
    pub changes: BTreeMap<usize, Option<String>>,
}

/// The in-place cell editor (one at a time) + the row's ctid key.
pub struct Editing {
    key: String,
    editor: CellEditor,
}

pub struct GridDelegate {
    pub pool: crate::db::Db,
    pub schema: String,
    pub table: String,
    /// Base tables only (views/matviews are read-only, have no ctid).
    pub editable: bool,
    pub columns: Vec<GridColumn>,
    pub metas: Vec<GridColumnMeta>,
    rows: BTreeMap<usize, Vec<Value>>,
    ctids: BTreeMap<usize, String>,
    total: Option<i64>,
    sort: Option<(String, bool)>,
    pub filter: Option<WhereClause>,
    pending: HashSet<i64>,
    /// Bumped on every reload/resort so stale window fetches are dropped.
    generation: u64,
    pub error: Option<String>,
    pub query_ms: Option<u128>,
    // ---- pending changes (saved with ⌘S) ----
    pub edits: BTreeMap<String, RowEdit>,
    pub deleted: BTreeMap<String, Vec<Value>>,
    pub editing: Option<Editing>,
    history: crate::undo::History<GridSnapshot>,
    /// `connection/database/schema.table` — where resized widths are kept.
    pub width_key: Option<String>,
    /// Paging: the grid shows `page_limit` rows starting at `page_offset`.
    pub page_offset: i64,
    pub page_limit: i64,
    /// New rows (not saved yet), shown after the page's rows. Keys `new:N`
    /// share `edits` with existing rows; ⌘S INSERTs them.
    pub inserted: Vec<String>,
    next_new: usize,
    /// Debounce for the filler double-click (the event can arrive twice).
    last_add: Option<Instant>,
}

/// Pending grid changes, as undo snapshots.
type GridSnapshot = (
    BTreeMap<String, RowEdit>,
    BTreeMap<String, Vec<Value>>,
    Vec<String>,
);

impl GridDelegate {
    pub fn total_rows(&self) -> Option<i64> {
        self.total
    }

    /// Rows on the current page.
    pub fn page_len(&self) -> Option<i64> {
        self.total
            .map(|n| (n - self.page_offset).clamp(0, self.page_limit))
    }
}

impl GridDelegate {
    pub fn new(pool: crate::db::Db, schema: String, table: String, editable: bool) -> Self {
        Self {
            pool,
            schema,
            table,
            editable,
            columns: Vec::new(),
            metas: Vec::new(),
            rows: BTreeMap::new(),
            ctids: BTreeMap::new(),
            total: None,
            sort: None,
            filter: None,
            pending: HashSet::new(),
            generation: 0,
            error: None,
            query_ms: None,
            edits: BTreeMap::new(),
            deleted: BTreeMap::new(),
            editing: None,
            history: Default::default(),
            width_key: None,
            page_offset: 0,
            page_limit: PAGE_LIMIT,
            inserted: Vec::new(),
            next_new: 0,
            last_add: None,
        }
    }

    // ---------- pending changes ----------

    /// Number of rows with unsaved changes (edited or marked deleted).
    pub fn pending_count(&self) -> usize {
        self.inserted.len()
            + self.deleted.len()
            + self
                .edits
                .keys()
                .filter(|k| !self.deleted.contains_key(*k) && !k.starts_with("new:"))
                .count()
    }

    pub fn discard_changes(&mut self) {
        self.inserted.clear();
        self.edits.clear();
        self.deleted.clear();
        self.editing = None;
        self.history.clear();
    }

    fn snapshot(&self) -> GridSnapshot {
        (
            self.edits.clone(),
            self.deleted.clone(),
            self.inserted.clone(),
        )
    }

    /// ⌘Z / ⇧⌘Z on pending changes; false when there's nothing to step to.
    pub fn undo(&mut self, redo: bool) -> bool {
        let current = self.snapshot();
        let target = if redo {
            self.history.redo(current)
        } else {
            self.history.undo(current)
        };
        let Some((edits, deleted, inserted)) = target else {
            return false;
        };
        self.editing = None;
        self.edits = edits;
        self.deleted = deleted;
        self.inserted = inserted;
        true
    }

    fn row_key(&self, row_ix: usize) -> Option<String> {
        self.row_key_ref(row_ix).map(str::to_string)
    }

    /// [`Self::row_key`] without the allocation (per-cell render paths).
    fn row_key_ref(&self, row_ix: usize) -> Option<&str> {
        let base = self.page_rows();
        if row_ix >= base {
            return self.inserted.get(row_ix - base).map(String::as_str);
        }
        self.ctids.get(&row_ix).map(String::as_str)
    }

    /// Rows of the page itself (new rows come after these).
    fn page_rows(&self) -> usize {
        self.page_len().map_or(self.rows.len(), |n| n as usize)
    }

    /// Is `row_ix` a new (unsaved) row?
    pub fn is_new_row(&self, row_ix: usize) -> bool {
        row_ix >= self.page_rows() && row_ix - self.page_rows() < self.inserted.len()
    }

    /// The row's values before pending edits (all NULL for a new row).
    fn original(&self, row_ix: usize) -> Option<Vec<Value>> {
        if self.is_new_row(row_ix) {
            return Some(vec![Value::Null; self.columns.len()]);
        }
        self.rows.get(&row_ix).cloned()
    }

    /// ⌘D: a new row with the selected row's values (primary key left to
    /// its default).
    pub fn duplicate_row(
        &mut self,
        from: usize,
        window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        let Some(values) = self.row_values(from) else {
            return;
        };
        let Some(row) = self.add_row(window, cx) else {
            return;
        };
        self.cancel_edit(cx);
        let Some(key) = self.row_key(row) else { return };
        let original = vec![Value::Null; self.columns.len()];
        for (col, v) in values.into_iter().enumerate() {
            let pk = self
                .metas
                .get(col)
                .is_some_and(|m| m.is_pk || m.default.is_some());
            if pk || v.is_null() {
                continue;
            }
            self.apply_change(
                key.clone(),
                original.clone(),
                col,
                Some(Some(cell_text(&v))),
            );
        }
        cx.notify();
    }

    /// Tab / ⇧Tab while editing: save, then edit the neighbouring cell.
    pub fn edit_neighbour(
        &mut self,
        step: isize,
        window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        let Some((row, col)) = self.editing.as_ref().map(|e| (e.editor.row, e.editor.col)) else {
            return;
        };
        self.commit_edit(cx);
        let n = self.columns.len() as isize;
        let next = col as isize + step;
        let (row, col) = if next >= n {
            (row + 1, 0)
        } else if next < 0 {
            (row.saturating_sub(1), (n - 1) as usize)
        } else {
            (row, next as usize)
        };
        self.begin_edit(row, col, window, cx);
    }

    /// "+ Row" / double-click below the data: a new row, first cell editing.
    pub fn add_row(
        &mut self,
        window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> Option<usize> {
        if !self.editable || self.columns.is_empty() {
            return None;
        }
        let before = self.snapshot();
        self.next_new += 1;
        self.inserted.push(format!("new:{}", self.next_new));
        let after = self.snapshot();
        self.history.record(before, &after);
        let row = self.page_rows() + self.inserted.len() - 1;
        // Start on the first column that has no default (usually after `id`).
        let col = self
            .metas
            .iter()
            .position(|m| m.default.is_none())
            .unwrap_or(0);
        self.begin_edit(row, col, window, cx);
        cx.notify();
        Some(row)
    }

    /// [`Self::display_value`] borrowing the loaded value (no clone of big
    /// text / JSON on every frame).
    fn display_value_ref(
        &self,
        row_ix: usize,
        col_ix: usize,
    ) -> Option<std::borrow::Cow<'_, Value>> {
        if let Some(edit) = self.row_key_ref(row_ix).and_then(|k| self.edits.get(k))
            && let Some(change) = edit.changes.get(&col_ix)
        {
            return Some(std::borrow::Cow::Owned(match change {
                Some(s) => Value::String(s.clone()),
                None => Value::Null,
            }));
        }
        self.rows
            .get(&row_ix)
            .and_then(|r| r.get(col_ix))
            .map(std::borrow::Cow::Borrowed)
    }

    /// Current display value of a cell: pending edit wins over the loaded one.
    fn display_value(&self, row_ix: usize, col_ix: usize) -> Option<Value> {
        if let Some(edit) = self.row_key(row_ix).and_then(|k| self.edits.get(&k))
            && let Some(change) = edit.changes.get(&col_ix)
        {
            return Some(match change {
                Some(s) => Value::String(s.clone()),
                None => Value::Null,
            });
        }
        self.rows.get(&row_ix).and_then(|r| r.get(col_ix)).cloned()
    }

    /// Double-click: open an in-place, type-aware editor on the cell.
    pub fn begin_edit(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        if !self.editable || col_ix >= self.columns.len() {
            return;
        }
        if self
            .editing
            .as_ref()
            .is_some_and(|e| e.editor.row == row_ix && e.editor.col == col_ix)
        {
            return;
        }
        self.commit_edit(cx);
        let Some(key) = self.row_key(row_ix) else {
            return;
        };
        if self.deleted.contains_key(&key) {
            return;
        }
        let current = self.display_value(row_ix, col_ix);
        let meta = self.metas.get(col_ix).cloned();
        let editor = CellEditor::new(row_ix, col_ix, meta.as_ref(), current.as_ref(), window, cx);
        self.editing = Some(Editing { key, editor });
        cx.notify();
    }

    /// Close the editor, keeping its value as a pending change.
    pub fn commit_edit(&mut self, cx: &mut Context<TableState<Self>>) {
        self.commit_current(cx);
    }

    fn commit_current(&mut self, cx: &mut Context<TableState<Self>>) {
        let Some(ed) = self.editing.take() else {
            return;
        };
        let (row, col) = (ed.editor.row, ed.editor.col);
        let Some(original) = self.original(row) else {
            return;
        };
        let change = cell_edit::change_for(ed.editor.value(cx), original.get(col));
        self.set_change(ed.key, original, col, change);
        cx.notify();
    }

    /// Esc: close the editor without keeping its value.
    pub fn cancel_edit(&mut self, cx: &mut Context<TableState<Self>>) -> bool {
        if self.editing.take().is_some() {
            cx.notify();
            true
        } else {
            false
        }
    }

    /// `change`: `None` = revert the cell, `Some(v)` = pending value.
    fn set_change(
        &mut self,
        key: String,
        original: Vec<Value>,
        col_ix: usize,
        change: Option<Option<String>>,
    ) {
        let before = self.snapshot();
        self.apply_change(key, original, col_ix, change);
        let after = self.snapshot();
        self.history.record(before, &after);
    }

    fn apply_change(
        &mut self,
        key: String,
        original: Vec<Value>,
        col_ix: usize,
        change: Option<Option<String>>,
    ) {
        match change {
            Some(v) => {
                self.edits
                    .entry(key)
                    .or_insert_with(|| RowEdit {
                        original,
                        changes: BTreeMap::new(),
                    })
                    .changes
                    .insert(col_ix, v);
            }
            None => {
                if let Some(edit) = self.edits.get_mut(&key) {
                    edit.changes.remove(&col_ix);
                    if edit.changes.is_empty() {
                        self.edits.remove(&key);
                    }
                }
            }
        }
    }

    /// Row detail panel: a cell as shown (its pending edit included).
    pub fn detail_value(&self, row_ix: usize, col_ix: usize) -> Option<Value> {
        self.display_value(row_ix, col_ix)
    }

    /// Does the row still show `values`? (Borrowed compare: the row panel
    /// checks this every frame.)
    pub fn row_matches(&self, row_ix: usize, values: &[Option<Value>]) -> bool {
        values.len() == self.metas.len()
            && values
                .iter()
                .enumerate()
                .all(|(c, v)| self.display_value_ref(row_ix, c).as_deref() == v.as_ref())
    }

    /// Row detail panel: write a field back as a pending change (`None`
    /// = NULL); the same revert rule as the in-cell editor.
    pub fn set_cell_text(&mut self, row_ix: usize, col_ix: usize, value: Option<String>) {
        if !self.editable {
            return;
        }
        let (Some(key), Some(original)) = (self.row_key(row_ix), self.original(row_ix)) else {
            return;
        };
        if self.deleted.contains_key(&key) {
            return;
        }
        let v = match value {
            Some(t) => cell_edit::EditValue::Text(t),
            None => cell_edit::EditValue::Null,
        };
        let change = cell_edit::change_for(Some(v), original.get(col_ix));
        self.set_change(key, original, col_ix, change);
    }

    /// ⌘C: the selected cell's value, or a whole selected row as
    /// tab-separated values.
    pub fn copy_text(&self, cell: Option<(usize, usize)>, row: Option<usize>) -> Option<String> {
        match (cell, row) {
            (Some((r, c)), _) => Some(self.cell_plain(r, c)),
            (None, Some(r)) => Some(
                (0..self.columns.len())
                    .map(|c| self.cell_plain(r, c))
                    .collect::<Vec<_>>()
                    .join("\t"),
            ),
            _ => None,
        }
    }

    /// ⇧⌘V: tab / newline separated text into the cells from `(row, col)`
    /// on, as pending changes (`NULL` pastes a NULL). Returns cells set.
    pub fn paste_text(&mut self, row: usize, col: usize, text: &str) -> usize {
        let mut n = 0;
        let rows = self.page_len().unwrap_or(0).max(0) as usize + self.inserted.len();
        for (i, line) in text.trim_end_matches(['\n', '\r']).split('\n').enumerate() {
            let r = row + i;
            if r >= rows.max(row + 1) {
                break;
            }
            for (j, field) in line.trim_end_matches('\r').split('\t').enumerate() {
                let c = col + j;
                if c >= self.columns.len() {
                    break;
                }
                let v = (field != "NULL").then(|| field.to_string());
                self.set_cell_text(r, c, v);
                n += 1;
            }
        }
        n
    }

    /// Every value of a column on the loaded page (JSON key completion).
    pub fn column_values(&self, col_ix: usize) -> Vec<Value> {
        self.rows
            .values()
            .filter_map(|r| r.get(col_ix).cloned())
            .collect()
    }

    /// Context menu "Set NULL".
    pub fn set_null(&mut self, row_ix: usize, col_ix: usize) {
        if !self.editable {
            return;
        }
        let (Some(key), Some(original)) = (self.row_key(row_ix), self.original(row_ix)) else {
            return;
        };
        let was_null = original.get(col_ix).is_none_or(Value::is_null);
        self.set_change(key, original, col_ix, (!was_null).then_some(None));
    }

    pub fn is_row_deleted(&self, row_ix: usize) -> bool {
        self.row_key(row_ix)
            .is_some_and(|k| self.deleted.contains_key(&k))
    }

    /// Delete key: mark / unmark the row for deletion (red until ⌘S).
    pub fn toggle_delete(&mut self, row_ix: usize) {
        if !self.editable {
            return;
        }
        let (Some(key), Some(original)) = (self.row_key(row_ix), self.rows.get(&row_ix).cloned())
        else {
            return;
        };
        if self.editing.as_ref().is_some_and(|e| e.key == key) {
            self.editing = None;
        }
        let before = self.snapshot();
        if key.starts_with("new:") {
            self.inserted.retain(|k| *k != key);
            self.edits.remove(&key);
            let after = self.snapshot();
            self.history.record(before, &after);
            return;
        }
        if self.deleted.remove(&key).is_none() {
            self.deleted.insert(key, original);
        }
        let after = self.snapshot();
        self.history.record(before, &after);
    }

    /// `WHERE` identifying one loaded row: primary key if any, else ctid.
    fn key_predicate(
        &self,
        ctid: &str,
        original: &[Value],
        params: &mut Vec<Option<String>>,
    ) -> String {
        let pk: Vec<(usize, &GridColumnMeta)> = self
            .metas
            .iter()
            .enumerate()
            .filter(|(_, m)| m.is_pk)
            .collect();
        let d = self.pool.dialect();
        if pk.is_empty() {
            params.push(Some(ctid.to_string()));
            let p = d.param(params.len(), None);
            return match d.row_key() {
                Some((_, compare)) => compare(&p),
                None => "1 = 0".to_string(),
            };
        }
        pk.iter()
            .map(|(ix, m)| {
                params.push(original.get(*ix).map(cell_text));
                format!(
                    "{} = {}",
                    d.quote(&m.name),
                    d.param(params.len(), Some(&m.sql_type))
                )
            })
            .collect::<Vec<_>>()
            .join(" AND ")
    }

    /// Every pending change as one parameterised statement (deletes first).
    pub fn save_statements(&self) -> Vec<Stmt> {
        let d = self.pool.dialect();
        let target = self.pool.qualified(&self.schema, &self.table);
        let mut out = Vec::new();
        for (ctid, original) in &self.deleted {
            let mut params = Vec::new();
            let pred = self.key_predicate(ctid, original, &mut params);
            out.push(Stmt {
                sql: format!("DELETE FROM {target} WHERE {pred}"),
                params,
            });
        }
        for key in &self.inserted {
            let changes = self
                .edits
                .get(key)
                .map(|e| e.changes.clone())
                .unwrap_or_default();
            let mut params = Vec::new();
            let mut cols = Vec::new();
            let mut vals = Vec::new();
            for (col, v) in &changes {
                let Some(m) = self.metas.get(*col) else {
                    continue;
                };
                params.push(v.clone());
                cols.push(d.quote(&m.name));
                vals.push(d.param(params.len(), Some(&m.sql_type)));
            }
            out.push(Stmt {
                sql: if cols.is_empty() {
                    d.insert_defaults(&target)
                } else {
                    format!(
                        "INSERT INTO {target} ({}) VALUES ({})",
                        cols.join(", "),
                        vals.join(", ")
                    )
                },
                params,
            });
        }
        for (ctid, edit) in &self.edits {
            if self.deleted.contains_key(ctid)
                || edit.changes.is_empty()
                || ctid.starts_with("new:")
            {
                continue;
            }
            let mut params = Vec::new();
            let sets: Vec<String> = edit
                .changes
                .iter()
                .filter_map(|(col, v)| {
                    let m = self.metas.get(*col)?;
                    params.push(v.clone());
                    Some(format!(
                        "{} = {}",
                        d.quote(&m.name),
                        d.param(params.len(), Some(&m.sql_type))
                    ))
                })
                .collect();
            let pred = self.key_predicate(ctid, &edit.original, &mut params);
            out.push(Stmt {
                sql: format!("UPDATE {target} SET {} WHERE {pred}", sets.join(", ")),
                params,
            });
        }
        out
    }

    /// ORDER BY fragment built ONLY from quoted identifiers + fixed keywords.
    /// Unsorted grids fall back to the primary key so LIMIT/OFFSET windows are
    /// stable and an UPDATEd row doesn't jump to the end (heap order).
    fn order_by(&self) -> Option<String> {
        match &self.sort {
            Some((col, desc)) => Some(format!(
                "ORDER BY {} {}",
                self.pool.quote(col),
                if *desc { "DESC" } else { "ASC" }
            )),
            None => default_order(self.pool.dialect(), &self.metas),
        }
    }

    fn store_window(&mut self, offset: i64, rows: Vec<Value>) {
        for (i, row) in rows.into_iter().enumerate() {
            let mut values = match row {
                Value::Object(map) => map.into_values().collect::<Vec<_>>(),
                other => vec![other],
            };
            let ix = offset as usize + i;
            // The engine's row key comes first when it has one (see fetch_window);
            // otherwise the primary key values identify the row.
            if self.editable && self.pool.row_key() {
                if !values.is_empty()
                    && let Value::String(ctid) = values.remove(0)
                {
                    self.ctids.insert(ix, ctid);
                }
            } else if self.editable {
                let pk: Vec<&Value> = self
                    .metas
                    .iter()
                    .enumerate()
                    .filter(|(_, m)| m.is_pk)
                    .filter_map(|(c, _)| values.get(c))
                    .collect();
                if !pk.is_empty() {
                    self.ctids.insert(
                        ix,
                        format!("pk:{}", serde_json::to_string(&pk).unwrap_or_default()),
                    );
                }
            }
            self.rows.insert(ix, values);
        }
        self.pending.remove(&offset);
    }

    /// Ensure the window containing `row_ix` is cached or loading.
    fn ensure_window(&mut self, row_ix: usize, cx: &mut Context<TableState<Self>>) {
        if self.columns.is_empty() {
            return;
        }
        let start = (row_ix as i64 / WINDOW_ROWS) * WINDOW_ROWS;
        if self.pending.contains(&start) {
            return;
        }
        let end = start as usize + WINDOW_ROWS as usize;
        let complete = (start as usize..end).all(|r| self.rows.contains_key(&r));
        if complete {
            return;
        }
        // Don't fetch past the page.
        let page_len = self.page_len().unwrap_or(self.page_limit);
        if start >= page_len {
            return;
        }
        let limit = WINDOW_ROWS.min(page_len - start);
        let db_offset = self.page_offset + start;
        self.pending.insert(start);
        let (pool, schema, table, order, filter, with_ctid, generation) = (
            self.pool.clone(),
            self.schema.clone(),
            self.table.clone(),
            self.order_by(),
            self.filter.clone(),
            self.editable,
            self.generation,
        );
        cx.spawn(async move |weak, cx: &mut AsyncApp| {
            let t0 = Instant::now();
            let result = db::fetch_window(
                &pool,
                &schema,
                &table,
                filter.as_ref(),
                order.as_deref(),
                with_ctid,
                limit,
                db_offset,
            )
            .await;
            let ms = t0.elapsed().as_millis();
            let _ = weak.update(cx, |state, cx| {
                let d = state.delegate_mut();
                if d.generation != generation {
                    return;
                }
                match result {
                    Ok(rows) => {
                        d.query_ms = Some(ms);
                        d.store_window(start, rows);
                    }
                    Err(e) => {
                        d.pending.remove(&start);
                        d.error = Some(e);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn resort(&mut self, col: Option<(String, bool)>, cx: &mut Context<TableState<Self>>) {
        self.sort = col;
        self.clear_cache();
        self.ensure_window(0, cx);
        cx.notify();
    }

    /// Drop cached rows (pending edits are keyed by ctid and survive).
    fn clear_cache(&mut self) {
        self.generation += 1;
        self.rows.clear();
        self.ctids.clear();
        self.pending.clear();
        self.error = None;
        self.editing = None;
    }

    /// Content-fitted widths from the loaded rows, then the user's saved
    /// widths for this table on top.
    fn fit_columns(&mut self) {
        let saved = self
            .width_key
            .as_deref()
            .map(crate::widths::saved)
            .unwrap_or_default();
        for (i, col) in self.columns.iter_mut().enumerate() {
            if let Some(w) = saved.get(&col.name) {
                col.width = px(*w);
                continue;
            }
            let texts: Vec<String> = self
                .rows
                .values()
                .take(200)
                .map(|r| r.get(i).map(cell_text).unwrap_or_default())
                .collect();
            col.width = crate::widths::auto_width(&col.name, texts.iter().map(String::as_str));
        }
    }

    /// The user resized columns: keep them (in this grid and on disk).
    pub fn remember_widths(&mut self, widths: &[Pixels]) {
        let mut changed = std::collections::BTreeMap::new();
        for (col, w) in self.columns.iter_mut().zip(widths) {
            if col.width != *w {
                col.width = *w;
                changed.insert(col.name.clone(), f32::from(*w));
            }
        }
        if let (Some(key), false) = (&self.width_key, changed.is_empty()) {
            crate::widths::save(key, changed);
        }
    }

    /// Column names in grid order.
    pub fn column_names(&self) -> Vec<String> {
        self.columns.iter().map(|c| c.name.clone()).collect()
    }

    /// Primary-key column indexes (UPDATE's WHERE in "Copy Row As").
    pub fn key_columns(&self) -> Vec<usize> {
        self.metas
            .iter()
            .enumerate()
            .filter(|(_, m)| m.is_pk)
            .map(|(i, _)| i)
            .collect()
    }

    /// A loaded row's values, pending edits applied (what the grid shows).
    /// A row as a JSON object (for "Send to Chat").
    pub fn row_json(&self, row_ix: usize) -> Option<String> {
        let row = self.row_values(row_ix)?;
        Some(crate::copy_as::format_rows(
            crate::copy_as::CopyFormat::Json,
            &self.column_names(),
            &[row],
            None,
            false,
        ))
    }

    pub fn row_values(&self, row_ix: usize) -> Option<Vec<Value>> {
        let mut row = self.rows.get(&row_ix)?.clone();
        if let Some(edit) = self.row_key(row_ix).and_then(|k| self.edits.get(&k)) {
            for (col, v) in &edit.changes {
                if let Some(slot) = row.get_mut(*col) {
                    *slot = v.clone().map(Value::String).unwrap_or(Value::Null);
                }
            }
        }
        Some(row)
    }

    /// Menu "Set Empty String" (a pending edit like typing it).
    pub fn set_empty(&mut self, row_ix: usize, col_ix: usize) {
        if !self.editable {
            return;
        }
        let (Some(key), Some(original)) = (self.row_key(row_ix), self.original(row_ix)) else {
            return;
        };
        self.set_change(key, original, col_ix, Some(Some(String::new())));
    }

    pub fn sort_by(&mut self, col_ix: usize, desc: bool, cx: &mut Context<TableState<Self>>) {
        let name = self.columns.get(col_ix).map(|c| c.name.clone());
        self.resort(name.map(|n| (n, desc)), cx);
    }

    /// Plain-text cell for export / accessibility.
    pub fn cell_plain(&self, row_ix: usize, col_ix: usize) -> String {
        self.rows
            .get(&row_ix)
            .and_then(|r| r.get(col_ix))
            .map(cell_text)
            .unwrap_or_default()
    }
}

/// Settings ▸ filter menu ▸ Default Table Sort.
fn default_order(d: crate::engine::Dialect, metas: &[GridColumnMeta]) -> Option<String> {
    match crate::settings::get().default_table_sort.as_str() {
        "None" => None,
        "Primary key descending" => pk_order(d, metas).map(|o| {
            let cols: Vec<String> = o
                .trim_start_matches("ORDER BY ")
                .split(", ")
                .map(|c| format!("{c} DESC"))
                .collect();
            format!("ORDER BY {}", cols.join(", "))
        }),
        _ => pk_order(d, metas),
    }
}

fn pk_order(d: crate::engine::Dialect, metas: &[GridColumnMeta]) -> Option<String> {
    let pk: Vec<String> = metas
        .iter()
        .filter(|m| m.is_pk)
        .map(|m| d.quote(&m.name))
        .collect();
    (!pk.is_empty()).then(|| format!("ORDER BY {}", pk.join(", ")))
}

/// First paint data (columns + count + first window), fetched off the UI
/// thread by [`reload`] and applied with [`apply_initial`].
pub struct InitialData {
    pub generation: u64,
    pub metas: DbResult<Vec<GridColumnMeta>>,
    pub total: DbResult<i64>,
    pub first: Vec<Value>,
    pub first_error: Option<String>,
}

pub struct LoadParams {
    pool: crate::db::Db,
    schema: String,
    table: String,
    filter: Option<WhereClause>,
    order: Option<String>,
    with_ctid: bool,
    generation: u64,
    page_offset: i64,
    page_limit: i64,
}

pub async fn load_initial(p: LoadParams) -> InitialData {
    let metas = db::fetch_columns(&p.pool, &p.schema, &p.table).await;
    let total = db::fetch_count(&p.pool, &p.schema, &p.table, p.filter.as_ref()).await;
    let order = match (&p.order, &metas) {
        (None, Ok(m)) => default_order(p.pool.dialect(), m),
        (o, _) => o.clone(),
    };
    let (first, first_error) = match (&metas, &total) {
        (Ok(_), Ok(_)) => match db::fetch_window(
            &p.pool,
            &p.schema,
            &p.table,
            p.filter.as_ref(),
            order.as_deref(),
            p.with_ctid,
            WINDOW_ROWS.min(p.page_limit),
            p.page_offset,
        )
        .await
        {
            Ok(rows) => (rows, None),
            Err(e) => (Vec::new(), Some(e)),
        },
        _ => (Vec::new(), None),
    };
    InitialData {
        generation: p.generation,
        metas,
        total,
        first,
        first_error,
    }
}

/// (Re)load a grid: metadata, count and first window with the current
/// filter + sort. Pending edits are kept (keyed by ctid).
pub fn reload(state: &Entity<TableState<GridDelegate>>, cx: &mut App) {
    let params = state.update(cx, |state, cx| {
        let d = state.delegate_mut();
        // Keep the current rows on screen until the new ones land (no empty
        // flash between pages); the generation bump discards stale windows.
        d.generation += 1;
        d.pending.clear();
        d.editing = None;
        cx.notify();
        LoadParams {
            pool: d.pool.clone(),
            schema: d.schema.clone(),
            table: d.table.clone(),
            filter: d.filter.clone(),
            order: d.order_by(),
            with_ctid: d.editable,
            generation: d.generation,
            page_offset: d.page_offset,
            page_limit: d.page_limit,
        }
    });
    let state = state.clone();
    cx.spawn(async move |cx: &mut AsyncApp| {
        let data = load_initial(params).await;
        // NOTE: apply_initial performs its own state.update — don't nest it.
        cx.update(|cx| apply_initial(&state, data, cx));
    })
    .detach();
}

pub fn apply_initial(state: &Entity<TableState<GridDelegate>>, data: InitialData, cx: &mut App) {
    state.update(cx, |state, cx| {
        let d = state.delegate_mut();
        if d.generation != data.generation {
            return;
        }
        d.rows.clear();
        d.ctids.clear();
        d.error = None;
        match data.metas {
            Ok(metas) => {
                d.columns = metas.iter().map(GridColumn::from_meta).collect();
                d.metas = metas;
            }
            Err(e) => d.error = Some(e),
        }
        match data.total {
            Ok(n) => d.total = Some(n),
            Err(e) => d.error = Some(e),
        }
        if let Some(e) = data.first_error {
            d.error = Some(e);
        } else {
            d.store_window(0, data.first);
        }
        d.fit_columns();
        // Columns arrive after TableState::new snapshotted zero of them;
        // rebuild the header layout or headers/cells never render.
        state.refresh(cx);
        cx.notify();
    });
}

pub(crate) fn cell_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        // Postgres array literal (`{a,b}`, what an array column takes back);
        // elsewhere lists are JSON (MongoDB / DynamoDB / Cassandra / BigQuery).
        Value::Array(a) if crate::db::dialect() == crate::engine::Dialect::Postgres => {
            let inner: Vec<String> = a.iter().map(cell_text).collect();
            format!("{{{}}}", inner.join(","))
        }
        Value::Array(_) | Value::Object(_) => json_preview(v),
    }
}

fn truncate_chars(s: &str, n: usize) -> String {
    // Stop at the n-th char instead of counting a long value to its end.
    match s.char_indices().nth(n) {
        Some((cut, _)) => {
            let mut out = s[..cut].to_string();
            out.push('…');
            out
        }
        None => s.to_string(),
    }
}

/// Collapsed one-line preview for nested JSON (jsonb arrives as real objects
/// from row_to_json, not strings). Serializes only as far as the preview
/// shows, not a whole large document.
fn json_preview(v: &Value) -> String {
    struct Bounded(Vec<u8>);
    impl std::io::Write for Bounded {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.0.len() > 600 {
                return Err(std::io::Error::other("preview full"));
            }
            self.0.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut w = Bounded(Vec::new());
    let _ = serde_json::to_writer(&mut w, v);
    if w.0.is_empty() {
        return "[object]".to_string();
    }
    truncate_chars(&String::from_utf8_lossy(&w.0), 120)
}

pub(crate) fn render_value(
    value: Option<&Value>,
    pg_type: &str,
    right_align: bool,
    cx: &App,
) -> AnyElement {
    use gpui_kit::component::theme::ActiveTheme as _;
    let t = cx.theme();
    let mono = crate::settings::table_font();
    let base = div()
        .text_size(px(crate::settings::table_text()))
        .font_family(mono)
        .truncate();
    let Some(v) = value else {
        return base
            .italic()
            .text_color(t.colors.muted_foreground)
            .when(right_align, |this| this.w_full().text_right())
            .child("…")
            .into_any_element();
    };
    match v {
        // NULL follows the column's alignment (right in numeric columns).
        Value::Null => base
            .italic()
            .text_color(t.colors.muted_foreground)
            .when(right_align, |this| this.w_full().text_right())
            .child(crate::settings::get().null_text.clone())
            .into_any_element(),
        Value::Bool(b) => base
            .text_color(t.colors.accent)
            .child(b.to_string())
            .into_any_element(),
        Value::Number(n) => base
            .w_full()
            .text_right()
            .text_color(t.colors.foreground)
            .child(n.to_string())
            .into_any_element(),
        Value::String(s) => {
            let display = if pg_type == "json" || pg_type == "jsonb" {
                truncate_chars(s, 120)
            } else if pg_type == "bytea" {
                let hex = s.strip_prefix("\\x").unwrap_or(s);
                let preview: String = hex.chars().take(48).collect();
                if hex.chars().count() > 48 {
                    format!("\\x{preview}… ({} bytes)", hex.len() / 2)
                } else {
                    format!("\\x{preview}")
                }
            } else {
                truncate_chars(s, 200)
            };
            let styled = base.text_color(t.colors.foreground);
            let styled = if right_align {
                styled.w_full().text_right()
            } else {
                styled
            };
            styled.child(display).into_any_element()
        }
        Value::Array(_) => base
            .text_color(t.colors.foreground)
            .child(truncate_chars(&cell_text(v), 160))
            .into_any_element(),
        Value::Object(_) => base
            .text_color(t.colors.muted_foreground)
            .child(json_preview(v))
            .into_any_element(),
    }
}

impl CellEditHost for GridDelegate {
    fn commit_edit(&mut self, cx: &mut Context<TableState<Self>>) {
        self.commit_current(cx);
    }
}

impl TableDelegate for GridDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _cx: &App) -> usize {
        self.page_rows() + self.inserted.len()
    }

    fn filler_double_clicked(
        &mut self,
        window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> Option<usize> {
        if self.last_add.is_some_and(|t| t.elapsed().as_millis() < 500) {
            return None;
        }
        self.last_add = Some(Instant::now());
        self.add_row(window, cx);
        None
    }

    fn column(&self, col_ix: usize, _cx: &App) -> Column {
        let col = &self.columns[col_ix];
        let mut c = Column::new(col.name.clone(), col.name.clone());
        // Cells pad themselves (px_2): tints / editors / frames fill edge to edge.
        c = c.p_0();
        if col.right_align {
            c.align = gpui_kit::TextAlign::Right;
        }
        c.width = col.width;
        c.resizable = true;
        c.sort = Some(match &self.sort {
            Some((name, desc)) if *name == col.name => {
                if *desc {
                    ColumnSort::Descending
                } else {
                    ColumnSort::Ascending
                }
            }
            _ => ColumnSort::Default,
        });
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
        let col = &self.columns[col_ix];
        let t = cx.theme();
        // Name only (types live in the Structure view), medium weight;
        // numeric columns right-align the header with their values.
        div()
            .when(col.right_align, |this| this.w_full().text_right())
            .px_2()
            .text_size(px(crate::settings::table_text()))
            .font_weight(FontWeight::MEDIUM)
            .font_family(crate::settings::table_font())
            .text_color(t.colors.foreground)
            .child(col.name.clone())
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        div().id(("grid-row", row_ix))
    }

    /// Deleted / inserted / edited rows keep their color when selected
    /// (a stronger shade) and on striped rows.
    fn row_tint(&self, row_ix: usize, selected: bool, _cx: &App) -> Option<Hsla> {
        let key = self.row_key_ref(row_ix)?;
        let (c, a) = if self.deleted.contains_key(key) {
            (crate::theme::DELETED, 0.28)
        } else if key.starts_with("new:") {
            (crate::theme::ADDED, 0.20)
        } else if self.edits.contains_key(key) {
            (crate::theme::EDITED, 0.22)
        } else {
            return None;
        };
        Some(Hsla::from(rgb(c)).opacity(if selected { a + 0.14 } else { a }))
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
            .filter(|e| e.editor.row == row_ix && e.editor.col == col_ix)
        {
            return div()
                .size_full()
                .flex()
                .items_center()
                // Editor sits exactly where the text was: same inset, the
                // row tint + red frame show through (no black box).
                // px_1 + the input's own 4px ≈ the cells' 8px text inset.
                .px_1()
                .child(ed.editor.render())
                .into_any_element();
        }
        if self.display_value_ref(row_ix, col_ix).is_none() && !self.is_new_row(row_ix) {
            let total_known = self.page_len();
            if total_known.is_none_or(|n| (row_ix as i64) < n) {
                self.ensure_window(row_ix, cx);
            }
        }
        let value = self.display_value_ref(row_ix, col_ix);
        let changed = self
            .row_key_ref(row_ix)
            .and_then(|k| self.edits.get(k))
            .is_some_and(|e| e.changes.contains_key(&col_ix));
        let col = &self.columns[col_ix];
        let cell = div()
            .id(("gcell", row_ix * self.columns.len().max(1) + col_ix))
            .size_full()
            .flex()
            .items_center()
            .px_2()
            .when(changed, |this| {
                this.bg(rgb(crate::theme::EDITED).opacity(0.35))
            })
            .map(|this| {
                // New row, column not set yet: the database default applies.
                if value.is_none() && self.is_new_row(row_ix) {
                    let muted = cx.theme().muted_foreground;
                    this.child(
                        div()
                            .italic()
                            .text_size(px(crate::settings::table_text()))
                            .font_family(crate::settings::table_font())
                            .text_color(muted)
                            .when(col.right_align, |d| d.w_full().text_right())
                            .child("DEFAULT"),
                    )
                } else {
                    this.child(render_value(
                        value.as_deref(),
                        &col.pg_type,
                        col.right_align,
                        cx,
                    ))
                }
            });
        // The context menu hangs off the cell itself: in cell-selection mode
        // DataTable stops right-click propagation at the cell, so a
        // table-level menu would never open. Views get the read-only part.
        let entity = cx.entity();
        cell.context_menu(move |menu, window, cx| {
            let deleted = entity.read(cx).delegate().is_row_deleted(row_ix);
            cell_menu(&entity, row_ix, col_ix, deleted, menu, window, cx)
        })
        .into_any_element()
    }

    fn perform_sort(
        &mut self,
        col_ix: usize,
        sort: ColumnSort,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        let name = self.columns[col_ix].name.clone();
        match sort {
            ColumnSort::Ascending => self.resort(Some((name, false)), cx),
            ColumnSort::Descending => self.resort(Some((name, true)), cx),
            ColumnSort::Default => self.resort(None, cx),
        }
    }

    fn visible_rows_changed(
        &mut self,
        visible_range: Range<usize>,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        // Prefetch one window ahead in scroll direction (cheap heuristic:
        // always cover the visible range + the next window).
        if visible_range.is_empty() {
            return;
        }
        let end = visible_range
            .end
            .min(self.page_len().map(|n| n as usize).unwrap_or(usize::MAX));
        self.ensure_window(visible_range.start, cx);
        if end > visible_range.start {
            self.ensure_window(end.saturating_sub(1), cx);
        }
        let next = end + WINDOW_ROWS as usize;
        if self.page_len().is_none_or(|n| (next as i64) < n) {
            self.ensure_window(end, cx);
        }
    }

    fn loading(&self, _cx: &App) -> bool {
        self.columns.is_empty() && self.error.is_none()
    }

    fn cell_text(&self, row_ix: usize, col_ix: usize, _cx: &App) -> String {
        self.cell_plain(row_ix, col_ix)
    }
}

/// Right-click menu of one data cell: edit / set value, copy
/// (value, column name, row as TSV/CSV/JSON/SQL/Markdown), quick filter and
/// sort on this column, delete row.
fn cell_menu(
    entity: &Entity<TableState<GridDelegate>>,
    row_ix: usize,
    col_ix: usize,
    deleted: bool,
    menu: PopupMenu,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    use crate::copy_as::{CopyFormat, Target, format_rows};
    use crate::filter::FilterOp;
    let d = entity.read(cx).delegate();
    let editable = d.editable;
    let col_name = d
        .columns
        .get(col_ix)
        .map(|c| c.name.clone())
        .unwrap_or_default();
    let value = d.row_values(row_ix).and_then(|r| r.get(col_ix).cloned());
    let is_null = value.as_ref().is_none_or(Value::is_null);
    let text = value
        .as_ref()
        .map(crate::copy_as::plain)
        .unwrap_or_default();

    let e = entity.clone();
    let copy_menu = PopupMenu::build(window, cx, move |mut m, _, _| {
        for fmt in CopyFormat::ALL {
            let e = e.clone();
            m = m.item(PopupMenuItem::new(fmt.label()).on_click(move |_, _, cx| {
                let d = e.read(cx).delegate();
                let (cols, key) = (d.column_names(), d.key_columns());
                let Some(row) = d.row_values(row_ix) else {
                    return;
                };
                let target = Target {
                    schema: &d.schema,
                    table: &d.table,
                    key: &key,
                };
                let out = format_rows(fmt, &cols, &[row], Some(&target), fmt != CopyFormat::Json);
                cx.write_to_clipboard(ClipboardItem::new_string(out));
            }));
        }
        m
    });
    let filter_menu = PopupMenu::build(window, cx, {
        let text = text.clone();
        let short = if text.chars().count() > 24 {
            format!("{}…", text.chars().take(24).collect::<String>())
        } else {
            text.clone()
        };
        move |m, _, _| {
            let quick = |label: String, op: FilterOp, v: String| {
                PopupMenuItem::new(label).on_click(move |_, window, cx| {
                    let view = cx.global::<crate::app::TuskHandle>().0.clone();
                    view.update(cx, |app, cx| {
                        app.quick_filter(col_ix, op, v.clone(), window, cx)
                    });
                })
            };
            let m = if is_null {
                m
            } else {
                m.item(quick(format!("= '{short}'"), FilterOp::Eq, text.clone()))
                    .item(quick(format!("<> '{short}'"), FilterOp::Ne, text.clone()))
                    .separator()
            };
            m.item(quick("IS NULL".into(), FilterOp::IsNull, String::new()))
                .item(quick(
                    "IS NOT NULL".into(),
                    FilterOp::IsNotNull,
                    String::new(),
                ))
        }
    });
    let e = entity.clone();
    let sort_menu = PopupMenu::build(window, cx, move |m, _, _| {
        let (e1, e2) = (e.clone(), e.clone());
        m.item(PopupMenuItem::new("Ascending").on_click(move |_, _, cx| {
            e1.update(cx, |st, cx| st.delegate_mut().sort_by(col_ix, false, cx));
        }))
        .item(PopupMenuItem::new("Descending").on_click(move |_, _, cx| {
            e2.update(cx, |st, cx| st.delegate_mut().sort_by(col_ix, true, cx));
        }))
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
        menu = menu.item(PopupMenuItem::new("Set NULL").on_click(move |_, _, cx| {
            e.update(cx, |st, cx| {
                st.delegate_mut().set_null(row_ix, col_ix);
                cx.notify();
            });
        }));
        let e = entity.clone();
        menu = menu
            .item(
                PopupMenuItem::new("Set Empty String").on_click(move |_, _, cx| {
                    e.update(cx, |st, cx| {
                        st.delegate_mut().set_empty(row_ix, col_ix);
                        cx.notify();
                    });
                }),
            )
            .separator();
    }
    let copy_value = text.clone();
    menu = menu
        .item(PopupMenuItem::new("Copy Value").on_click(move |_, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(copy_value.clone()));
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
                    let (Some(json), table) = (d.row_json(row_ix), d.table.clone()) else {
                        return;
                    };
                    let view = cx.global::<crate::app::TuskHandle>().0.clone();
                    view.update(cx, |app, cx| {
                        app.send_to_chat(format!("Row of {table}"), json, "json", window, cx)
                    });
                })
        })
        .separator()
        .item(PopupMenuItem::submenu("Filter", filter_menu))
        .item(PopupMenuItem::submenu("Sort", sort_menu));
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

/// Build the grid element for a tab state.
pub fn grid_element(state: &Entity<TableState<GridDelegate>>) -> DataTable<GridDelegate> {
    DataTable::new(state)
        // The pane frames the grid; no second rounded border inside it.
        .bordered(false)
        .stripe(crate::settings::get().grid_stripes)
}

/// Create the table state for a grid tab (cell selection so double-click edits).
pub fn new_state(
    delegate: GridDelegate,
    window: &mut Window,
    cx: &mut App,
) -> Entity<TableState<GridDelegate>> {
    cx.new(|cx| {
        TableState::new(delegate, window, cx)
            .cell_selectable(true)
            .row_selectable(true)
            // No whole-column (vertical) selection highlight.
            .col_selectable(false)
            // No empty row-header gutter on the left; re-clicking a selected
            // cell still escalates to whole-row selection.
            .row_header(false)
    })
}

/// A grid save on non-SQL engines, end to end: the statements the grid
/// writes for an edited cell reach the driver and change the data.
#[cfg(test)]
mod live_save_tests {
    use std::collections::BTreeMap;

    use serde_json::Value;

    use super::{GridDelegate, RowEdit};
    use crate::drivers::live;
    use crate::engine::Engine;

    #[test]
    fn redis_cell_edit_saves() {
        if !live::reachable(36379) {
            return;
        }
        let rt = crate::db::runtime();
        let c = live::conn(Engine::Redis, 36379, "", "3");
        let db = rt
            .block_on(crate::drivers::connect(
                &c,
                c.host.clone(),
                c.port,
                String::new(),
            ))
            .unwrap();
        let d = db.driver();
        for cmd in ["FLUSHDB", "SET user:1 alice@x", "EXPIRE user:1 3600"] {
            rt.block_on(d.query_rows(cmd.into(), 1)).unwrap();
        }
        let mut g = GridDelegate::new(db.clone(), "db3".into(), "keys".into(), true);
        g.metas = rt.block_on(d.columns("db3".into(), "keys".into())).unwrap();
        let original = vec![
            Value::from("user:1"),
            Value::from("string"),
            Value::from(3600),
            Value::from("alice@x"),
        ];
        g.edits.insert(
            "pk:[\"user:1\"]".into(),
            RowEdit {
                original,
                changes: BTreeMap::from([(3, Some("carol@x".to_string()))]),
            },
        );
        let stmts = g.save_statements();
        assert_eq!(
            stmts.len(),
            1,
            "{:?}",
            stmts.iter().map(|s| &s.sql).collect::<Vec<_>>()
        );
        rt.block_on(crate::db::execute_batch(&db, stmts)).unwrap();
        let r = rt.block_on(d.query_rows("GET user:1".into(), 1)).unwrap();
        assert_eq!(r[0]["result"], "carol@x");
        // The TTL survives a value edit.
        let r = rt.block_on(d.query_rows("TTL user:1".into(), 1)).unwrap();
        assert!(r[0]["result"].as_i64().unwrap() > 0);
        rt.block_on(d.query_rows("FLUSHDB".into(), 1)).unwrap();
    }
}
