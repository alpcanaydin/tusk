//! Structure view of a table: one row per column
//! with name / data type / nullability / default / foreign key / comment.
//!
//! Editable like the data grid: double-click a cell to edit (the row turns
//! orange; `is_nullable` picks YES / NO from a list), Delete marks a column to
//! drop (red), "+ Column" appends a new one (green). ⌘S turns every pending
//! change into `ALTER TABLE` / `COMMENT ON` DDL run in one transaction.

use gpui_kit::assets::IconName;
use gpui_kit::component::Icon;
use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::table::{Column, DataTable, TableDelegate, TableState};
use gpui_kit::component::theme::ActiveTheme as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::db::{self, GridColumnMeta, Stmt};
use crate::icons::DbIcon;

/// Structure grid columns.
const FIELDS: [(&str, f32); 6] = [
    ("column_name", 200.),
    ("data_type", 200.),
    ("is_nullable", 100.),
    ("column_default", 240.),
    ("foreign_key", 200.),
    ("comment", 240.),
];
const F_NAME: usize = 0;
const F_TYPE: usize = 1;
const F_NULLABLE: usize = 2;
const F_DEFAULT: usize = 3;
const F_FK: usize = 4;
const F_COMMENT: usize = 5;

/// One column row: the loaded definition (`None` for a newly added column)
/// plus its current, possibly edited, values.
#[derive(Clone, Debug, PartialEq)]
pub struct StructRow {
    pub orig: Option<GridColumnMeta>,
    pub name: String,
    pub sql_type: String,
    pub nullable: bool,
    pub default: Option<String>,
    pub comment: Option<String>,
    pub deleted: bool,
    /// Part of the primary key (set per column while designing a table).
    pub pk: bool,
}

impl StructRow {
    fn from_meta(m: &GridColumnMeta) -> Self {
        Self {
            orig: Some(m.clone()),
            name: m.name.clone(),
            sql_type: m.sql_type.clone(),
            nullable: m.nullable,
            default: m.default.clone(),
            comment: m.comment.clone(),
            deleted: false,
            pk: m.is_pk,
        }
    }

    fn is_changed(&self) -> bool {
        match &self.orig {
            None => true,
            Some(o) => {
                o.name != self.name
                    || o.sql_type != self.sql_type
                    || o.nullable != self.nullable
                    || o.default != self.default
                    || o.comment != self.comment
            }
        }
    }

    fn field_text(&self, field: usize) -> String {
        match field {
            F_NAME => self.name.clone(),
            F_TYPE => self.sql_type.clone(),
            F_NULLABLE => if self.nullable { "YES" } else { "NO" }.to_string(),
            F_DEFAULT => self.default.clone().unwrap_or_default(),
            F_FK => self
                .orig
                .as_ref()
                .and_then(|o| o.foreign_key.clone())
                .unwrap_or_default(),
            F_COMMENT => self.comment.clone().unwrap_or_default(),
            _ => String::new(),
        }
    }
}

struct FieldEditor {
    row: usize,
    field: usize,
    input: Entity<InputState>,
    /// data_type editor: highlighted entry of the type list.
    pick: usize,
    /// The list filters by the text only after the user types (opened, it
    /// shows every type with the current one highlighted).
    filtering: bool,
    _sub: Subscription,
}

/// Cells edited through a drop-down list.
fn is_picker(field: usize) -> bool {
    field == F_TYPE || field == F_NULLABLE
}

pub struct StructureDelegate {
    pub schema: String,
    pub table: String,
    /// Base tables only; views / matviews show their columns read-only.
    pub editable: bool,
    pub rows: Vec<StructRow>,
    editing: Option<FieldEditor>,
    /// When Enter last committed an editor: the same keystroke must not
    /// reopen it through the grid's Enter-to-edit.
    pub enter_committed: Option<std::time::Instant>,
    /// The schema's own types (enums, domains) for the data_type picker.
    pub user_types: Vec<String>,
    /// Designing a table that doesn't exist yet: ⌘S runs `CREATE TABLE`.
    pub create: bool,
    history: crate::undo::History<Vec<StructRow>>,
    /// The data_type list's scroll (keeps the highlighted type in view).
    type_scroll: ScrollHandle,
}

impl StructureDelegate {
    pub fn new(schema: String, table: String, editable: bool, metas: &[GridColumnMeta]) -> Self {
        Self {
            schema,
            table,
            editable,
            rows: metas.iter().map(StructRow::from_meta).collect(),
            editing: None,
            enter_committed: None,
            user_types: Vec::new(),
            create: false,
            history: Default::default(),
            type_scroll: ScrollHandle::new(),
        }
    }

