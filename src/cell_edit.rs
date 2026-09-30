//! Type-aware in-cell editors, shared by the table grid and
//! editable SQL results:
//!
//! - `bool` and enum columns → kit `Select` (values + `NULL` when nullable)
//! - `date` / `timestamp[tz]` → kit `DatePicker`; a timestamp keeps its
//!   original time-of-day + zone, only the date part changes
//! - everything else → a plain text `Input`
//!
//! Choosing a value in a Select / DatePicker commits immediately; a text
//! input commits on Enter or blur. The host delegate implements
//! [`CellEditHost`] so the widgets' events can reach its `commit_edit`.

use chrono::NaiveDate;
use gpui_kit::assets::IconName;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::IndexPath;
use gpui_kit::component::Sizable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::calendar::{Calendar, CalendarEvent, CalendarState, Date};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::table::{TableDelegate, TableState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde_json::Value;

use crate::db::GridColumnMeta;
use crate::grid::cell_text;

pub(crate) const NULL_LABEL: &str = "NULL";

/// A table delegate that hosts [`CellEditor`]s.
pub trait CellEditHost: TableDelegate + Sized + 'static {
    fn commit_edit(&mut self, cx: &mut Context<TableState<Self>>);
}

/// What the editor holds right now.
pub enum EditValue {
    Text(String),
    Null,
}

enum Widget {
    Text(Entity<InputState>),
    /// `picked` holds the value from the Select's `Confirm` event (the
    /// state's own selection isn't updated yet when the event fires).
    Choice {
        state: Entity<SelectState<Vec<SharedString>>>,
        picked: std::rc::Rc<std::cell::RefCell<Option<Option<SharedString>>>>,
    },
    /// Date / timestamp: ONE text field with the whole value (typed or
    /// picked) and a calendar button whose popover holds the month view and,
    /// for timestamps, an `HH:MM:SS` field. Both write into `input`.
    DateTime {
        input: Entity<InputState>,
        calendar: Entity<CalendarState>,
        time: Option<Entity<InputState>>,
    },
}

pub struct CellEditor {
    pub row: usize,
    pub col: usize,
    /// Numeric column: the input keeps the grid's right alignment.
    right: bool,
    widget: Widget,
    _subs: Vec<Subscription>,
}

/// Choice list for a column, if it gets a Select.
pub(crate) fn choices(meta: Option<&GridColumnMeta>) -> Option<Vec<SharedString>> {
    let meta = meta?;
    let mut items: Vec<SharedString> = if meta.pg_type == "bool" {
        vec!["true".into(), "false".into()]
    } else if !meta.enum_values.is_empty() {
        meta.enum_values.iter().map(|v| v.clone().into()).collect()
    } else {
        return None;
    };
    if meta.nullable {
        items.push(NULL_LABEL.into());
    }
    Some(items)
}

pub(crate) fn is_date_type(meta: Option<&GridColumnMeta>) -> bool {
    meta.is_some_and(|m| matches!(m.pg_type.as_str(), "date" | "timestamp" | "timestamptz"))
}

/// `2026-09-05T11:41:16.320+00:00` → (date, `T11:41:16.320+00:00`).
pub(crate) fn split_date(text: &str) -> (Option<NaiveDate>, String) {
    let head = text.get(..10).unwrap_or("");
    match NaiveDate::parse_from_str(head, "%Y-%m-%d") {
        Ok(d) => (Some(d), text[10..].to_string()),
        Err(_) => (None, String::new()),
    }
}

