//! Export window: file name (+ placeholders), the fields to export, the
//! query that produces the rows, then CSV / JSON / SQL with their options.
//! Sources: tables of a schema (optionally the grid's filter) or a query
//! result already in memory. Several tables → one file each in a folder.

use crate::theme::TextCaption as _;
use std::path::PathBuf;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::theme::ActiveTheme as _;
use gpui_kit::component::{Disableable as _, Icon, Root, Sizable as _, TitleBar};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde_json::Value;

use crate::copy_as::{CopyFormat, Target, format_rows, plain};
use crate::db::{WhereClause, quote_ident};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    Csv,
    Json,
    Sql,
}

impl Format {
    const ALL: [Format; 3] = [Format::Csv, Format::Json, Format::Sql];
    fn label(self) -> &'static str {
        match self {
            Format::Csv => "CSV",
            Format::Json => "JSON",
            Format::Sql => "SQL",
        }
    }
    fn ext(self) -> &'static str {
        match self {
            Format::Csv => "csv",
            Format::Json => "json",
            Format::Sql => "sql",
        }
    }
}

/// CSV quoting.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Quote {
    IfNeeded,
    Always,
    Never,
}

impl Quote {
    fn label(self) -> &'static str {
        match self {
            Quote::IfNeeded => "Quote if needed",
            Quote::Always => "Quote all fields",
            Quote::Never => "Never quote",
        }
    }
}

/// What is being exported.
#[derive(Clone)]
pub enum Source {
    /// Tables of a schema; `filter` = the grid's applied filter (one table).
    Tables {
        schema: String,
        all: Vec<String>,
        picked: Vec<String>,
        filter: Option<WhereClause>,
    },
    /// A query result already in memory.
    Result {
        title: String,
        columns: Vec<String>,
        rows: Vec<Vec<Value>>,
    },
}

#[derive(Clone)]
pub struct Options {
    pub format: Format,
    // CSV
    pub null_empty: bool,
    pub line_break_space: bool,
    pub header: bool,
    pub delimiter: char,
    pub quote: Quote,
    pub crlf: bool,
    pub decimal_comma: bool,
    // JSON
    pub pretty: bool,
    // SQL
    pub sql_drop: bool,
    pub sql_create: bool,
    pub sql_data: bool,
    /// Rows per INSERT statement (1 = one statement per row).
    pub sql_batch: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            format: Format::Csv,
            null_empty: true,
            line_break_space: false,
            header: true,
            delimiter: ',',
            quote: Quote::IfNeeded,
            crlf: false,
            decimal_comma: false,
            pretty: true,
            sql_drop: false,
            sql_create: true,
            sql_data: true,
            sql_batch: 1,
        }
    }
}

fn csv_cell(v: &Value, o: &Options) -> String {
    let mut s = match v {
        Value::Null if o.null_empty => String::new(),
        Value::Null => "NULL".into(),
        Value::Number(n) if o.decimal_comma => n.to_string().replace('.', ","),
        other => plain(other),
    };
    if o.line_break_space {
        s = s.replace("\r\n", " ").replace(['\n', '\r'], " ");
    }
    let needs = s.contains([o.delimiter, '"', '\n', '\r']);
    match o.quote {
        Quote::Always => format!("\"{}\"", s.replace('"', "\"\"")),
        Quote::IfNeeded if needs => format!("\"{}\"", s.replace('"', "\"\"")),
        _ => s,
    }
}