    /// A new table: an `id serial` primary key to start from.
    pub fn new_table(schema: String, table: String) -> Self {
        Self {
            schema,
            table,
            editable: true,
            rows: vec![StructRow {
                orig: None,
                name: "id".into(),
                sql_type: "serial".into(),
                nullable: false,
                default: None,
                comment: None,
                deleted: false,
                pk: true,
            }],
            editing: None,
            enter_committed: None,
            user_types: Vec::new(),
            create: true,
            history: Default::default(),
            type_scroll: ScrollHandle::new(),
        }
    }

    pub fn has_changes(&self) -> bool {
        self.create
            || self.editing.is_some()
            || self.rows.iter().any(|r| r.deleted || r.is_changed())
    }

    pub fn pending_count(&self) -> usize {
        if self.create {
            return self.rows.len().max(1);
        }
        self.rows
            .iter()
            .filter(|r| r.deleted || r.is_changed())
            .count()
    }

    /// Discard: every column back to its loaded definition (new ones go).
    pub fn discard_changes(&mut self) {
        if self.create {
            return;
        }
        let before = self.rows.clone();
        self.editing = None;
        self.rows = self
            .rows
            .iter()
            .filter_map(|r| r.orig.as_ref().map(StructRow::from_meta))
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

    /// "+ Column": an empty row (DEFAULT placeholders) to type into.
    pub fn add_column(&mut self) -> usize {
        let before = self.rows.clone();
        self.rows.push(StructRow {
            orig: None,
            name: String::new(),
            sql_type: String::new(),
            nullable: true,
            default: None,
            comment: None,
            deleted: false,
            pk: false,
        });
        let after = self.rows.clone();
        self.history.record(before, &after);
        self.rows.len() - 1
    }

    /// Delete key: mark (or unmark) a column for DROP; a new, unsaved column
    /// is simply removed.
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
        let Some(row) = self.rows.get_mut(row_ix) else {
            return;
        };
        if row.deleted || field == F_FK {
            return;
        }
        let current = row.field_text(field);
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
                    // data_type: Enter takes the highlighted type from the list.
                    let picking = state
                        .delegate()
                        .editing
                        .as_ref()
                        .is_some_and(|e| is_picker(e.field));
                    if picking {
                        state.delegate_mut().confirm_type_pick(None, cx);
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
                // Typing filters the type list from its top again.
                InputEvent::Change => {
                    if let Some(ed) = state.delegate_mut().editing.as_mut() {
                        ed.pick = 0;
                        ed.filtering = true;
                    }
                    state.delegate().type_scroll.scroll_to_item(0);
                    cx.notify();
                }
                _ => {}
            },
        );
        input.read(cx).focus_handle(cx).focus(window, cx);
        input.update(cx, |st, cx| st.select_all(window, cx));
        let pick = if is_picker(field) {
            let cur = self
                .rows
                .get(row_ix)
                .map(|r| r.field_text(field).to_lowercase())
                .unwrap_or_default();
            self.pick_options(field, "")
                .iter()
                .position(|t| t.to_lowercase() == cur)
                .unwrap_or(0)
        } else {
            0
        };
        self.type_scroll.scroll_to_item(pick);
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