impl CellEditor {
    /// Build the right editor for `meta`'s type, focused / opened.
    pub fn new<T: CellEditHost>(
        row: usize,
        col: usize,
        meta: Option<&GridColumnMeta>,
        current: Option<&Value>,
        window: &mut Window,
        cx: &mut Context<TableState<T>>,
    ) -> Self {
        let right = meta.is_some_and(|m| {
            matches!(
                m.pg_type.as_str(),
                "int2" | "int4" | "int8" | "float4" | "float8" | "numeric" | "money"
            )
        });
        let text = current
            .filter(|v| !v.is_null())
            .map(cell_text)
            .unwrap_or_default();
        let is_null = current.is_none_or(Value::is_null);

        if let Some(items) = choices(meta) {
            let selected = if is_null {
                items.iter().position(|i| i == NULL_LABEL)
            } else {
                items.iter().position(|i| i.as_ref() == text)
            };
            let state =
                cx.new(|cx| SelectState::new(items, selected.map(IndexPath::new), window, cx));
            let picked = std::rc::Rc::new(std::cell::RefCell::new(None));
            let sink = picked.clone();
            let sub = cx.subscribe_in(
                &state,
                window,
                move |table: &mut TableState<T>,
                      _,
                      ev: &SelectEvent<Vec<SharedString>>,
                      window,
                      cx| {
                    let SelectEvent::Confirm(value) = ev;
                    *sink.borrow_mut() = Some(value.clone());
                    table.delegate_mut().commit_edit(cx);
                    table.focus_handle(cx).focus(window, cx);
                    cx.notify();
                },
            );
            state.update(cx, |st, cx| st.open_menu(window, cx));
            return Self {
                row,
                col,
                right,
                widget: Widget::Choice { state, picked },
                _subs: vec![sub],
            };
        }

        if is_date_type(meta) {
            let with_time = meta.is_some_and(|m| m.pg_type != "date");
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
            let calendar = cx.new(|cx| {
                let mut c = CalendarState::new(window, cx);
                if let Some(d) = date {
                    c.set_date(d, window, cx);
                }
                c
            });
            let mut subs = Vec::new();
            let enter_commits = |table: &mut TableState<T>,
                                 _: &Entity<InputState>,
                                 ev: &InputEvent,
                                 window: &mut Window,
                                 cx: &mut Context<TableState<T>>| {
                if let InputEvent::PressEnter { .. } = ev {
                    table.delegate_mut().commit_edit(cx);
                    table.focus_handle(cx).focus(window, cx);
                    cx.notify();
                }
            };
            subs.push(cx.subscribe_in(&input, window, enter_commits));
            // Picking a day rewrites the date part; date-only columns commit.
            let target = input.clone();
            subs.push(cx.subscribe_in(
                &calendar,
                window,
                move |table: &mut TableState<T>, _, ev: &CalendarEvent, window, cx| {
                    let CalendarEvent::Selected(Date::Single(Some(d))) = ev else {
                        return;
                    };
                    let d = *d;
                    target.update(cx, |st, cx| {
                        let v = replace_parts(&st.value(), Some(d), None, with_time);
                        st.set_value(v, window, cx);
                    });
                    if !with_time {
                        table.delegate_mut().commit_edit(cx);
                        table.focus_handle(cx).focus(window, cx);
                    }
                    cx.notify();
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
                    move |table: &mut TableState<T>,
                          time: &Entity<InputState>,
                          ev: &InputEvent,
                          window,
                          cx| {
                        match ev {
                            InputEvent::Change => {
                                let tv = time.read(cx).value().to_string();
                                target.update(cx, |st, cx| {
                                    let v = replace_parts(&st.value(), None, Some(tv), true);
                                    st.set_value(v, window, cx);
                                });
                            }
                            InputEvent::PressEnter { .. } => {
                                table.delegate_mut().commit_edit(cx);
                                table.focus_handle(cx).focus(window, cx);
                                cx.notify();
                            }
                            _ => {}
                        }
                    },
                ));
                t
            });
            input.read(cx).focus_handle(cx).focus(window, cx);
            input.update(cx, |st, cx| st.select_all(window, cx));
            return Self {
                row,
                col,
                right,
                widget: Widget::DateTime {
                    input,
                    calendar,
                    time,
                },
                _subs: subs,
            };
        }