/// File contents for one table / result, `columns` already narrowed to the
/// picked fields.
pub fn render(
    opts: &Options,
    target: Option<(&str, &str)>,
    create_sql: Option<&str>,
    columns: &[String],
    rows: &[Vec<Value>],
) -> String {
    match opts.format {
        Format::Csv => {
            let nl = if opts.crlf { "\r\n" } else { "\n" };
            let d = opts.delimiter.to_string();
            let mut out: Vec<String> = Vec::new();
            if opts.header {
                out.push(
                    columns
                        .iter()
                        .map(|c| csv_cell(&Value::String(c.clone()), opts))
                        .collect::<Vec<_>>()
                        .join(&d),
                );
            }
            for row in rows {
                out.push(
                    (0..columns.len())
                        .map(|i| csv_cell(row.get(i).unwrap_or(&Value::Null), opts))
                        .collect::<Vec<_>>()
                        .join(&d),
                );
            }
            out.join(nl) + nl
        }
        Format::Json => {
            let objs: Vec<Value> = rows
                .iter()
                .map(|row| {
                    Value::Object(
                        columns
                            .iter()
                            .enumerate()
                            .map(|(i, c)| {
                                let v = row.get(i).cloned().unwrap_or(Value::Null);
                                let v = if v.is_null() && opts.null_empty {
                                    Value::String(String::new())
                                } else {
                                    v
                                };
                                (c.clone(), v)
                            })
                            .collect(),
                    )
                })
                .collect();
            let v = Value::Array(objs);
            if opts.pretty {
                serde_json::to_string_pretty(&v).unwrap_or_default()
            } else {
                v.to_string()
            }
        }
        Format::Sql => {
            let mut parts: Vec<String> = Vec::new();
            if let Some((schema, table)) = target {
                let q = format!("{}.{}", quote_ident(schema), quote_ident(table));
                if opts.sql_drop {
                    parts.push(format!("DROP TABLE IF EXISTS {q};"));
                }
                if opts.sql_create
                    && let Some(c) = create_sql
                {
                    parts.push(c.to_string());
                }
            }
            if opts.sql_data && !rows.is_empty() {
                let t = target.map(|(schema, table)| Target {
                    schema,
                    table,
                    key: &[],
                });
                if opts.sql_batch <= 1 {
                    parts.push(format_rows(
                        CopyFormat::Insert,
                        columns,
                        rows,
                        t.as_ref(),
                        false,
                    ));
                } else {
                    parts.push(multi_insert(columns, rows, t.as_ref(), opts.sql_batch));
                }
            }
            parts.join("\n\n") + "\n"
        }
    }
}

