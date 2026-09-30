//! Index view of a table: one row per index, edited like the Structure view.
//!
//! "+ Index" appends a pending row (green), Delete marks an index to drop
//! (red), double-click edits a cell (`index_algorithm` and `is_unique` pick
//! from a list). ⌘S turns the pending rows into
//! `CREATE INDEX` / `DROP INDEX` / `ALTER INDEX` / `COMMENT ON INDEX`, run
//! in the same transaction as the other pending changes of the tab.

use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::table::{Column, DataTable, TableDelegate, TableState};
use gpui_kit::component::theme::ActiveTheme as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::db::{self, IndexDef, Stmt};
use crate::icons::DbIcon;

const FIELDS: [(&str, f32); 7] = [
    ("index_name", 220.),
    ("index_algorithm", 130.),
    ("is_unique", 90.),
    ("column_name", 220.),
    ("condition", 200.),
    ("include", 160.),
    ("comment", 220.),
];
const F_NAME: usize = 0;
const F_ALGO: usize = 1;
const F_UNIQUE: usize = 2;
const F_COLUMNS: usize = 3;
const F_CONDITION: usize = 4;
const F_INCLUDE: usize = 5;
const F_COMMENT: usize = 6;

/// One index row: the loaded definition (`None` for a new index) plus its
/// current, possibly edited, values.
#[derive(Clone, Debug, PartialEq)]
pub struct IndexRow {
    pub orig: Option<IndexDef>,
    pub name: String,
    pub algorithm: String,
    pub unique: bool,
    pub columns: String,
    pub condition: Option<String>,
    pub include: String,
    pub comment: Option<String>,
    pub deleted: bool,
}

impl IndexRow {
    fn from_def(d: &IndexDef) -> Self {
        Self {
            orig: Some(d.clone()),
            name: d.name.clone(),
            algorithm: d.algorithm.to_uppercase(),
            unique: d.unique,
            columns: d.columns.clone(),
            condition: d.condition.clone(),
            include: d.include.clone(),
            comment: d.comment.clone(),
            deleted: false,
        }
    }

    /// Constraint-backed indexes (primary key / unique constraint) are
    /// dropped through their constraint and not redefined in place.
    fn locked(&self) -> bool {
        self.orig.as_ref().is_some_and(|o| o.constraint.is_some())
    }

    /// Anything but the name / comment changed: needs drop + create.
    fn definition_changed(&self) -> bool {
        self.orig.as_ref().is_some_and(|o| {
            o.algorithm.to_uppercase() != self.algorithm
                || o.unique != self.unique
                || o.columns != self.columns
                || o.condition != self.condition
                || o.include != self.include
        })
    }

    fn is_changed(&self) -> bool {
        self.orig.as_ref().is_some_and(|o| {
            self.definition_changed() || o.name != self.name || o.comment != self.comment
        })
    }

    fn field_text(&self, field: usize) -> String {
        match field {
            F_NAME => self.name.clone(),
            F_ALGO => self.algorithm.clone(),
            F_UNIQUE => if self.unique { "TRUE" } else { "FALSE" }.into(),
            F_COLUMNS => self.columns.clone(),
            F_CONDITION => self.condition.clone().unwrap_or_default(),
            F_INCLUDE => self.include.clone(),
            F_COMMENT => self.comment.clone().unwrap_or_default(),
            _ => String::new(),
        }
    }

    fn field_changed(&self, field: usize) -> bool {
        let Some(o) = &self.orig else { return false };
        match field {
            F_NAME => o.name != self.name,
            F_ALGO => o.algorithm.to_uppercase() != self.algorithm,
            F_UNIQUE => o.unique != self.unique,
            F_COLUMNS => o.columns != self.columns,
            F_CONDITION => o.condition != self.condition,
            F_INCLUDE => o.include != self.include,
            F_COMMENT => o.comment != self.comment,
            _ => false,
        }
    }
}

struct FieldEditor {
    row: usize,
    field: usize,
    input: Entity<InputState>,
    /// Picker cells: highlighted entry of the list.
    pick: usize,
    /// The list filters by the text only after the user types.
    filtering: bool,
    _sub: Subscription,
}

/// Cells edited through a drop-down list.
fn is_picker(field: usize) -> bool {
    field == F_ALGO || field == F_UNIQUE
}