        let input = cx.new(|cx| {
            let mut st = InputState::new(window, cx);
            st.set_value(text, window, cx);
            st
        });
        let sub = cx.subscribe_in(
            &input,
            window,
            |table: &mut TableState<T>, _, ev: &InputEvent, window, cx| match ev {
                InputEvent::PressEnter { .. } => {
                    table.delegate_mut().commit_edit(cx);
                    table.focus_handle(cx).focus(window, cx);
                    cx.notify();
                }
                InputEvent::Blur => {
                    table.delegate_mut().commit_edit(cx);
                    cx.notify();
                }
                _ => {}
            },
        );
        input.read(cx).focus_handle(cx).focus(window, cx);
        // Editing starts with the whole value selected: typing replaces it.
        input.update(cx, |st, cx| st.select_all(window, cx));
        Self {
            row,
            col,
            right,
            widget: Widget::Text(input),
            _subs: vec![sub],
        }
    }

    /// Current value; `None` when a picker has nothing chosen yet.
    pub fn value(&self, cx: &App) -> Option<EditValue> {
        match &self.widget {
            Widget::Text(input) => Some(EditValue::Text(input.read(cx).value().to_string())),
            Widget::Choice { state, picked } => {
                let value = match picked.borrow().clone() {
                    Some(v) => v?,
                    None => state.read(cx).selected_value()?.clone(),
                };
                match value.as_ref() {
                    NULL_LABEL => Some(EditValue::Null),
                    v => Some(EditValue::Text(v.to_string())),
                }
            }
            Widget::DateTime { input, .. } => {
                let v = input.read(cx).value().trim().to_string();
                Some(if v.is_empty() {
                    EditValue::Null
                } else {
                    EditValue::Text(v)
                })
            }
        }
    }

    pub fn render(&self) -> AnyElement {
        match &self.widget {
            Widget::Text(input) => Input::new(input)
                .xsmall()
                .appearance(false)
                .bordered(false)
                .w_full()
                .when(self.right, |i| i.text_align(TextAlign::Right))
                .text_size(px(crate::settings::table_text()))
                .line_height(px(crate::settings::table_line_height()))
                .h(px(crate::settings::table_line_height() + 4.))
                .font_family(crate::settings::table_font())
                .into_any_element(),
            Widget::Choice { state, .. } => Select::new(state)
                // Menu rows at a readable size; the trigger text follows the
                // table font below.
                .small()
                .appearance(false)
                .w_full()
                .text_size(px(crate::settings::table_text()))
                .font_family(crate::settings::table_font())
                .menu_width(px(200.))
                .into_any_element(),
            Widget::DateTime {
                input,
                calendar,
                time,
            } => {
                let (cal, time) = (calendar.clone(), time.clone());
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .w_full()
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(input)
                                .xsmall()
                                .appearance(false)
                                .bordered(false)
                                .w_full()
                                .text_size(px(crate::settings::table_text()))
                                .font_family(crate::settings::table_font()),
                        ),
                    )
                    .child(
                        Popover::new("cell-datetime")
                            .anchor(Anchor::TopRight)
                            .trigger(
                                Button::new("cell-datetime-cal")
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Calendar),
                            )
                            .content(move |_, _, cx| {
                                let muted = cx.theme().muted_foreground;
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_2()
                                    .child(Calendar::new(&cal))
                                    .when_some(time.clone(), |this, t| {
                                        this.child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap_2()
                                                .px_1()
                                                .child(
                                                    div().text_sm().text_color(muted).child("Time"),
                                                )
                                                .child(
                                                    div().flex_1().child(
                                                        Input::new(&t).small().font_family(
                                                            crate::settings::table_font(),
                                                        ),
                                                    ),
                                                ),
                                        )
                                    })
                            }),
                    )
                    .into_any_element()
            }
        }
    }
}

/// Rewrite the date and/or time part of `YYYY-MM-DD HH:MM:SS[.f][zone]`,
/// keeping whatever part isn't replaced (and the UTC offset).
pub(crate) fn replace_parts(
    current: &str,
    date: Option<NaiveDate>,
    time: Option<String>,
    with_time: bool,
) -> String {
    let (d0, suffix) = split_date(current.trim());
    let (t0, zone) = split_time(&suffix);
    let day = date
        .or(d0)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default();
    if !with_time {
        return day;
    }
    let t = time.unwrap_or(if t0.is_empty() { "00:00:00".into() } else { t0 });
    format!("{day} {}{zone}", t.trim())
}

