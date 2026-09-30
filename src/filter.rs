//! Filter bar for grid tabs.
//!
//! Each row is `[✓] [column ▾] [operator ▾] [value] [Apply] [−] [+]`; the
//! column may also be "Raw SQL" (the value is then a WHERE fragment typed by
//! The user, run against their own database ). Enabled
//! rows are AND-ed. Identifiers are quoted and values travel as bind params
//! cast to the column's SQL type, so typed values never become SQL.

use gpui_kit::component::IndexPath;
use gpui_kit::component::input::InputState;
use gpui_kit::component::searchable_list::SearchableVec;
use gpui_kit::component::select::SelectState;
use gpui_kit::*;

use crate::db::{self, GridColumnMeta, WhereClause};

/// Operators in order.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FilterOp {
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    In,
    NotIn,
    IsNull,
    IsNotNull,
    Between,
    Like,
    ILike,
    Contains,
    NotContains,
    HasPrefix,
    HasSuffix,
}

impl FilterOp {
    pub const ALL: [FilterOp; 17] = [
        FilterOp::Eq,
        FilterOp::Ne,
        FilterOp::Lt,
        FilterOp::Gt,
        FilterOp::Le,
        FilterOp::Ge,
        FilterOp::In,
        FilterOp::NotIn,
        FilterOp::IsNull,
        FilterOp::IsNotNull,
        FilterOp::Between,
        FilterOp::Like,
        FilterOp::ILike,
        FilterOp::Contains,
        FilterOp::NotContains,
        FilterOp::HasPrefix,
        FilterOp::HasSuffix,
    ];

    pub fn label(self) -> &'static str {
        match self {
            FilterOp::Eq => "=",
            FilterOp::Ne => "<>",
            FilterOp::Lt => "<",
            FilterOp::Gt => ">",
            FilterOp::Le => "<=",
            FilterOp::Ge => ">=",
            FilterOp::In => "IN",
            FilterOp::NotIn => "NOT IN",
            FilterOp::IsNull => "IS NULL",
            FilterOp::IsNotNull => "IS NOT NULL",
            FilterOp::Between => "BETWEEN",
            FilterOp::Like => "LIKE",
            FilterOp::ILike => "ILIKE",
            FilterOp::Contains => "Contains",
            FilterOp::NotContains => "Not contains",
            FilterOp::HasPrefix => "Has prefix",
            FilterOp::HasSuffix => "Has suffix",
        }
    }

    pub fn needs_value(self) -> bool {
        !matches!(self, FilterOp::IsNull | FilterOp::IsNotNull)
    }
}

/// Pure description of one filter row (what [`build_where`] consumes).
#[derive(Clone, Debug, PartialEq)]
pub struct FilterSpec {
    pub enabled: bool,
    /// `None` = Raw SQL.
    pub column: Option<usize>,
    pub op: FilterOp,
    pub value: String,
}

pub(crate) fn split_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .collect()
}

/// AND all enabled rows into one WHERE clause. Rows that can't produce a
/// Predicate (empty value, unknown column) are skipped.
#[cfg(test)]
pub fn build_where(specs: &[FilterSpec], columns: &[GridColumnMeta]) -> Option<WhereClause> {
    build_where_for(specs, columns, crate::engine::Dialect::Postgres)
}

