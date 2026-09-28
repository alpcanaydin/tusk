//! Row detail panel (right side): the selected row of the data grid as a
//! form, one field per column. JSON / JSONB fields get a highlighted editor
//! that checks the JSON and completes the keys the column already uses;
//! long text gets a growing textarea. Leaving a field writes it back to the
//! grid as a pending change (⌘S saves, like an in-cell edit).

use std::collections::BTreeSet;
use std::rc::Rc;

use anyhow::Result;
use gpui_kit::component::IndexPath;
use gpui_kit::component::calendar::{Calendar, CalendarEvent, CalendarState, Date};
use gpui_kit::component::input::{
    CompletionProvider, Editor, EditorState, InputEvent, InputState, Rope, Textarea, TextareaState,
};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use lsp_types::{CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse};

use crate::cell_edit::{NULL_LABEL, choices, is_date_type, replace_parts, split_date, split_time};

use super::*;

/// Panel width bounds (the drag handle on its left edge resizes it).
const MIN_W: f32 = 280.;
const DEFAULT_W: f32 = 380.;

pub(super) enum FieldInput {
    Line(Entity<InputState>),
    Text(Entity<TextareaState>),
    Json(Entity<EditorState>),
    /// bool / enum: the grid's value list (+ NULL when nullable).
    Choice(Entity<SelectState<Vec<SharedString>>>),
    /// date / timestamp: the value plus a calendar (and a time field).
    DateTime {
        input: Entity<InputState>,
        calendar: Entity<CalendarState>,
        time: Option<Entity<InputState>>,
    },
}

pub(super) struct DetailField {
    col: usize,
    name: String,
    ty: String,
    input: FieldInput,
    /// Invalid JSON: shown under the editor, not written back.
    error: Option<String>,
    _subs: Vec<Subscription>,
}

pub(super) struct RowDetail {
    tab: usize,
    row: usize,
    /// The row's values the fields were built from: rebuilt when they
    /// change underneath (grid edit, reload, undo).
    built: Vec<Option<serde_json::Value>>,
    fields: Vec<DetailField>,
}

pub(super) struct RowDetailState {
    pub open: bool,
    pub width: f32,
    pub wide: bool,
    pub drag: Option<(f32, f32)>,
    pub detail: Option<RowDetail>,
}

impl Default for RowDetailState {
    fn default() -> Self {
        Self {
            open: false,
            width: DEFAULT_W,
            wide: false,
            drag: None,
            detail: None,
        }
    }
}

/// JSON editor completions: the keys this column's values already use.
struct JsonKeys {
    keys: Vec<String>,
}

fn collect_keys(v: &serde_json::Value, out: &mut BTreeSet<String>) {
    match v {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                out.insert(k.clone());
                collect_keys(v, out);
            }
        }
        serde_json::Value::Array(items) => items.iter().for_each(|v| collect_keys(v, out)),
        // jsonb read as text in some paths: parse it.
        serde_json::Value::String(s) if s.starts_with('{') || s.starts_with('[') => {
            if let Ok(inner) = serde_json::from_str::<serde_json::Value>(s) {
                collect_keys(&inner, out);
            }
        }
        _ => {}
    }
}

fn word_before(text: &str, offset: usize) -> String {
    let before = &text[..offset.min(text.len())];
    let start = before
        .char_indices()
        .rev()
        .find(|(_, c)| !(c.is_alphanumeric() || *c == '_' || *c == '-'))
        .map_or(0, |(i, c)| i + c.len_utf8());
    before[start..].to_string()
}

impl CompletionProvider for JsonKeys {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        _trigger: CompletionContext,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Task<Result<CompletionResponse>> {
        let full = text.to_string();
        let prefix = word_before(&full, offset);
        // Only inside a string that could be a key (right after a quote).
        let quoted = full[..offset.saturating_sub(prefix.len()).min(full.len())].ends_with('"');
        let items: Vec<CompletionItem> = if quoted {
            self.keys
                .iter()
                .filter(|k| k.to_lowercase().starts_with(&prefix.to_lowercase()) && **k != prefix)
                .take(50)
                .map(|k| CompletionItem {
                    label: k.clone(),
                    kind: Some(CompletionItemKind::PROPERTY),
                    detail: Some("key".into()),
                    insert_text: Some(k.clone()),
                    filter_text: Some(prefix.clone()),
                    ..Default::default()
                })
                .collect()
        } else {
            Vec::new()
        };
        Task::ready(Ok(CompletionResponse::Array(items)))
    }