/// `INSERT … VALUES (…), (…), …` with `batch` rows per statement.
fn multi_insert(
    columns: &[String],
    rows: &[Vec<Value>],
    t: Option<&Target>,
    batch: usize,
) -> String {
    let table = t
        .map(|t| format!("{}.{}", quote_ident(t.schema), quote_ident(t.table)))
        .unwrap_or_else(|| "\"table\"".into());
    let cols = columns
        .iter()
        .map(|c| quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ");
    rows.chunks(batch)
        .map(|chunk| {
            let vals = chunk
                .iter()
                .map(|row| {
                    let v = (0..columns.len())
                        .map(|i| crate::copy_as::sql_literal(row.get(i).unwrap_or(&Value::Null)))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("({v})")
                })
                .collect::<Vec<_>>()
                .join(",\n    ");
            format!("INSERT INTO {table} ({cols}) VALUES\n    {vals};")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Keep only the picked fields (in source order); empty = all.
fn narrow(
    columns: &[String],
    rows: Vec<Vec<Value>>,
    fields: &[String],
) -> (Vec<String>, Vec<Vec<Value>>) {
    if fields.is_empty() {
        return (columns.to_vec(), rows);
    }
    let keep: Vec<usize> = (0..columns.len())
        .filter(|i| fields.contains(&columns[*i]))
        .collect();
    let cols = keep.iter().map(|&i| columns[i].clone()).collect();
    let rows = rows
        .into_iter()
        .map(|r| {
            keep.iter()
                .map(|&i| r.get(i).cloned().unwrap_or(Value::Null))
                .collect()
        })
        .collect();
    (cols, rows)
}

#[derive(Default)]
struct OpenExport(Option<AnyWindowHandle>);
impl Global for OpenExport {}

pub struct ExportWindow {
    focus: FocusHandle,
    pool: Option<crate::db::Db>,
    source: Source,
    opts: Options,
    file_name: Entity<InputState>,
    /// Columns of the single table / result (for the fields picker).
    columns: Vec<String>,
    /// Picked fields; empty = all.
    fields: Vec<String>,
    /// Opened for several tables (Export Tables…): the table list stays,
    /// whatever is ticked; the fields picker joins it for a single table.
    multi: bool,
    busy: bool,
    notice: Option<(bool, String)>,
}

impl ExportWindow {
    pub fn open(pool: Option<crate::db::Db>, source: Source, cx: &mut App) {
        if let Some(handle) = cx.default_global::<OpenExport>().0 {
            let _ = cx.update_window(handle, |_, window, _| window.remove_window());
        }
        let bounds = Bounds::centered(None, size(px(560.), px(660.)), cx);
        let result = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(500.), px(480.))),
                focus: !crate::background(),
                ..TitleBar::window_options()
            },
            move |window, cx| {
                let view = cx.new(|cx| ExportWindow::new(pool, source, window, cx));
                view.read(cx).focus.clone().focus(window, cx);
                cx.new(|cx| Root::new(view, window, cx))
            },
        );
        match result {
            Ok(h) => cx.set_global(OpenExport(Some(h.into()))),
            Err(e) => log::warn!("export window failed to open: {e}"),
        }
    }

    fn new(
        pool: Option<crate::db::Db>,
        source: Source,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let base = match &source {
            Source::Tables { picked, .. } if picked.len() == 1 => picked[0].clone(),
            Source::Tables { .. } => "{table}".into(),
            Source::Result { title, .. } => title.clone(),
        };
        let file_name = cx.new(|cx| {
            let mut st = InputState::new(window, cx);
            st.set_value(base, window, cx);
            st
        });
        let columns = match &source {
            Source::Result { columns, .. } => columns.clone(),
            _ => Vec::new(),
        };
        let multi = matches!(&source, Source::Tables { picked, .. } if picked.len() != 1);
        let mut this = Self {
            focus: cx.focus_handle(),
            pool,
            multi,
            source,
            opts: Options::default(),
            file_name,
            columns,
            fields: Vec::new(),
            busy: false,
            notice: None,
        };
        this.load_columns(cx);
        this
    }

    /// One table: fetch its columns for the fields picker.
    fn load_columns(&mut self, cx: &mut Context<Self>) {
        let (Some(pool), Source::Tables { schema, picked, .. }) = (self.pool.clone(), &self.source)
        else {
            return;
        };
        if picked.len() != 1 {
            self.columns.clear();
            return;
        }
        let (schema, table) = (schema.clone(), picked[0].clone());
        cx.spawn(async move |weak, cx: &mut AsyncApp| {
            let cols = crate::db::fetch_columns(&pool, &schema, &table).await;
            let _ = weak.update(cx, |this: &mut ExportWindow, cx| {
                if let Ok(c) = cols {
                    this.columns = c.into_iter().map(|m| m.name).collect();
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn picked_count(&self) -> usize {
        match &self.source {
            Source::Tables { picked, .. } => picked.len(),
            Source::Result { .. } => 1,
        }
    }

    /// The SELECT that produces the exported rows (shown, read-only).
    fn export_query(&self) -> String {
        let cols = if self.fields.is_empty() {
            "*".to_string()
        } else {
            self.fields
                .iter()
                .map(|f| quote_ident(f))
                .collect::<Vec<_>>()
                .join(", ")
        };
        match &self.source {
            Source::Tables {
                schema,
                picked,
                filter,
                ..
            } => picked
                .iter()
                .map(|t| {
                    let mut q = format!(
                        "SELECT {cols} FROM {}.{}",
                        quote_ident(schema),
                        quote_ident(t)
                    );
                    if let Some(w) = filter {
                        q.push_str(&format!(" WHERE {}", crate::filter::display_sql(w)));
                    }
                    q + ";"
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Source::Result { title, .. } => format!("-- the rows of {title}, as shown"),
        }
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        if self.picked_count() == 0 || self.busy {
            return;
        }
        let template = self.file_name.read(cx).value().to_string();
        let now = chrono::Local::now();
        let fill = move |table: &str| {
            let n = template
                .replace("{table}", table)
                .replace("{date}", &now.format("%Y-%m-%d").to_string())
                .replace("{time}", &now.format("%H-%M-%S").to_string());
            if n.trim().is_empty() {
                table.to_string()
            } else {
                n.replace('/', "-")
            }
        };
        let many = self.picked_count() > 1;
        let ext = self.opts.format.ext();
        let first = match &self.source {
            Source::Tables { picked, .. } => picked.first().cloned().unwrap_or_default(),
            Source::Result { title, .. } => title.clone(),
        };
        let home = dirs::home_dir().unwrap_or_default().join("Downloads");
        let dest = if many {
            Dest::Dir(cx.prompt_for_paths(PathPromptOptions {
                files: false,
                directories: true,
                multiple: false,
                prompt: Some("Export Here".into()),
            }))
        } else {
            let name = format!("{}.{ext}", fill(&first));
            Dest::File(cx.prompt_for_new_path(&home, Some(&name)))
        };
        let (pool, source, opts, fields) = (
            self.pool.clone(),
            self.source.clone(),
            self.opts.clone(),
            self.fields.clone(),
        );
        cx.spawn(async move |weak, cx: &mut AsyncApp| {
            let target: Option<PathBuf> = match dest {
                Dest::Dir(rx) => rx
                    .await
                    .ok()
                    .and_then(|r| r.ok())
                    .flatten()
                    .and_then(|v| v.into_iter().next()),
                Dest::File(rx) => rx.await.ok().and_then(|r| r.ok()).flatten(),
            };
            let Some(target) = target else { return };
            let _ = weak.update(cx, |this: &mut ExportWindow, cx| {
                this.busy = true;
                this.notice = Some((true, "Exporting…".into()));
                cx.notify();
            });
            let result = run_export(pool, source, opts, fields, target.clone(), many, fill).await;
            let _ = weak.update(cx, |this: &mut ExportWindow, cx| {
                this.busy = false;
                this.notice = Some(match result {
                    Ok(n) => {
                        cx.reveal_path(&target);
                        (true, format!("Exported {n} rows to {}", target.display()))
                    }
                    Err(e) => (false, e),
                });
                cx.notify();
            });
        })
        .detach();
    }
}

enum Dest {
    Dir(futures::channel::oneshot::Receiver<anyhow::Result<Option<Vec<PathBuf>>>>),
    File(futures::channel::oneshot::Receiver<anyhow::Result<Option<PathBuf>>>),
}

/// Fetch + render + write; returns the number of exported rows.
async fn run_export(
    pool: Option<crate::db::Db>,
    source: Source,
    opts: Options,
    fields: Vec<String>,
    target: PathBuf,
    many: bool,
    fill: impl Fn(&str) -> String,
) -> Result<usize, String> {
    let write = |path: PathBuf, text: String| {
        std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))
    };
    match source {
        Source::Result { columns, rows, .. } => {
            let (cols, rows) = narrow(&columns, rows, &fields);
            write(target, render(&opts, None, None, &cols, &rows))?;
            Ok(rows.len())
        }
        Source::Tables {
            schema,
            picked,
            filter,
            ..
        } => {
            let pool = pool.ok_or("Not connected.")?;
            let mut total = 0;
            for table in &picked {
                let (columns, rows) =
                    crate::objects::fetch_all_rows(&pool, &schema, table, filter.clone()).await?;
                let (columns, rows) = narrow(&columns, rows, &fields);
                let create = if opts.format == Format::Sql && opts.sql_create {
                    Some(
                        crate::objects::script(
                            &pool,
                            crate::objects::ObjKind::Table,
                            &schema,
                            table,
                            crate::objects::Script::Create,
                        )
                        .await
                        .unwrap_or_default(),
                    )
                } else {
                    None
                };
                let text = render(
                    &opts,
                    Some((&schema, table)),
                    create.as_deref(),
                    &columns,
                    &rows,
                );
                let path = if many {
                    target.join(format!("{}.{}", fill(table), opts.format.ext()))
                } else {
                    target.clone()
                };
                write(path, text)?;
                total += rows.len();
            }
            Ok(total)
        }
    }
}

/// `label  [control]` row of the options card (label right-aligned).
fn opt_row(label: &'static str, control: impl IntoElement, fg: Hsla) -> Div {
    div()
        .flex()
        .items_center()
        .gap_3()
        .child(
            div()
                .w(px(96.))
                .flex_none()
                .text_right()
                .text_sm()
                .text_color(fg)
                .child(label),
        )
        .child(div().w(px(180.)).child(control))
}

type SetOpt = fn(&mut Options);

impl ExportWindow {
    fn render_inner(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let (bg, fg, muted, border, card, accent) = (
            t.background,
            t.foreground,
            t.muted_foreground,
            t.border,
            t.group_box,
            t.accent,
        );
        let (ok_c, err_c) = (t.green, t.red);
        let label = |s: &str| div().text_sm().text_color(fg).pb_1().child(s.to_string());
        let view = cx.entity();

        let title = match &self.source {
            Source::Tables { picked, filter, .. } if picked.len() == 1 && filter.is_some() => {
                "Export filter result".to_string()
            }
            Source::Tables { picked, .. } if picked.len() == 1 => format!("Export {}", picked[0]),
            Source::Tables { picked, .. } => format!("Export {} tables", picked.len()),
            Source::Result { .. } => "Export query result".to_string(),
        };

        // File name + Customize (placeholders).
        let v = view.clone();
        let file_row = div()
            .flex()
            .items_center()
            .gap_2()
            .child(div().text_sm().text_color(fg).child("File name:"))
            .child(div().flex_1().child(Input::new(&self.file_name).small()))
            .child(
                Button::new("exp-customize")
                    .label("Customize")
                    .small()
                    .dropdown_menu(move |mut menu, _, _| {
                        for (l, tok) in [
                            ("Table name", "{table}"),
                            ("Date (YYYY-MM-DD)", "{date}"),
                            ("Time (HH-MM-SS)", "{time}"),
                        ] {
                            let v = v.clone();
                            menu =
                                menu.item(PopupMenuItem::new(l).on_click(move |_, window, cx| {
                                    v.update(cx, |this, cx| {
                                        this.file_name.update(cx, |st, cx| {
                                            let cur = st.value().to_string();
                                            let sep = if cur.is_empty() || cur.ends_with('_') {
                                                ""
                                            } else {
                                                "_"
                                            };
                                            st.set_value(format!("{cur}{sep}{tok}"), window, cx);
                                        });
                                    });
                                }));
                        }
                        menu
                    }),
            );

        // Export Tables… → table list (kept while ticking); exactly one
        // table / a result → field chips (under the list when both).
        let single = match &self.source {
            Source::Tables { picked, .. } => picked.len() == 1,
            Source::Result { .. } => true,
        };
        let tables_picker: Option<AnyElement> =
            if let (true, Source::Tables { all, picked, .. }) = (self.multi, &self.source) {
                let mut list = div().flex().flex_col().gap_1();
                for (i, name) in all.iter().enumerate() {
                    let (checked, n2) = (picked.contains(name), name.clone());
                    list = list.child(
                        Checkbox::new(SharedString::from(format!("exp-t-{i}")))
                            .label(name.clone())
                            .checked(checked)
                            .on_click(cx.listener(move |this, on: &bool, _, cx| {
                                if let Source::Tables { picked, .. } = &mut this.source {
                                    picked.retain(|p| *p != n2);
                                    if *on {
                                        picked.push(n2.clone());
                                    }
                                }
                                this.fields.clear();
                                this.load_columns(cx);
                                cx.notify();
                            })),
                    );
                }
                div()
                    .flex()
                    .flex_col()
                    .child(label("Tables to export"))
                    .child(
                        div()
                            .id("exp-tables")
                            .max_h(px(150.))
                            .overflow_y_scroll()
                            .p_2()
                            .rounded(crate::theme::RADIUS_MD)
                            .border_1()
                            .border_color(border)
                            .bg(card)
                            .child(list),
                    )
                    .into_any_element()
                    .into()
            } else {
                None
            };
        let fields_picker: Option<AnyElement> = if single {
            // Field chips: highlighted = exported; click to toggle.
            let mut chips = div().flex().flex_wrap().gap_1();
            for (i, c) in self.columns.iter().enumerate() {
                let on = self.fields.is_empty() || self.fields.contains(c);
                let name = c.clone();
                chips = chips.child(
                    div()
                        .id(("exp-field", i))
                        .px_1p5()
                        .h(px(20.))
                        .flex()
                        .items_center()
                        .rounded(crate::theme::RADIUS_SM)
                        .text_caption()
                        .font_family(crate::settings::table_font())
                        .when(on, |d| d.bg(accent.opacity(0.22)).text_color(fg))
                        .when(!on, |d| d.border_1().border_color(border).text_color(muted))
                        .child(c.clone())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if this.fields.is_empty() {
                                this.fields = this.columns.clone();
                            }
                            match this.fields.iter().position(|f| *f == name) {
                                Some(p) => {
                                    this.fields.remove(p);
                                }
                                None => this.fields.push(name.clone()),
                            }
                            if this.fields.len() == this.columns.len() {
                                this.fields.clear();
                            }
                            cx.notify();
                        })),
                );
            }
            let hint = if self.fields.is_empty() {
                "All fields".to_string()
            } else {
                format!("{} of {} fields", self.fields.len(), self.columns.len())
            };
            div()
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(label("Select fields to export"))
                        .child(div().text_caption().text_color(muted).child(hint)),
                )
                .child(
                    div()
                        .id("exp-fields")
                        .max_h(px(96.))
                        .overflow_y_scroll()
                        .p_2()
                        .rounded(crate::theme::RADIUS_MD)
                        .border_1()
                        .border_color(border)
                        .bg(card)
                        .child(chips),
                )
                .into_any_element()
                .into()
        } else {
            None
        };

        let query = div().flex().flex_col().child(label("Export query")).child(
            div()
                .id("exp-query")
                .max_h(px(64.))
                .overflow_y_scroll()
                .p_2()
                .rounded(crate::theme::RADIUS_MD)
                .border_1()
                .border_color(border)
                .bg(card)
                .text_caption()
                .font_family(crate::settings::table_font())
                .text_color(muted)
                .child(self.export_query()),
        );

        // Format tabs.
        let mut tabs = div()
            .flex()
            .gap_0p5()
            .p_0p5()
            .rounded(crate::theme::RADIUS_MD)
            .bg(fg.opacity(0.06));
        for f in Format::ALL {
            let active = self.opts.format == f;
            tabs = tabs.child(
                div()
                    .id(SharedString::from(format!("fmt-{}", f.label())))
                    .px_3()
                    .h(px(22.))
                    .flex()
                    .items_center()
                    .rounded(crate::theme::RADIUS_SM)
                    .text_caption()
                    .text_color(if active { fg } else { muted })
                    .when(active, |d| d.bg(fg.opacity(0.12)))
                    .when(!active, |d| d.hover(|d| d.text_color(fg)))
                    .child(f.label())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.opts.format = f;
                        cx.notify();
                    })),
            );
        }

        let o = self.opts.clone();
        let check = |id: &'static str, text: &'static str, on: bool, f: fn(&mut Options, bool)| {
            Checkbox::new(id)
                .label(text)
                .checked(on)
                .on_click(
                    cx.listener(move |this: &mut ExportWindow, v: &bool, _, cx| {
                        f(&mut this.opts, *v);
                        cx.notify();
                    }),
                )
        };
        let pick = |id: &'static str, current: String, options: Vec<(&'static str, SetOpt)>| {
            let v = cx.entity();
            Button::new(id)
                .label(current)
                .small()
                .w_full()
                .dropdown_caret(true)
                .dropdown_menu(move |mut menu, _, _| {
                    for (text, set) in options.clone() {
                        let v = v.clone();
                        menu = menu.item(PopupMenuItem::new(text).on_click(move |_, _, cx| {
                            v.update(cx, |this, cx| {
                                set(&mut this.opts);
                                cx.notify();
                            });
                        }));
                    }
                    menu
                })
        };
        let options: AnyElement = match o.format {
            Format::Csv => {
                let delim = match o.delimiter {
                    '\t' => "Tab".to_string(),
                    c => c.to_string(),
                };
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(check(
                        "exp-null",
                        "Convert NULL to EMPTY",
                        o.null_empty,
                        |o, v| o.null_empty = v,
                    ))
                    .child(check(
                        "exp-lb",
                        "Convert line break to space",
                        o.line_break_space,
                        |o, v| o.line_break_space = v,
                    ))
                    .child(check(
                        "exp-header",
                        "Put field names in the first row",
                        o.header,
                        |o, v| o.header = v,
                    ))
                    .child(div().h(px(4.)))
                    .child(opt_row(
                        "Delimiter",
                        pick(
                            "exp-delim",
                            delim,
                            vec![
                                (",", |o| o.delimiter = ','),
                                (";", |o| o.delimiter = ';'),
                                ("Tab", |o| o.delimiter = '\t'),
                                ("|", |o| o.delimiter = '|'),
                            ],
                        ),
                        fg,
                    ))
                    .child(opt_row(
                        "Quoting",
                        pick(
                            "exp-quote",
                            o.quote.label().into(),
                            vec![
                                ("Quote if needed", |o| o.quote = Quote::IfNeeded),
                                ("Quote all fields", |o| o.quote = Quote::Always),
                                ("Never quote", |o| o.quote = Quote::Never),
                            ],
                        ),
                        fg,
                    ))
                    .child(opt_row(
                        "Line break",
                        pick(
                            "exp-nl",
                            if o.crlf { "\\r\\n" } else { "\\n" }.into(),
                            vec![("\\n", |o| o.crlf = false), ("\\r\\n", |o| o.crlf = true)],
                        ),
                        fg,
                    ))
                    .child(opt_row(
                        "Decimal",
                        pick(
                            "exp-dec",
                            if o.decimal_comma { "," } else { "." }.into(),
                            vec![
                                (".", |o| o.decimal_comma = false),
                                (",", |o| o.decimal_comma = true),
                            ],
                        ),
                        fg,
                    ))
                    .into_any_element()
            }
            Format::Json => div()
                .flex()
                .flex_col()
                .gap_2()
                .child(check(
                    "exp-null-j",
                    "Convert NULL to EMPTY",
                    o.null_empty,
                    |o, v| o.null_empty = v,
                ))
                .child(check("exp-pretty", "Pretty print", o.pretty, |o, v| {
                    o.pretty = v
                }))
                .into_any_element(),
            Format::Sql => div()
                .flex()
                .flex_col()
                .gap_2()
                .child(check(
                    "exp-drop",
                    "Include DROP TABLE statement",
                    o.sql_drop,
                    |o, v| o.sql_drop = v,
                ))
                .child(check(
                    "exp-create",
                    "Include CREATE TABLE statement",
                    o.sql_create,
                    |o, v| o.sql_create = v,
                ))
                .child(check(
                    "exp-data",
                    "Include content (INSERT)",
                    o.sql_data,
                    |o, v| o.sql_data = v,
                ))
                .child(div().h(px(4.)))
                .child(opt_row(
                    "Rows / INSERT",
                    pick(
                        "exp-batch",
                        o.sql_batch.to_string(),
                        vec![
                            ("1", |o| o.sql_batch = 1),
                            ("100", |o| o.sql_batch = 100),
                            ("500", |o| o.sql_batch = 500),
                            ("1000", |o| o.sql_batch = 1000),
                        ],
                    ),
                    fg,
                ))
                .into_any_element(),
        };
        let format_card = div()
            .flex()
            .flex_col()
            .items_center()
            .gap_3()
            .p_3()
            .rounded(crate::theme::RADIUS_LG)
            .border_1()
            .border_color(border)
            .bg(card)
            .child(tabs)
            .child(options);

        let n = self.picked_count();
        div()
            .track_focus(&self.focus)
            .key_context(crate::dialog_keys::CONTEXT)
            .on_action(crate::dialog_keys::close)
            .on_action(
                cx.listener(|this, _: &crate::dialog_keys::DialogConfirm, _, cx| this.start(cx)),
            )
            .size_full()
            .flex()
            .flex_col()
            .bg(bg)
            .text_color(fg)
            .font_family(crate::settings::ui_font())
            .child(
                TitleBar::new()
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .justify_center()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .child(div().w(px(60.))),
            )
            .child(
                div()
                    .id("exp-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .px_5()
                    .pt_3()
                    .pb_4()
                    .child(file_row)
                    .children(tables_picker)
                    .children(fields_picker)
                    .child(query)
                    .child(format_card),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_5()
                    .py_3()
                    .border_t_1()
                    .border_color(border)
                    .child(div().flex_1().min_w_0().children(self.notice.clone().map(
                        |(ok, msg)| {
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .text_caption()
                                .text_color(if ok { ok_c } else { err_c })
                                .child(
                                    Icon::new(if ok {
                                        IconName::Check
                                    } else {
                                        IconName::CircleAlert
                                    })
                                    .size(px(12.)),
                                )
                                .child(msg)
                        },
                    )))
                    .child(
                        Button::new("exp-cancel")
                            .label("Cancel")
                            .small()
                            .on_click(|_, window, _| window.remove_window()),
                    )
                    .child(
                        Button::new("exp-go")
                            .label(if n > 1 {
                                format!("Export {n} Tables…")
                            } else {
                                "Export…".into()
                            })
                            .small()
                            .primary()
                            .disabled(n == 0 || self.busy)
                            .on_click(cx.listener(|this, _, _, cx| this.start(cx))),
                    ),
            )
    }
}