/// [`build_where`] in `d`'s SQL. Postgres binds the values as `$n`
/// parameters; the other dialects get them as escaped literals (their
/// placeholder styles differ, and the text is built from quoted parts only).
pub fn build_where_for(
    specs: &[FilterSpec],
    columns: &[GridColumnMeta],
    d: crate::engine::Dialect,
) -> Option<WhereClause> {
    use crate::engine::Dialect;
    let pg = d == Dialect::Postgres;
    let mut terms: Vec<crate::db::FilterTerm> = Vec::new();
    let mut parts: Vec<String> = Vec::new();
    let mut params: Vec<String> = Vec::new();
    for spec in specs.iter().filter(|s| s.enabled) {
        let value = spec.value.trim();
        let Some(col_ix) = spec.column else {
            if !value.is_empty() {
                parts.push(format!("({value})"));
            }
            continue;
        };
        let Some(col) = columns.get(col_ix) else {
            continue;
        };
        if spec.op.needs_value() && value.is_empty() {
            continue;
        }
        terms.push(crate::db::FilterTerm {
            column: col.name.clone(),
            op: spec.op,
            value: value.to_string(),
        });
        let ident = d.quote(&col.name);
        let text = d.text(&ident);
        let ty = &col.sql_type;
        let mut bind = |v: String| {
            if pg {
                params.push(v);
                format!("${}", params.len())
            } else {
                d.literal(&v)
            }
        };
        // Postgres compares a text parameter as the column's type; the
        // others convert a literal on their own.
        let typed = |p: String| if pg { format!("CAST({p} AS {ty})") } else { p };
        let cmp = |op: &str, p: String| format!("{ident} {op} {}", typed(p));
        let part = match spec.op {
            FilterOp::Eq => cmp("=", bind(value.to_string())),
            FilterOp::Ne => cmp("<>", bind(value.to_string())),
            FilterOp::Lt => cmp("<", bind(value.to_string())),
            FilterOp::Gt => cmp(">", bind(value.to_string())),
            FilterOp::Le => cmp("<=", bind(value.to_string())),
            FilterOp::Ge => cmp(">=", bind(value.to_string())),
            FilterOp::In | FilterOp::NotIn => {
                let items = split_list(value);
                if items.is_empty() {
                    continue;
                }
                let list: Vec<String> = items.into_iter().map(|v| typed(bind(v))).collect();
                let not = if spec.op == FilterOp::NotIn {
                    "NOT "
                } else {
                    ""
                };
                format!("{ident} {not}IN ({})", list.join(", "))
            }
            FilterOp::IsNull => format!("{ident} IS NULL"),
            FilterOp::IsNotNull => format!("{ident} IS NOT NULL"),
            FilterOp::Between => {
                let items = split_list(value);
                let [lo, hi] = items.as_slice() else {
                    continue;
                };
                let (lo, hi) = (bind(lo.clone()), bind(hi.clone()));
                format!("{ident} BETWEEN {} AND {}", typed(lo), typed(hi))
            }
            FilterOp::Like => format!("{text} LIKE {}", bind(value.to_string())),
            FilterOp::ILike => d.ilike(&text, &bind(value.to_string()), false),
            FilterOp::Contains => {
                let p = bind(value.to_string());
                d.ilike(&text, &d.concat(&["'%'", &p, "'%'"]), false)
            }
            FilterOp::NotContains => {
                let p = bind(value.to_string());
                d.ilike(&text, &d.concat(&["'%'", &p, "'%'"]), true)
            }
            FilterOp::HasPrefix => {
                let p = bind(value.to_string());
                d.ilike(&text, &d.concat(&[&p, "'%'"]), false)
            }
            FilterOp::HasSuffix => {
                let p = bind(value.to_string());
                d.ilike(&text, &d.concat(&["'%'", &p]), false)
            }
        };
        parts.push(part);
    }
    if parts.is_empty() {
        None
    } else {
        Some(WhereClause {
            sql: parts.join(" AND "),
            params,
            terms,
        })
    }
}

/// The clause with its `$n` parameters inlined (engines that can't bind
/// them the Postgres way; the literals are escaped).
pub fn inline_params(w: &WhereClause) -> String {
    display_sql(w)
}

/// The clause with its parameters inlined as literals — for display only
/// (execution always binds them).
pub fn display_sql(w: &WhereClause) -> String {
    let mut sql = w.sql.clone();
    // Highest index first so `$1` never clobbers the prefix of `$10`.
    for (i, p) in w.params.iter().enumerate().rev() {
        sql = sql.replace(&format!("${}", i + 1), &db::quote_literal(p));
    }
    sql
}

/// One live filter row in the bar: column + operator pickers (kit Selects,
/// so ⌘← / ⌘→ can open them) and the value field.
pub struct FilterRow {
    pub enabled: bool,
    pub column: Option<usize>,
    pub op: FilterOp,
    pub value: Entity<InputState>,
    /// Items: "Raw SQL", then the columns.
    pub column_select: Entity<SelectState<SearchableVec<SharedString>>>,
    pub op_select: Entity<SelectState<Vec<SharedString>>>,
}

impl FilterRow {
    pub fn new(
        column: Option<usize>,
        columns: &[String],
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        let value = cx.new(|cx| InputState::new(window, cx).placeholder("Value"));
        let prefs = crate::settings::get();
        let mut names: Vec<String> = columns.to_vec();
        if prefs.filter_column_sort == "Alphabetical" {
            names.sort_by_key(|n| n.to_lowercase());
        }
        let items: Vec<SharedString> = std::iter::once(SharedString::from("Raw SQL"))
            .chain(names.iter().map(|c| SharedString::from(c.clone())))
            .collect();
        let picked_name = column.and_then(|c| columns.get(c));
        let selected = Some(IndexPath::new(
            picked_name
                .and_then(|n| names.iter().position(|x| x == n))
                .map_or(0, |i| i + 1),
        ));
        let op = FilterOp::ALL
            .into_iter()
            .find(|o| o.label() == prefs.filter_default_operator)
            .unwrap_or(FilterOp::Eq);
        let column_select = cx.new(|cx| {
            SelectState::new(SearchableVec::new(items), selected, window, cx).searchable(true)
        });
        let ops: Vec<SharedString> = FilterOp::ALL.iter().map(|o| o.label().into()).collect();
        let op_ix = FilterOp::ALL.iter().position(|o| *o == op).unwrap_or(0);
        let op_select = cx.new(|cx| SelectState::new(ops, Some(IndexPath::new(op_ix)), window, cx));
        Self {
            enabled: prefs.filter_default_enabled,
            column,
            op,
            value,
            column_select,
            op_select,
        }
    }