    fn is_completion_trigger(&self, _offset: usize, new_text: &str, _cx: &mut App) -> bool {
        new_text
            .chars()
            .last()
            .is_some_and(|c| c == '"' || c.is_alphanumeric() || c == '_')
    }
}

fn is_json(pg_type: &str) -> bool {
    matches!(pg_type, "json" | "jsonb")
}

fn is_long_text(pg_type: &str) -> bool {
    pg_type == "text"
}

/// Text a field starts from (NULL shows as an empty field).
fn field_text(v: &Option<serde_json::Value>, pg_type: &str) -> String {
    match v {
        None | Some(serde_json::Value::Null) => String::new(),
        Some(serde_json::Value::String(s)) if is_json(pg_type) => {
            // Pending JSON edits are strings: pretty-print when they parse.
            serde_json::from_str::<serde_json::Value>(s)
                .ok()
                .and_then(|j| serde_json::to_string_pretty(&j).ok())
                .unwrap_or_else(|| s.clone())
        }
        Some(j) if is_json(pg_type) => {
            serde_json::to_string_pretty(j).unwrap_or_else(|_| crate::grid::cell_text(j))
        }
        Some(v) => crate::grid::cell_text(v),
    }
}

impl TuskApp {
    pub(super) fn toggle_row_detail(&mut self, cx: &mut Context<Self>) {
        self.row_panel.open = !self.row_panel.open;
        // One right-side panel at a time.
        if self.row_panel.open {
            self.ai.open = false;
        }
        if !self.row_panel.open {
            self.row_panel.detail = None;
        }
        cx.notify();
    }

    /// The data tab + row the panel follows (the grid's selected row).
    fn detail_target(&self, cx: &App) -> Option<(usize, usize)> {
        let ix = self.active_tab?;
        let g = self.grid_tab(ix)?;
        if g.view != TabView::Data {
            return None;
        }
        let st = g.state.read(cx);
        let row = st.selected_cell().map(|c| c.0).or(st.selected_row())?;
        Some((ix, row))
    }