    /// Types for the picker matching `query`: prefix matches first, then
    /// the ones that merely contain it (the whole list when empty).
    fn type_matches(&self, query: &str) -> Vec<String> {
        let q = query.trim().to_lowercase();
        let all = self.user_types.iter().cloned().chain(
            crate::ddl::type_names(db::engine())
                .iter()
                .map(|t| t.to_string()),
        );
        if q.is_empty() {
            return all.collect();
        }
        let (mut starts, mut contains): (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
        for t in all {
            let l = t.to_lowercase();
            if l.starts_with(&q) {
                starts.push(t);
            } else if l.contains(&q) {
                contains.push(t);
            }
        }
        starts.extend(contains);
        starts
    }

    /// The list under a picker cell: types, or YES / NO for is_nullable.
    fn pick_options(&self, field: usize, query: &str) -> Vec<String> {
        if field == F_NULLABLE {
            crate::combo::filter(&["YES", "NO"], query)
        } else {
            self.type_matches(query)
        }
    }

    /// What the type list filters by: nothing until the user types.
    fn type_query(&self, cx: &App) -> String {
        match self.editing.as_ref() {
            Some(ed) if ed.filtering => ed.input.read(cx).value().to_string(),
            _ => String::new(),
        }
    }

    /// ↑ / ↓ in the data_type editor.
    pub fn move_type_pick(&mut self, delta: isize, cx: &mut Context<TableState<Self>>) {
        let Some(field) = self
            .editing
            .as_ref()
            .map(|e| e.field)
            .filter(|f| is_picker(*f))
        else {
            return;
        };
        let n = self.pick_options(field, &self.type_query(cx)).len();
        if n == 0 {
            return;
        }
        if let Some(ed) = self.editing.as_mut() {
            ed.pick = (ed.pick as isize + delta).rem_euclid(n as isize) as usize;
            self.type_scroll.scroll_to_item(ed.pick);
        }
        cx.notify();
    }

    /// Enter in the data_type editor: take the highlighted type — or what
    /// was typed when it is a full type of its own (`varchar(255)`).
    pub fn confirm_type_pick(
        &mut self,
        choice: Option<String>,
        cx: &mut Context<TableState<Self>>,
    ) {
        let Some(ed) = self.editing.as_ref() else {
            return;
        };
        let typed = ed.input.read(cx).value().trim().to_string();
        let filtering = ed.filtering;
        let field = ed.field;
        let matches = self.pick_options(field, &self.type_query(cx));
        let picked = choice.or_else(|| {
            let exact = filtering && matches.iter().any(|m| m.eq_ignore_ascii_case(&typed));
            // is_nullable: only YES / NO, whatever was typed.
            if field == F_NULLABLE {
                matches.get(ed.pick).cloned()
            } else if typed.contains('(') || typed.contains('[') || exact || matches.is_empty() {
                None
            } else {
                matches.get(ed.pick).cloned()
            }
        });
        let row = ed.row;
        self.commit_edit(cx);
        if let Some(t) = picked {
            let before = self.rows.clone();
            if let Some(r) = self.rows.get_mut(row) {
                if field == F_NULLABLE {
                    r.nullable = t == "YES";
                } else {
                    r.sql_type = t;
                }
            }
            let after = self.rows.clone();
            self.history.record(before, &after);
        }
        self.enter_committed = Some(std::time::Instant::now());
        cx.notify();
    }

    /// Columns of the primary key, in table order.
    pub fn pk_columns(&self) -> Vec<String> {
        self.rows
            .iter()
            .filter(|r| r.pk && !r.deleted && !r.name.is_empty())
            .map(|r| r.name.clone())
            .collect()
    }

    /// Designing a table: add / remove a column from the primary key.
    pub fn toggle_pk(&mut self, row_ix: usize) {
        if !self.create {
            return;
        }
        let before = self.rows.clone();
        if let Some(r) = self.rows.get_mut(row_ix) {
            r.pk = !r.pk;
            if r.pk {
                r.nullable = false;
            }
        }
        let after = self.rows.clone();
        self.history.record(before, &after);
    }

    /// Why ⌘S can't run yet (a new column without a name or type).
    pub fn validation_error(&self) -> Option<String> {
        self.rows
            .iter()
            .filter(|r| !r.deleted && r.orig.is_none())
            .any(|r| r.name.trim().is_empty() || r.sql_type.trim().is_empty())
            .then(|| "Give every new column a name and a data type.".to_string())
            .or_else(|| self.ddl().err())
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
            F_NAME if !text.is_empty() => row.name = text,
            F_TYPE if !text.is_empty() => row.sql_type = text,
            F_NULLABLE if text.eq_ignore_ascii_case("yes") || text.eq_ignore_ascii_case("no") => {
                row.nullable = text.eq_ignore_ascii_case("yes")
            }
            F_DEFAULT => row.default = opt(text),
            F_COMMENT => row.comment = opt(text),
            _ => {}
        }
        let after = self.rows.clone();
        self.history.record(before, &after);
        cx.notify();
    }