impl Render for ExportWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Finished results go out as toasts; progress stays inline.
        if !self.busy
            && let Some((ok, msg)) = self.notice.take()
        {
            crate::toast::push_top(window, cx, Some(ok), msg);
        }
        div()
            .size_full()
            .relative()
            .child(self.render_inner(window, cx))
            .children(gpui_kit::component::Root::render_notification_layer(
                window, cx,
            ))
    }
}

#[cfg(test)]
mod tests {
    use super::{Format, Options, Quote, render};
    use serde_json::json;

    #[test]
    fn csv_options_and_sql_batches() {
        let cols = vec!["id".to_string(), "note".to_string(), "price".to_string()];
        let rows = vec![
            vec![json!(1), json!("a;b\nc"), json!(2.5)],
            vec![json!(2), json!(null), json!(3)],
        ];
        let mut o = Options {
            delimiter: ';',
            line_break_space: true,
            decimal_comma: true,
            ..Options::default()
        };
        assert_eq!(
            render(&o, None, None, &cols, &rows),
            "id;note;price\n1;\"a;b c\";2,5\n2;;3\n"
        );
        o.quote = Quote::Always;
        o.header = false;
        o.null_empty = false;
        assert!(render(&o, None, None, &cols, &rows).starts_with("\"1\";\"a;b c\""));
        o.format = Format::Sql;
        o.sql_create = false;
        o.sql_batch = 100;
        let sql = render(&o, Some(("public", "t")), None, &cols, &rows);
        assert!(sql.starts_with(
            "INSERT INTO \"public\".\"t\" (\"id\", \"note\", \"price\") VALUES\n    (1, "
        ));
        assert_eq!(sql.matches("INSERT").count(), 1);
    }
}