pub struct IndexDelegate {
    pub schema: String,
    pub table: String,
    pub editable: bool,
    pub rows: Vec<IndexRow>,
    /// Loaded at least once (the tab shows the count before that).
    pub loaded: bool,
    editing: Option<FieldEditor>,
    /// When Enter last committed an editor: the same keystroke must not
    /// reopen it through the grid's Enter-to-edit.
    pub enter_committed: Option<std::time::Instant>,
    history: crate::undo::History<Vec<IndexRow>>,
    /// The picker list's scroll (keeps the highlighted entry in view).
    pick_scroll: ScrollHandle,
}

impl IndexDelegate {
    pub fn new(schema: String, table: String, editable: bool) -> Self {
        Self {
            schema,
            table,
            editable,
            rows: Vec::new(),
            loaded: false,
            editing: None,
            enter_committed: None,
            history: Default::default(),
            pick_scroll: ScrollHandle::new(),
        }
    }

    /// Fresh rows from the database (drops pending edits and history).
    pub fn set_indexes(&mut self, defs: &[IndexDef]) {
        self.rows = defs.iter().map(IndexRow::from_def).collect();
        self.loaded = true;
        self.editing = None;
        self.history.clear();
    }

    pub fn pending_count(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| r.deleted || r.orig.is_none() || r.is_changed())
            .count()
    }

    /// Discard: every index back to its loaded definition (new ones go).
    pub fn discard_changes(&mut self) {
        let before = self.rows.clone();
        self.editing = None;
        self.rows = self
            .rows
            .iter()
            .filter_map(|r| r.orig.as_ref().map(IndexRow::from_def))
            .collect();
        let after = self.rows.clone();
        self.history.record(before, &after);
    }

    pub fn undo(&mut self, redo: bool) -> bool {
        let current = self.rows.clone();
        let target = if redo {
            self.history.redo(current)
        } else {
            self.history.undo(current)
        };
        let Some(rows) = target else { return false };
        self.editing = None;
        self.rows = rows;
        true
    }

    /// "+ Index": an empty BTREE row; its name defaults on save.
    pub fn add_index(&mut self) -> usize {
        let before = self.rows.clone();
        self.rows.push(IndexRow {
            orig: None,
            name: String::new(),
            algorithm: "BTREE".into(),
            unique: false,
            columns: String::new(),
            condition: None,
            include: String::new(),
            comment: None,
            deleted: false,
        });
        let after = self.rows.clone();
        self.history.record(before, &after);
        self.rows.len() - 1
    }

    /// Delete key: mark (or unmark) an index for DROP; an unsaved row is
    /// simply removed.
    pub fn toggle_delete(&mut self, row_ix: usize) {
        if !self.editable {
            return;
        }
        self.editing = None;
        let before = self.rows.clone();
        let Some(row) = self.rows.get_mut(row_ix) else {
            return;
        };
        if row.orig.is_none() {
            self.rows.remove(row_ix);
        } else {
            row.deleted = !row.deleted;
        }
        let after = self.rows.clone();
        self.history.record(before, &after);
    }

    pub fn begin_edit(
        &mut self,
        row_ix: usize,
        field: usize,
        window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        if !self.editable {
            return;
        }
        self.commit_edit(cx);
        let Some(row) = self.rows.get(row_ix) else {
            return;
        };
        // Constraint-backed indexes: only the comment is editable here.
        if row.deleted || (row.locked() && field != F_COMMENT) {
            return;
        }
        let current = row.field_text(field);
        let pick = self
            .pick_options(field, "")
            .iter()
            .position(|o| o.eq_ignore_ascii_case(&current))
            .unwrap_or(0);
        let input = cx.new(|cx| {
            let mut st = InputState::new(window, cx);
            st.set_value(current, window, cx);
            st
        });
        let sub = cx.subscribe_in(
            &input,
            window,
            |state: &mut TableState<Self>, _, ev: &InputEvent, window, cx| match ev {
                InputEvent::PressEnter { .. } => {
                    let picking = state
                        .delegate()
                        .editing
                        .as_ref()
                        .is_some_and(|e| is_picker(e.field));
                    if picking {
                        state.delegate_mut().confirm_pick(None, cx);
                    } else {
                        state.delegate_mut().commit_edit(cx);
                    }
                    state.delegate_mut().enter_committed = Some(std::time::Instant::now());
                    state.focus_handle(cx).focus(window, cx);
                    cx.notify();
                }
                InputEvent::Blur => {
                    state.delegate_mut().commit_edit(cx);
                    cx.notify();
                }
                // Typing filters the list from its top again.
                InputEvent::Change => {
                    if let Some(ed) = state.delegate_mut().editing.as_mut() {
                        ed.pick = 0;
                        ed.filtering = true;
                    }
                    state.delegate().pick_scroll.scroll_to_item(0);
                    cx.notify();
                }
                _ => {}
            },
        );
        input.read(cx).focus_handle(cx).focus(window, cx);
        input.update(cx, |st, cx| st.select_all(window, cx));
        self.pick_scroll.scroll_to_item(pick);
        self.editing = Some(FieldEditor {
            row: row_ix,
            field,
            input,
            pick,
            filtering: false,
            _sub: sub,
        });
        cx.notify();
    }

    pub fn commit_edit(&mut self, cx: &mut Context<TableState<Self>>) {
        let Some(ed) = self.editing.take() else {
            return;
        };
        let text = ed.input.read(cx).value().trim().to_string();
        let before = self.rows.clone();
        let Some(row) = self.rows.get_mut(ed.row) else {
            return;
        };
        let opt = |t: String| (!t.is_empty()).then_some(t);
        match ed.field {
            F_NAME => row.name = text,
            F_COLUMNS => row.columns = text,
            F_CONDITION => row.condition = opt(text),
            F_INCLUDE => row.include = text,
            F_COMMENT => row.comment = opt(text),
            // Picker cells take typed text only when it is one of the options.
            F_ALGO => {
                if let Some(a) = crate::ddl::index_algorithms(db::engine())
                    .iter()
                    .find(|a| a.eq_ignore_ascii_case(&text))
                {
                    row.algorithm = a.to_string();
                }
            }
            F_UNIQUE => match text.to_ascii_uppercase().as_str() {
                "TRUE" => row.unique = true,
                "FALSE" => row.unique = false,
                _ => {}
            },
            _ => {}
        }
        let after = self.rows.clone();
        self.history.record(before, &after);
        cx.notify();
    }

    /// The list under a picker cell, filtered by `query`.
    fn pick_options(&self, field: usize, query: &str) -> Vec<String> {
        match field {
            F_ALGO => crate::combo::filter(crate::ddl::index_algorithms(db::engine()), query),
            F_UNIQUE => crate::combo::filter(&["TRUE", "FALSE"], query),
            _ => Vec::new(),
        }
    }

    /// What the list filters by: nothing until the user types.
    fn pick_query(&self, cx: &App) -> String {
        match self.editing.as_ref() {
            Some(ed) if ed.filtering => ed.input.read(cx).value().to_string(),
            _ => String::new(),
        }
    }

    /// ↑ / ↓ in a picker cell.
    pub fn move_pick(&mut self, delta: isize, cx: &mut Context<TableState<Self>>) {
        let Some(field) = self
            .editing
            .as_ref()
            .map(|e| e.field)
            .filter(|f| is_picker(*f))
        else {
            return;
        };
        let n = self.pick_options(field, &self.pick_query(cx)).len();
        if n == 0 {
            return;
        }
        if let Some(ed) = self.editing.as_mut() {
            ed.pick = (ed.pick as isize + delta).rem_euclid(n as isize) as usize;
            self.pick_scroll.scroll_to_item(ed.pick);
        }
        cx.notify();
    }

    /// Enter / a click in a picker cell: take that entry (or the highlighted one).
    pub fn confirm_pick(&mut self, choice: Option<String>, cx: &mut Context<TableState<Self>>) {
        let Some(ed) = self.editing.as_ref() else {
            return;
        };
        let (row, field) = (ed.row, ed.field);
        let picked = choice.or_else(|| {
            self.pick_options(field, &self.pick_query(cx))
                .get(ed.pick)
                .cloned()
        });
        self.editing = None;
        if let Some(v) = picked {
            let before = self.rows.clone();
            if let Some(r) = self.rows.get_mut(row) {
                if field == F_UNIQUE {
                    r.unique = v == "TRUE";
                } else {
                    r.algorithm = v;
                }
            }
            let after = self.rows.clone();
            self.history.record(before, &after);
        }
        self.enter_committed = Some(std::time::Instant::now());
        cx.notify();
    }

    /// Tab / Shift-Tab while editing: commit and edit the next cell of the row.
    pub fn edit_neighbour(
        &mut self,
        delta: isize,
        window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        let Some((row, field)) = self.editing.as_ref().map(|e| (e.row, e.field)) else {
            return;
        };
        let count = FIELDS.len() as isize;
        let next = field as isize + delta;
        self.commit_edit(cx);
        if (0..count).contains(&next) {
            self.begin_edit(row, next as usize, window, cx);
        }
    }

    /// Enter on the framed cell: edit it, unless an editor is open or Enter
    /// just closed one.
    pub fn enter_edit(
        &mut self,
        cell: Option<(usize, usize)>,
        window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        let just_closed = self
            .enter_committed
            .is_some_and(|t| t.elapsed() < std::time::Duration::from_millis(250));
        if self.editing.is_some() || just_closed {
            return;
        }
        if let Some((r, c)) = cell {
            self.begin_edit(r, c, window, cx);
        }
    }

    /// The cell being edited, if any.
    pub fn editing_cell(&self) -> Option<(usize, usize)> {
        self.editing.as_ref().map(|e| (e.row, e.field))
    }

    pub fn cancel_edit(&mut self, cx: &mut Context<TableState<Self>>) -> bool {
        if self.editing.take().is_some() {
            cx.notify();
            true
        } else {
            false
        }
    }

    /// A name for a new index without one: `<table>_<cols>_idx`.
    fn default_name(&self, row: &IndexRow) -> String {
        let cols: String = row
            .columns
            .split(',')
            .map(|c| {
                c.trim()
                    .chars()
                    .map(|ch| if ch.is_alphanumeric() { ch } else { '_' })
                    .collect::<String>()
            })
            .filter(|c| !c.is_empty())
            .collect::<Vec<_>>()
            .join("_");
        format!("{}_{}_idx", self.table, cols)
    }

    /// Why ⌘S can't run yet (a new index without columns, or a change the
    /// engine can't make), if anything.
    pub fn validation_error(&self) -> Option<String> {
        self.rows
            .iter()
            .filter(|r| !r.deleted && r.orig.is_none())
            .any(|r| r.columns.trim().is_empty())
            .then(|| "Give every new index at least one column.".to_string())
            .or_else(|| self.ddl().err())
    }

    /// Pending changes as the connected engine's DDL: drops first, then
    /// redefinitions, renames and comments, then new indexes.
    pub fn save_statements(&self) -> Vec<Stmt> {
        self.ddl()
            .map(|v| v.into_iter().map(Stmt::plain).collect())
            .unwrap_or_default()
    }

    fn ddl(&self) -> Result<Vec<String>, String> {
        use crate::ddl;
        let e = db::engine();
        let (schema, table) = (self.schema.as_str(), self.table.as_str());
        let mut out = Vec::new();
        for r in &self.rows {
            if let (true, Some(o)) = (r.deleted, &r.orig) {
                out.push(ddl::drop_index(e, schema, table, o));
            }
        }
        let recreate = |out: &mut Vec<String>, r: &IndexRow, o: &IndexDef| -> Result<(), String> {
            out.push(ddl::drop_index(e, schema, table, o));
            out.push(ddl::create_index(e, schema, table, r, &r.name)?);
            if r.comment.is_some() {
                out.extend(ddl::comment_index(e, schema, &r.name, &r.comment));
            }
            Ok(())
        };
        for r in self.rows.iter().filter(|r| !r.deleted) {
            let Some(o) = &r.orig else { continue };
            if r.definition_changed() && !r.locked() {
                recreate(&mut out, r, o)?;
                continue;
            }
            if o.name != r.name && !r.locked() {
                match ddl::rename_index(e, schema, table, &o.name, &r.name) {
                    Some(sql) => out.push(sql),
                    None => {
                        recreate(&mut out, r, o)?;
                        continue;
                    }
                }
            }
            if o.comment != r.comment {
                out.extend(ddl::comment_index(e, schema, &r.name, &r.comment));
            }
        }
        for r in self.rows.iter().filter(|r| !r.deleted && r.orig.is_none()) {
            let name = if r.name.trim().is_empty() {
                self.default_name(r)
            } else {
                r.name.clone()
            };
            out.push(ddl::create_index(e, schema, table, r, &name)?);
            if r.comment.is_some() {
                out.extend(ddl::comment_index(e, schema, &name, &r.comment));
            }
        }
        Ok(out)
    }
}