    /// Tab / Shift-Tab while editing: commit and edit the next text cell of
    /// the row (toggle cells are skipped, not flipped).
    pub fn edit_neighbour(
        &mut self,
        delta: isize,
        window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        let Some((row, field)) = self.editing.as_ref().map(|e| (e.row, e.field)) else {
            return;
        };
        let count = (if self.create { 4 } else { FIELDS.len() }) as isize;
        let mut next = field as isize + delta;
        while (0..count).contains(&next) && next as usize == F_FK {
            next += delta;
        }
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

    /// Pending changes as the connected engine's DDL (see [`crate::ddl`]):
    /// drops, then per-column changes (renames first), then additions; a
    /// designed table is one `CREATE TABLE`. Empty when the engine can't
    /// make a change ([`Self::validation_error`] says why).
    pub fn save_statements(&self) -> Vec<Stmt> {
        self.ddl()
            .map(|v| v.into_iter().map(Stmt::plain).collect())
            .unwrap_or_default()
    }

    fn ddl(&self) -> Result<Vec<String>, String> {
        let e = db::engine();
        if self.create {
            crate::ddl::create_table(e, &self.schema, &self.table, &self.rows)
        } else {
            crate::ddl::alter_columns(e, &self.schema, &self.table, &self.rows)
        }
    }
}

impl TableDelegate for StructureDelegate {
    fn render_empty(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        crate::theme::empty_state("No columns", cx)
    }
    fn columns_count(&self, _cx: &App) -> usize {
        // A table being designed: name / type / nullable / default only.
        if self.create { 4 } else { FIELDS.len() }
    }

    fn rows_count(&self, _cx: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _cx: &App) -> Column {
        let (name, width) = FIELDS[col_ix];
        let mut c = Column::new(name, name);
        // Cells pad themselves (px_2): tints / editors / frames fill edge to edge.
        c = c.p_0();
        c.width = px(width);
        c.resizable = true;
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
        div().id(("struct-row", row_ix))
    }