/// `T11:41:16.320+00:00` → (`11:41:16.320`, `+00:00`).
pub(crate) fn split_time(suffix: &str) -> (String, String) {
    let rest = suffix.trim_start_matches(['T', ' ']);
    match rest.find(['+', '-', 'Z']) {
        Some(i) => (rest[..i].to_string(), rest[i..].to_string()),
        None => (rest.to_string(), String::new()),
    }
}

/// Resolve an editor's value against the cell's original: `None` = unchanged
/// (drop any pending change), `Some(v)` = pending value (`None` = NULL).
pub fn change_for(value: Option<EditValue>, original: Option<&Value>) -> Option<Option<String>> {
    let orig_null = original.is_none_or(Value::is_null);
    match value? {
        EditValue::Null if orig_null => None,
        EditValue::Null => Some(None),
        EditValue::Text(t) => {
            let unchanged = if orig_null {
                t.is_empty()
            } else {
                original.map(cell_text).is_some_and(|o| o == t)
            };
            (!unchanged).then_some(Some(t))
        }
    }
}

/// Why `text` can't go in a numeric column (`int4`, `numeric`, SQLite
/// `INTEGER`…), or `None` when it can. Other types are left to the server.
pub fn invalid_for(sql_type: &str, text: &str) -> Option<String> {
    let ty = sql_type.to_ascii_lowercase();
    let t = text.trim();
    let is_int = ["int", "serial"].iter().any(|k| ty.contains(k))
        && !ty.contains("interval")
        && !ty.contains("point");
    let is_num = ["numeric", "decimal", "real", "float", "double", "money"]
        .iter()
        .any(|k| ty.contains(k));
    if is_int && !ty.starts_with('_') && !ty.ends_with("[]") {
        let digits = t.strip_prefix(['-', '+']).unwrap_or(t);
        if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
            return Some(format!("“{text}” isn't a whole number ({sql_type})."));
        }
    } else if is_num && !ty.starts_with('_') && !ty.ends_with("[]") && t.parse::<f64>().is_err() {
        return Some(format!("“{text}” isn't a number ({sql_type})."));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{EditValue, change_for, invalid_for, split_date, split_time};

    #[test]
    fn numeric_columns_reject_text() {
        assert!(invalid_for("integer", "QA").is_some());
        assert!(invalid_for("int4", "").is_some());
        assert!(invalid_for("INTEGER", "-42").is_none());
        assert!(invalid_for("numeric(10,2)", "3.5").is_none());
        assert!(invalid_for("double precision", "x").is_some());
        assert!(invalid_for("text", "QA").is_none());
        assert!(invalid_for("interval", "1 day").is_none());
        assert!(invalid_for("_int4", "{1,2}").is_none());
    }
    use serde_json::json;

    #[test]
    fn date_split_keeps_time_and_zone() {
        let (d, rest) = split_date("2026-09-05T11:41:16.320+00:00");
        assert_eq!(d.unwrap().to_string(), "2026-09-05");
        assert_eq!(rest, "T11:41:16.320+00:00");
        assert_eq!(split_date("nope").0, None);
        assert_eq!(
            split_time("T11:41:16.320+00:00"),
            ("11:41:16.320".to_string(), "+00:00".to_string())
        );
        assert_eq!(
            split_time(" 09:00:00"),
            ("09:00:00".to_string(), String::new())
        );
    }

    #[test]
    fn change_detection() {
        let orig = json!("paid");
        assert!(change_for(Some(EditValue::Text("paid".into())), Some(&orig)).is_none());
        assert_eq!(
            change_for(Some(EditValue::Text("shipped".into())), Some(&orig)),
            Some(Some("shipped".into()))
        );
        assert_eq!(change_for(Some(EditValue::Null), Some(&orig)), Some(None));
        assert!(change_for(Some(EditValue::Null), Some(&json!(null))).is_none());
        assert!(change_for(Some(EditValue::Text(String::new())), None).is_none());
        assert!(change_for(None, Some(&orig)).is_none());
    }
}