    /// Picked column label → index (`None` = Raw SQL).
    pub fn set_column_label(&mut self, label: &str, columns: &[String]) {
        self.column = columns.iter().position(|c| c == label);
    }

    pub fn set_op_label(&mut self, label: &str) {
        if let Some(op) = FilterOp::ALL.into_iter().find(|o| o.label() == label) {
            self.op = op;
        }
    }

    /// Point the pickers at `column` / `op` (quick filters set them directly).
    pub fn sync_pickers(&self, columns: &[String], window: &mut Window, cx: &mut App) {
        let name: SharedString = self
            .column
            .and_then(|c| columns.get(c))
            .cloned()
            .unwrap_or_else(|| "Raw SQL".into())
            .into();
        self.column_select
            .update(cx, |s, cx| s.set_selected_value(&name, window, cx));
        let op: SharedString = self.op.label().into();
        self.op_select
            .update(cx, |s, cx| s.set_selected_value(&op, window, cx));
    }

    pub fn spec(&self, cx: &App) -> FilterSpec {
        FilterSpec {
            enabled: self.enabled,
            column: self.column,
            op: self.op,
            value: self.value.read(cx).value().to_string(),
        }
    }
}

/// Filter bar state for one grid tab.
#[derive(Default)]
pub struct FilterBar {
    pub visible: bool,
    pub rows: Vec<FilterRow>,
    /// The clause currently applied to the grid (shown as "filtered" state).
    pub applied: Option<WhereClause>,
}

#[cfg(test)]
mod tests {
    // Not `super::*`: gpui_kit's glob exports its own `test` attribute macro.
    use super::{FilterOp, FilterSpec, GridColumnMeta, build_where};

    fn col(name: &str, ty: &str) -> GridColumnMeta {
        GridColumnMeta {
            name: name.into(),
            pg_type: ty.into(),
            sql_type: ty.into(),
            nullable: true,
            default: None,
            comment: None,
            is_pk: false,
            foreign_key: None,
            enum_values: Vec::new(),
        }
    }

    fn spec(column: Option<usize>, op: FilterOp, value: &str) -> FilterSpec {
        FilterSpec {
            enabled: true,
            column,
            op,
            value: value.into(),
        }
    }

    #[test]
    fn builds_parameterised_and_clauses() {
        let cols = vec![col("id", "integer"), col("email", "text")];
        let w = build_where(
            &[
                spec(Some(0), FilterOp::Ge, "10"),
                spec(Some(1), FilterOp::Contains, "o'neil"),
                spec(Some(0), FilterOp::In, "1, 2,3"),
                spec(Some(1), FilterOp::IsNull, ""),
                spec(None, FilterOp::Eq, "id < 100"),
                // skipped: empty value / disabled
                spec(Some(1), FilterOp::Eq, "  "),
                FilterSpec {
                    enabled: false,
                    ..spec(Some(0), FilterOp::Eq, "5")
                },
            ],
            &cols,
        )
        .expect("clause");
        assert_eq!(
            w.sql,
            "\"id\" >= CAST($1 AS integer) AND \"email\"::text ILIKE '%' || $2 || '%' \
             AND \"id\" IN (CAST($3 AS integer), CAST($4 AS integer), CAST($5 AS integer)) \
             AND \"email\" IS NULL AND (id < 100)"
        );
        assert_eq!(w.params, vec!["10", "o'neil", "1", "2", "3"]);
        assert!(super::display_sql(&w).starts_with(
            "\"id\" >= CAST('10' AS integer) AND \"email\"::text ILIKE '%' || 'o''neil' || '%'"
        ));
    }

    #[test]
    fn nothing_enabled_means_no_clause() {
        let cols = vec![col("id", "integer")];
        assert_eq!(build_where(&[], &cols), None);
        assert_eq!(
            build_where(&[spec(Some(0), FilterOp::Between, "1")], &cols),
            None
        );
    }
}