    /// Pending colors stay visible on the selected row (a stronger shade).
    /// Double-click below the rows: same as the bar's "+" button.
    fn filler_double_clicked(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> Option<usize> {
        if !self.editable {
            return None;
        }
        let row = self.add_column();
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
            let editor = div()
                .size_full()
                .flex()
                .items_center()
                // Editor sits exactly where the text was: same inset, the
                // row tint + red frame show through (no black box).
                // px_1 + the input's own 4px ≈ the cells' 8px text inset.
                .px_1()
                .child(
                    Input::new(&ed.input)
                        .xsmall()
                        .appearance(false)
                        .bordered(false)
                        .w_full()
                        .when(col_ix == F_NULLABLE, |i| i.text_align(TextAlign::Center))
                        // Same size as the cell text (the data grid's editor too).
                        .text_size(px(crate::settings::table_text()))
                        .font_family(crate::settings::table_font()),
                );
            if !is_picker(col_ix) {
                return editor.into_any_element();
            }
            // data_type / is_nullable: a combo box, the list under the cell.
            let matches = self.pick_options(col_ix, &self.type_query(cx));
            let pick = ed.pick.min(matches.len().saturating_sub(1));
            let width = if col_ix == F_TYPE { 220. } else { 120. };
            let list = crate::combo::list(
                &matches,
                pick,
                &self.type_scroll,
                width,
                |state: &mut TableState<Self>, name, cx| {
                    state.delegate_mut().confirm_type_pick(Some(name), cx)
                },
                cx,
            );
            return editor
                .relative()
                .key_context("TypePicker")
                .on_action(cx.listener(|state, _: &crate::actions::TypePickUp, _, cx| {
                    state.delegate_mut().move_type_pick(-1, cx)
                }))
                .on_action(
                    cx.listener(|state, _: &crate::actions::TypePickDown, _, cx| {
                        state.delegate_mut().move_type_pick(1, cx)
                    }),
                )
                .on_action(
                    cx.listener(|state, _: &crate::actions::TypePickConfirm, window, cx| {
                        state.delegate_mut().confirm_type_pick(None, cx);
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
        let changed = row.orig.as_ref().is_some_and(|o| match col_ix {
            F_NAME => o.name != row.name,
            F_TYPE => o.sql_type != row.sql_type,
            F_NULLABLE => o.nullable != row.nullable,
            F_DEFAULT => o.default != row.default,
            F_COMMENT => o.comment != row.comment,
            _ => false,
        });
        // New rows read like the grid's inserted rows: DEFAULT until set.
        let placeholder = match col_ix {
            _ if row.orig.is_none() && !row.pk => "DEFAULT",
            F_DEFAULT => "NULL",
            _ => "",
        };
        let is_pk = col_ix == F_NAME && row.pk;
        // data_type reads as a combo box (↕ opens the type list).
        let type_combo = col_ix == F_TYPE && self.editable && !row.deleted;
        div()
            .size_full()
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .when(changed, |this| {
                this.bg(rgb(crate::theme::EDITED).opacity(0.35))
            })
            .when(is_pk, |this| this.child(DbIcon::Key.icon_px(12.)))
            .child(
                div()
                    .text_size(px(crate::settings::table_text()))
                    .font_family(crate::settings::table_font())
                    .truncate()
                    .when(col_ix == F_NULLABLE, |this| this.w_full().text_center())
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
            .when(type_combo, |this| {
                this.child(div().flex_1()).child(
                    div()
                        .id(("type-combo", row_ix))
                        .flex_none()
                        .text_color(t.colors.muted_foreground)
                        .child(Icon::new(IconName::ChevronsUpDown).size(px(12.)))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |state, _, window, cx| {
                                cx.stop_propagation();
                                state.set_selected_cell(row_ix, F_TYPE, cx);
                                state.delegate_mut().begin_edit(row_ix, F_TYPE, window, cx);
                            }),
                        ),
                )
            })
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
    delegate: StructureDelegate,
    window: &mut Window,
    cx: &mut App,
) -> Entity<TableState<StructureDelegate>> {
    cx.new(|cx| {
        TableState::new(delegate, window, cx)
            .cell_selectable(true)
            .row_selectable(true)
            // No whole-column (vertical) selection highlight.
            .col_selectable(false)
            .row_header(false)
            .sortable(false)
    })
}

pub fn element(state: &Entity<TableState<StructureDelegate>>) -> DataTable<StructureDelegate> {
    DataTable::new(state)
        // The pane frames the grid; no second rounded border inside it.
        .bordered(false)
        .stripe(crate::settings::get().grid_stripes)
}

#[cfg(test)]
mod tests {
    // Not `super::*`: gpui_kit's glob exports its own `test` attribute macro.
    use super::{GridColumnMeta, StructureDelegate};

    fn meta(name: &str, ty: &str, pk: bool) -> GridColumnMeta {
        GridColumnMeta {
            name: name.into(),
            pg_type: ty.into(),
            sql_type: ty.into(),
            nullable: !pk,
            default: None,
            comment: None,
            is_pk: pk,
            foreign_key: None,
            enum_values: Vec::new(),
        }
    }

    #[test]
    fn ddl_for_every_kind_of_change() {
        let mut d = StructureDelegate::new(
            "public".into(),
            "t".into(),
            true,
            &[
                meta("id", "integer", true),
                meta("a", "text", false),
                meta("b", "text", false),
            ],
        );
        d.rows[1].name = "a2".into();
        d.rows[1].sql_type = "varchar(10)".into();
        d.rows[1].nullable = false;
        d.rows[1].default = Some("'x'".into());
        d.rows[1].comment = Some("it's".into());
        d.toggle_delete(2);
        let ix = d.add_column();
        assert!(d.validation_error().is_some(), "unnamed new column");
        d.rows[ix].name = "new_column_1".into();
        d.rows[ix].sql_type = "int".into();
        assert!(d.validation_error().is_none());
        let sql: Vec<String> = d.save_statements().into_iter().map(|s| s.sql).collect();
        assert_eq!(
            sql,
            vec![
                r#"ALTER TABLE "public"."t" DROP COLUMN "b""#,
                r#"ALTER TABLE "public"."t" RENAME COLUMN "a" TO "a2""#,
                r#"ALTER TABLE "public"."t" ALTER COLUMN "a2" TYPE varchar(10) USING "a2"::varchar(10)"#,
                r#"ALTER TABLE "public"."t" ALTER COLUMN "a2" SET NOT NULL"#,
                r#"ALTER TABLE "public"."t" ALTER COLUMN "a2" SET DEFAULT 'x'"#,
                r#"COMMENT ON COLUMN "public"."t"."a2" IS 'it''s'"#,
                r#"ALTER TABLE "public"."t" ADD COLUMN "new_column_1" int"#,
            ]
        );
    }

    #[test]
    fn create_table_uses_primary_key_columns() {
        let mut d = StructureDelegate::new_table("public".into(), "untitled_table_1".into());
        let ix = d.add_column();
        d.rows[ix].name = "tenant".into();
        d.rows[ix].sql_type = "int4".into();
        d.toggle_pk(ix);
        assert_eq!(d.pk_columns(), ["id", "tenant"]);
        let sql: Vec<String> = d.save_statements().into_iter().map(|s| s.sql).collect();
        assert_eq!(
            sql,
            [
                "CREATE TABLE \"public\".\"untitled_table_1\" (\n    \"id\" serial NOT NULL,\n    \"tenant\" int4 NOT NULL,\n    PRIMARY KEY (\"id\", \"tenant\")\n)"
            ]
        );
    }
}