    /// Called every frame from the root render: keep the fields in step
    /// with the selected row (rebuild when the row or its values change).
    pub(super) fn sync_row_detail(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.row_panel.open {
            return;
        }
        let Some((ix, row)) = self.detail_target(cx) else {
            self.row_panel.detail = None;
            return;
        };
        let Some(g) = self.grid_tab(ix) else { return };
        let state = g.state.clone();
        let d = state.read(cx).delegate();
        if self
            .row_panel
            .detail
            .as_ref()
            .is_some_and(|r| r.tab == ix && r.row == row && d.row_matches(row, &r.built))
        {
            return;
        }
        let values: Vec<Option<serde_json::Value>> =
            (0..d.metas.len()).map(|c| d.detail_value(row, c)).collect();
        let metas = d.metas.clone();
        let mut fields = Vec::new();
        for (col, m) in metas.iter().enumerate() {
            let text = field_text(&values[col], &m.pg_type);
            let mut subs = Vec::new();
            let input = if let Some(items) = choices(Some(m)) {
                let is_null = values[col].as_ref().is_none_or(serde_json::Value::is_null);
                let selected = if is_null {
                    items.iter().position(|i| i.as_ref() == NULL_LABEL)
                } else {
                    items.iter().position(|i| i.as_ref() == text)
                };
                let st =
                    cx.new(|cx| SelectState::new(items, selected.map(IndexPath::new), window, cx));
                subs.push(cx.subscribe_in(
                    &st,
                    window,
                    move |this, _, ev: &SelectEvent<Vec<SharedString>>, _, cx| {
                        let SelectEvent::Confirm(value) = ev;
                        match value.as_ref().map(|v| v.as_ref()) {
                            Some(NULL_LABEL) => this.set_detail_null(col, cx),
                            Some(v) => this.commit_detail_field(col, v.to_string(), cx),
                            None => {}
                        }
                    },
                ));
                FieldInput::Choice(st)
            } else if is_date_type(Some(m)) {
                let with_time = m.pg_type != "date";
                let (date, suffix) = split_date(&text);
                let (time_text, zone) = split_time(&suffix);
                let time_text = if time_text.is_empty() {
                    "00:00:00".to_string()
                } else {
                    time_text
                };
                let shown = match date {
                    Some(d) if with_time => format!("{} {time_text}{zone}", d.format("%Y-%m-%d")),
                    Some(d) => d.format("%Y-%m-%d").to_string(),
                    None => text.clone(),
                };
                let input = cx.new(|cx| {
                    let mut st = InputState::new(window, cx).placeholder(if with_time {
                        "YYYY-MM-DD HH:MM:SS"
                    } else {
                        "YYYY-MM-DD"
                    });
                    st.set_value(shown, window, cx);
                    st
                });
                subs.push(cx.subscribe_in(
                    &input,
                    window,
                    move |this, i, ev: &InputEvent, _, cx| {
                        if matches!(ev, InputEvent::Blur | InputEvent::PressEnter { .. }) {
                            let text = i.read(cx).value().trim().to_string();
                            if text.is_empty() {
                                this.set_detail_null(col, cx);
                            } else {
                                this.commit_detail_field(col, text, cx);
                            }
                        }
                    },
                ));
                let calendar = cx.new(|cx| {
                    let mut c = CalendarState::new(window, cx);
                    if let Some(d) = date {
                        c.set_date(d, window, cx);
                    }
                    c
                });
                // A picked day rewrites the date part (and saves it).
                let target = input.clone();
                subs.push(cx.subscribe_in(
                    &calendar,
                    window,
                    move |this, _, ev: &CalendarEvent, window, cx| {
                        let CalendarEvent::Selected(Date::Single(Some(d))) = ev else {
                            return;
                        };
                        let d = *d;
                        let v = replace_parts(&target.read(cx).value(), Some(d), None, with_time);
                        target.update(cx, |st, cx| st.set_value(v.clone(), window, cx));
                        this.commit_detail_field(col, v, cx);
                    },
                ));
                let time = with_time.then(|| {
                    let t = cx.new(|cx| {
                        let mut st = InputState::new(window, cx).placeholder("HH:MM:SS");
                        st.set_value(time_text.clone(), window, cx);
                        st
                    });
                    let target = input.clone();
                    subs.push(cx.subscribe_in(
                        &t,
                        window,
                        move |this, time, ev: &InputEvent, window, cx| match ev {
                            InputEvent::Change => {
                                let tv = time.read(cx).value().to_string();
                                target.update(cx, |st, cx| {
                                    let v = replace_parts(&st.value(), None, Some(tv), true);
                                    st.set_value(v, window, cx);
                                });
                            }
                            InputEvent::Blur | InputEvent::PressEnter { .. } => {
                                let v = target.read(cx).value().trim().to_string();
                                this.commit_detail_field(col, v, cx);
                            }
                            _ => {}
                        },
                    ));
                    t
                });
                FieldInput::DateTime {
                    input,
                    calendar,
                    time,
                }
            } else if is_json(&m.pg_type) {
                let keys = {
                    let mut set = BTreeSet::new();
                    for v in state.read(cx).delegate().column_values(col) {
                        collect_keys(&v, &mut set);
                    }
                    set.into_iter().take(500).collect()
                };
                let e = cx.new(|cx| {
                    let mut st = EditorState::new(window, cx)
                        .language("json")
                        .line_number(false)
                        .soft_wrap(true)
                        .default_value(text);
                    st.lsp_mut().completion_provider =
                        Some(Rc::new(JsonKeys { keys }) as Rc<dyn CompletionProvider>);
                    st
                });
                subs.push(
                    cx.subscribe_in(
                        &e,
                        window,
                        move |this, e, ev: &InputEvent, _, cx| match ev {
                            InputEvent::Blur => {
                                let text = e.read(cx).text().to_string();
                                this.commit_detail_field(col, text, cx);
                            }
                            InputEvent::Change => {
                                this.check_json_field(col, &e.read(cx).text().to_string(), cx)
                            }
                            _ => {}
                        },
                    ),
                );
                FieldInput::Json(e)
            } else if is_long_text(&m.pg_type) {
                let t = cx.new(|cx| {
                    let mut st = TextareaState::new(window, cx).auto_grow(4, 14);
                    st.set_value(text, window, cx);
                    st
                });
                subs.push(
                    cx.subscribe_in(&t, window, move |this, t, ev: &InputEvent, _, cx| {
                        if let InputEvent::Blur = ev {
                            let text = t.read(cx).value().to_string();
                            this.commit_detail_field(col, text, cx);
                        }
                    }),
                );
                FieldInput::Text(t)
            } else {
                let i = cx.new(|cx| {
                    let mut st = InputState::new(window, cx);
                    st.set_value(text, window, cx);
                    st
                });
                subs.push(
                    cx.subscribe_in(&i, window, move |this, i, ev: &InputEvent, _, cx| {
                        if matches!(ev, InputEvent::Blur | InputEvent::PressEnter { .. }) {
                            let text = i.read(cx).value().to_string();
                            this.commit_detail_field(col, text, cx);
                        }
                    }),
                );
                FieldInput::Line(i)
            };
            fields.push(DetailField {
                col,
                name: m.name.clone(),
                ty: m.sql_type.clone(),
                input,
                error: None,
                _subs: subs,
            });
        }
        self.row_panel.detail = Some(RowDetail {
            tab: ix,
            row,
            built: values,
            fields,
        });
    }