impl TableDelegate for IndexDelegate {
    fn render_empty(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        crate::theme::empty_state("No indexes", cx)
    }
    fn columns_count(&self, _cx: &App) -> usize {
        FIELDS.len()
    }

    fn rows_count(&self, _cx: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _cx: &App) -> Column {
        let (name, width) = FIELDS[col_ix];
        let mut c = Column::new(name, name).p_0();
        // Fixed column order: this delegate keeps its data by position.
        c.movable = false;
        c.width = px(width);
        c.resizable = true;
        c
    }

    fn render_last_empty_col(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        div()
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        div()
            .px_2()
            .text_size(px(crate::settings::table_text()))
            .font_weight(FontWeight::MEDIUM)
            .font_family(crate::settings::table_font())
            .text_color(cx.theme().foreground)
            .child(FIELDS[col_ix].0)
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        div().id(("index-row", row_ix))
    }

    /// Double-click below the rows: same as the bar's "+" button.
    fn filler_double_clicked(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> Option<usize> {
        if !self.editable {
            return None;
        }
        let row = self.add_index();
        cx.notify();
        Some(row)
    }

    fn row_tint(&self, row_ix: usize, selected: bool, _cx: &App) -> Option<Hsla> {
        let r = self.rows.get(row_ix)?;
        let (c, a) = if r.deleted {
            (crate::theme::DELETED, 0.28)
        } else if r.orig.is_none() {
            (crate::theme::ADDED, 0.22)
        } else if r.is_changed() {
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
            .filter(|e| e.row == row_ix && e.field == col_ix)
        {
            let editor = div().size_full().flex().items_center().px_1().child(
                Input::new(&ed.input)
                    .xsmall()
                    .appearance(false)
                    .bordered(false)
                    .w_full()
                    .when(col_ix == F_ALGO || col_ix == F_UNIQUE, |i| {
                        i.text_align(TextAlign::Center)
                    })
                    // Same size as the cell text (the data grid's editor too).
                    .text_size(px(crate::settings::table_text()))
                    .font_family(crate::settings::table_font()),
            );
            if !is_picker(col_ix) {
                return editor.into_any_element();
            }
            let matches = self.pick_options(col_ix, &self.pick_query(cx));
            let pick = ed.pick.min(matches.len().saturating_sub(1));
            let list = crate::combo::list(
                &matches,
                pick,
                &self.pick_scroll,
                140.,
                |state: &mut TableState<Self>, v, cx| {
                    state.delegate_mut().confirm_pick(Some(v), cx)
                },
                cx,
            );
            return editor
                .relative()
                .key_context("TypePicker")
                .on_action(cx.listener(|state, _: &crate::actions::TypePickUp, _, cx| {
                    state.delegate_mut().move_pick(-1, cx)
                }))
                .on_action(
                    cx.listener(|state, _: &crate::actions::TypePickDown, _, cx| {
                        state.delegate_mut().move_pick(1, cx)
                    }),
                )
                .on_action(
                    cx.listener(|state, _: &crate::actions::TypePickConfirm, window, cx| {
                        state.delegate_mut().confirm_pick(None, cx);
                        state.focus_handle(cx).focus(window, cx);
                        cx.notify();
                    }),
                )
                .when(!matches.is_empty(), |d| d.child(list))
                .into_any_element();
        }
        let t = cx.theme();
        let Some(row) = self.rows.get(row_ix) else {
            return div().into_any_element();
        };
        let text = row.field_text(col_ix);
        let empty = text.is_empty();
        let new_row = row.orig.is_none();
        // New rows read like the grid's inserted rows: DEFAULT until set.
        let placeholder = match col_ix {
            _ if new_row => "DEFAULT",
            F_CONDITION | F_INCLUDE => "EMPTY",
            F_COMMENT => "NULL",
            _ => "",
        };
        let is_pk = col_ix == F_NAME && row.orig.as_ref().is_some_and(|o| o.primary);
        let centered = col_ix == F_ALGO || col_ix == F_UNIQUE;
        div()
            .size_full()
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .when(row.field_changed(col_ix), |this| {
                this.bg(rgb(crate::theme::EDITED).opacity(0.35))
            })
            .when(is_pk, |this| this.child(DbIcon::Key.icon_px(12.)))
            .child(
                div()
                    .text_size(px(crate::settings::table_text()))
                    .font_family(crate::settings::table_font())
                    .truncate()
                    .when(centered, |this| this.w_full().text_center())
                    .map(|this| {
                        if empty {
                            this.italic()
                                .text_color(t.colors.muted_foreground)
                                .child(placeholder)
                        } else {
                            this.text_color(t.colors.foreground).child(text)
                        }
                    }),
            )
            .into_any_element()
    }

    fn cell_text(&self, row_ix: usize, col_ix: usize, _cx: &App) -> String {
        self.rows
            .get(row_ix)
            .map(|r| r.field_text(col_ix))
            .unwrap_or_default()
    }
}

pub fn new_state(
    delegate: IndexDelegate,
    window: &mut Window,
    cx: &mut App,
) -> Entity<TableState<IndexDelegate>> {
    cx.new(|cx| {
        TableState::new(delegate, window, cx)
            .cell_selectable(true)
            .row_selectable(true)
            .col_selectable(false)
            .row_header(false)
            .sortable(false)
    })
}

pub fn element(state: &Entity<TableState<IndexDelegate>>) -> DataTable<IndexDelegate> {
    DataTable::new(state)
        .bordered(false)
        .stripe(crate::settings::get().grid_stripes)
}

#[cfg(test)]
mod tests {
    use super::{IndexDef, IndexDelegate};

    fn def(name: &str, cols: &str, constraint: Option<&str>) -> IndexDef {
        IndexDef {
            name: name.into(),
            algorithm: "btree".into(),
            unique: constraint.is_some(),
            primary: constraint.is_some(),
            columns: cols.into(),
            include: String::new(),
            condition: None,
            comment: None,
            constraint: constraint.map(Into::into),
        }
    }

    fn sql(d: &IndexDelegate) -> Vec<String> {
        d.save_statements().into_iter().map(|s| s.sql).collect()
    }

    #[test]
    fn new_index_create_statement() {
        let mut d = IndexDelegate::new("public".into(), "t".into(), true);
        d.set_indexes(&[def("t_pkey", "id", Some("t_pkey"))]);
        let ix = d.add_index();
        d.rows[ix].columns = "email, lower(name)".into();
        d.rows[ix].unique = true;
        d.rows[ix].condition = Some("deleted_at IS NULL".into());
        assert_eq!(
            sql(&d),
            [
                r#"CREATE UNIQUE INDEX "t_email_lower_name__idx" ON "public"."t" USING btree ("email", lower(name)) WHERE deleted_at IS NULL"#
            ]
        );
        assert!(d.validation_error().is_none());
        d.rows[ix].columns.clear();
        assert!(d.validation_error().is_some());
    }

    #[test]
    fn drop_rename_and_redefine() {
        let mut d = IndexDelegate::new("public".into(), "t".into(), true);
        d.set_indexes(&[
            def("t_pkey", "id", Some("t_pkey")),
            def("t_a_idx", "a", None),
            def("t_b_idx", "b", None),
        ]);
        d.toggle_delete(0); // constraint-backed → DROP CONSTRAINT
        d.rows[1].name = "t_a_idx2".into(); // rename only
        d.rows[2].algorithm = "HASH".into(); // redefinition
        assert_eq!(
            sql(&d),
            [
                r#"ALTER TABLE "public"."t" DROP CONSTRAINT "t_pkey""#,
                r#"ALTER INDEX "public"."t_a_idx" RENAME TO "t_a_idx2""#,
                r#"DROP INDEX "public"."t_b_idx""#,
                r#"CREATE INDEX "t_b_idx" ON "public"."t" USING hash ("b")"#,
            ]
        );
        assert_eq!(d.pending_count(), 3);
    }
}