    fn check_json_field(&mut self, col: usize, text: &str, cx: &mut Context<Self>) {
        let Some(f) = self
            .row_panel
            .detail
            .as_mut()
            .and_then(|d| d.fields.iter_mut().find(|f| f.col == col))
        else {
            return;
        };
        let err = if text.trim().is_empty() {
            None
        } else {
            serde_json::from_str::<serde_json::Value>(text)
                .err()
                .map(|e| format!("Invalid JSON: {e}"))
        };
        if f.error != err {
            f.error = err;
            cx.notify();
        }
    }

    /// A field lost focus: its text becomes the cell's pending value.
    fn commit_detail_field(&mut self, col: usize, text: String, cx: &mut Context<Self>) {
        let Some((tab, row, pg_type)) = self.row_panel.detail.as_ref().and_then(|d| {
            let g = self.grid_tab(d.tab)?;
            let ty = g.state.read(cx).delegate().metas.get(col)?.pg_type.clone();
            Some((d.tab, d.row, ty))
        }) else {
            return;
        };
        let original = self
            .row_panel
            .detail
            .as_ref()
            .and_then(|d| d.built.get(col).cloned())
            .flatten();
        // Unchanged text (as it was shown) is not an edit.
        if field_text(&original, &pg_type) == text {
            return;
        }
        let mut value = Some(text.clone());
        if is_json(&pg_type) {
            if text.trim().is_empty() {
                value = None;
            } else {
                match serde_json::from_str::<serde_json::Value>(&text) {
                    Ok(j) => value = Some(j.to_string()),
                    Err(e) => {
                        self.toast(false, format!("Not saved — invalid JSON: {e}"));
                        cx.notify();
                        return;
                    }
                }
            }
        }
        if let Some(g) = self.grid_tab(tab) {
            g.state.update(cx, |st, cx| {
                st.delegate_mut().set_cell_text(row, col, value);
                cx.notify();
            });
        }
        cx.notify();
    }

    /// NULL button of a field.
    fn set_detail_null(&mut self, col: usize, cx: &mut Context<Self>) {
        let Some((tab, row)) = self.row_panel.detail.as_ref().map(|d| (d.tab, d.row)) else {
            return;
        };
        if let Some(g) = self.grid_tab(tab) {
            g.state.update(cx, |st, cx| {
                st.delegate_mut().set_cell_text(row, col, None);
                cx.notify();
            });
        }
        cx.notify();
    }

    /// JSON "Format": pretty-print the field's JSON in place.
    fn format_json_field(&mut self, col: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(FieldInput::Json(e)) = self
            .row_panel
            .detail
            .as_ref()
            .and_then(|d| d.fields.iter().find(|f| f.col == col))
            .map(|f| &f.input)
        else {
            return;
        };
        let e = e.clone();
        let text = e.read(cx).text().to_string();
        match serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|j| serde_json::to_string_pretty(&j).ok())
        {
            Some(pretty) => e.update(cx, |st, cx| st.set_value(pretty, window, cx)),
            None => self.toast(false, "Can't format: the JSON is invalid."),
        }
        cx.notify();
    }

    pub(super) fn render_row_detail(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.theme().clone();
        let (fg, muted, border, bg) = (t.foreground, t.muted_foreground, t.border, t.background);
        let viewport = f32::from(window.viewport_size().width);
        let width = if self.row_panel.wide {
            (viewport * 0.62).max(MIN_W)
        } else {
            self.row_panel.width
        };
        let icon_btn = |id: &'static str, icon: IconName, tip: &'static str| {
            div()
                .id(id)
                .cursor_pointer()
                .w(px(22.))
                .h(px(22.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.))
                .text_color(muted)
                .hover(|d| d.bg(muted.opacity(0.12)).text_color(fg))
                .child(Icon::new(icon).size(px(13.)))
                .tooltip(move |window, cx| {
                    gpui_kit::component::tooltip::Tooltip::new(tip).build(window, cx)
                })
        };
        let title = match &self.row_panel.detail {
            Some(d) => self
                .grid_tab(d.tab)
                .map(|g| format!("{} · row {}", g.table.name, d.row + 1))
                .unwrap_or_default(),
            None => "Row".into(),
        };
        let header = div()
            .h(px(crate::settings::bar_h() + 8.))
            .flex_none()
            .flex()
            .items_center()
            .gap_1()
            .px_3()
            .border_b_1()
            .border_color(border)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(crate::settings::ui_text()))
                    .text_color(fg)
                    .child(title),
            )
            .child(
                icon_btn(
                    "row-detail-wide",
                    if self.row_panel.wide {
                        IconName::Minimize
                    } else {
                        IconName::Maximize
                    },
                    if self.row_panel.wide {
                        "Narrow"
                    } else {
                        "Wide"
                    },
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.row_panel.wide = !this.row_panel.wide;
                    cx.notify();
                })),
            )
            .child(
                icon_btn("row-detail-close", IconName::Close, "Close")
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_row_detail(cx))),
            );
        let body: AnyElement = match &self.row_panel.detail {
            None => div()
                .p_4()
                .text_xs()
                .text_color(muted)
                .child("Select a row in the data grid to see it here.")
                .into_any_element(),
            Some(d) => {
                let null_cols: Vec<bool> = d
                    .built
                    .iter()
                    .map(|v| v.as_ref().is_none_or(serde_json::Value::is_null))
                    .collect();
                div()
                    .id("row-detail-fields")
                    .size_full()
                    .overflow_y_scroll()
                    .py_2()
                    .children(d.fields.iter().map(|f| {
                        let col = f.col;
                        let is_null = null_cols.get(col).copied().unwrap_or(false);
                        let editor: AnyElement = match &f.input {
                            FieldInput::Line(i) => Input::new(i)
                                .small()
                                .font_family(crate::settings::table_font())
                                .into_any_element(),
                            FieldInput::Text(i) => Textarea::new(i)
                                .font_family(crate::settings::table_font())
                                .into_any_element(),
                            FieldInput::Choice(st) => Select::new(st)
                                .small()
                                .w_full()
                                .into_any_element(),
                            FieldInput::DateTime { input, calendar, time } => {
                                let (cal, time) = (calendar.clone(), time.clone());
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .child(
                                        div().flex_1().min_w_0().child(
                                            Input::new(input).small().font_family(crate::settings::table_font()),
                                        ),
                                    )
                                    .child(
                                        Popover::new(("detail-cal", col))
                                            .anchor(Anchor::TopRight)
                                            .trigger(
                                                Button::new(("detail-cal-btn", col))
                                                    .ghost()
                                                    .small()
                                                    .icon(IconName::Calendar),
                                            )
                                            .content(move |_, _, cx| {
                                                let muted = cx.theme().muted_foreground;
                                                div()
                                                    .flex()
                                                    .flex_col()
                                                    .gap_2()
                                                    .child(Calendar::new(&cal))
                                                    .when_some(time.clone(), |d, t| {
                                                        d.child(
                                                            div()
                                                                .flex()
                                                                .items_center()
                                                                .gap_2()
                                                                .px_1()
                                                                .child(div().text_sm().text_color(muted).child("Time"))
                                                                .child(div().flex_1().child(
                                                                    Input::new(&t).small().font_family(crate::settings::table_font()),
                                                                )),
                                                        )
                                                    })
                                            }),
                                    )
                                    .into_any_element()
                            }
                            FieldInput::Json(e) => div()
                                .h(px(if self.row_panel.wide { 320. } else { 200. }))
                                .rounded(px(6.))
                                .border_1()
                                .border_color(if f.error.is_some() { t.red } else { border })
                                .overflow_hidden()
                                .child(Editor::new(e).bordered(false).h_full())
                                .into_any_element(),
                        };
                        let json = matches!(f.input, FieldInput::Json(_));
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .px_3()
                            .py_1p5()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_size(px(crate::settings::table_text()))
                                            .font_family(crate::settings::table_font())
                                            .text_color(fg)
                                            .child(f.name.clone()),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .text_xs()
                                            .text_color(muted.opacity(0.7))
                                            .child(f.ty.clone()),
                                    )
                                    .when(json, |d| {
                                        d.child(
                                            div()
                                                .id(("json-format", col))
                                                .cursor_pointer()
                                                .px_1p5()
                                                .rounded(px(4.))
                                                .text_xs()
                                                .text_color(muted)
                                                .hover(|d| d.bg(muted.opacity(0.12)).text_color(fg))
                                                .child("Format")
                                                .on_click(cx.listener(move |this, _, window, cx| {
                                                    this.format_json_field(col, window, cx)
                                                })),
                                        )
                                    })
                                    .child(
                                        div()
                                            .id(("field-null", col))
                                            .cursor_pointer()
                                            .px_1p5()
                                            .rounded(px(4.))
                                            .text_xs()
                                            .text_color(if is_null { fg } else { muted })
                                            .when(is_null, |d| d.bg(muted.opacity(0.18)))
                                            .hover(|d| d.bg(muted.opacity(0.12)).text_color(fg))
                                            .child("NULL")
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.set_detail_null(col, cx)
                                            })),
                                    ),
                            )
                            .child(editor)
                            .children(f.error.clone().map(|e| {
                                div().text_xs().text_color(t.red).child(e)
                            }))
                    }))
                    .into_any_element()
            }
        };
        div()
            .relative()
            .flex_none()
            .w(px(width))
            .h_full()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(border)
            .bg(bg)
            .child(
                div()
                    .id("row-detail-edge")
                    .absolute()
                    .left_0()
                    .top_0()
                    .bottom_0()
                    .w(px(5.))
                    .cursor_col_resize()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, e: &MouseDownEvent, _, _| {
                            this.row_panel.wide = false;
                            this.row_panel.drag =
                                Some((f32::from(e.position.x), this.row_panel.width));
                        }),
                    ),
            )
            .child(header)
            .child(div().flex_1().min_h_0().child(body))
            .into_any_element()
    }

    /// Status bar, right: the row detail toggle.
    pub(super) fn render_row_detail_button(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme();
        let (muted, fg, accent) = (t.muted_foreground, t.foreground, t.accent);
        let on = self.row_panel.open;
        div()
            .id("status-row-detail")
            .cursor_pointer()
            .w(px(22.))
            .h(px(20.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(4.))
            .text_color(if on { accent } else { muted })
            .hover(|d| {
                d.bg(muted.opacity(0.12))
                    .text_color(if on { accent } else { fg })
            })
            .child(Icon::new(IconName::PanelRight).size(px(13.)))
            .tooltip(|window, cx| {
                gpui_kit::component::tooltip::Tooltip::new("Row Detail")
                    .key_binding(crate::kbd::tip("cmd-shift-j"))
                    .build(window, cx)
            })
            .on_click(cx.listener(|this, _, _, cx| this.toggle_row_detail(cx)))
            .into_any_element()
    }

    /// Drag of the panel's left edge (from the root mouse-move handler).
    pub(super) fn drag_row_detail(
        &mut self,
        x: f32,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((x0, w0)) = self.row_panel.drag else {
            return false;
        };
        let max = (f32::from(window.viewport_size().width) - 420.).max(MIN_W);
        self.row_panel.width = (w0 + (x0 - x)).clamp(MIN_W, max);
        cx.notify();
        true
    }
}
