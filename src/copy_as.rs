//! "Copy Row As ▸ …" / export formats: TSV, CSV, JSON, SQL
//! INSERT, SQL UPDATE, Markdown. Pure functions over row values (JSON values
//! as fetched with `row_to_json`), shared by the grid menus and Export.

use serde_json::Value;

use crate::db::quote_ident;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyFormat {
    Tsv,
    Csv,
    Json,
    Insert,
    Update,
    Markdown,
}

impl CopyFormat {
    pub const ALL: [CopyFormat; 6] = [
        CopyFormat::Tsv,
        CopyFormat::Csv,
        CopyFormat::Json,
        CopyFormat::Insert,
        CopyFormat::Update,
        CopyFormat::Markdown,
    ];

    pub fn label(self) -> &'static str {
        match self {
            CopyFormat::Tsv => "TSV",
            CopyFormat::Csv => "CSV",
            CopyFormat::Json => "JSON",
            CopyFormat::Insert => "SQL INSERT",
            CopyFormat::Update => "SQL UPDATE",
            CopyFormat::Markdown => "Markdown",
        }
    }
}

/// Where the rows came from (for INSERT / UPDATE).
pub struct Target<'a> {
    pub schema: &'a str,
    pub table: &'a str,
    /// Column indexes of the primary key (UPDATE's WHERE). Empty = match
    /// on every column (`IS NOT DISTINCT FROM`).
    pub key: &'a [usize],
}

/// Display text of a value (NULL → empty; JSON objects compact).
pub fn plain(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// SQL literal of a value.
pub fn sql_literal(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => format!("'{}'", s.replace('\'', "''")),
        other => format!("'{}'", other.to_string().replace('\'', "''")),
    }
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn md_field(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

/// Rows in `fmt`. `header` adds the column-name line (TSV / CSV).
pub fn format_rows(
    fmt: CopyFormat,
    columns: &[String],
    rows: &[Vec<Value>],
    target: Option<&Target>,
    header: bool,
) -> String {
    let get = |row: &Vec<Value>, i: usize| row.get(i).cloned().unwrap_or(Value::Null);
    match fmt {
        CopyFormat::Tsv | CopyFormat::Csv => {
            let (sep, field): (&str, fn(&str) -> String) = if fmt == CopyFormat::Tsv {
                ("\t", |s| s.replace(['\t', '\n'], " "))
            } else {
                (",", csv_field)
            };
            let mut out: Vec<String> = Vec::new();
            if header {
                out.push(
                    columns
                        .iter()
                        .map(|c| field(c))
                        .collect::<Vec<_>>()
                        .join(sep),
                );
            }
            for row in rows {
                out.push(
                    (0..columns.len())
                        .map(|i| field(&plain(&get(row, i))))
                        .collect::<Vec<_>>()
                        .join(sep),
                );
            }
            out.join("\n")
        }
        CopyFormat::Json => {
            let objs: Vec<Value> = rows
                .iter()
                .map(|row| {
                    Value::Object(
                        columns
                            .iter()
                            .enumerate()
                            .map(|(i, c)| (c.clone(), get(row, i)))
                            .collect(),
                    )
                })
                .collect();
            let v = if objs.len() == 1 {
                objs.into_iter().next().unwrap_or(Value::Null)
            } else {
                Value::Array(objs)
            };
            serde_json::to_string_pretty(&v).unwrap_or_default()
        }
        CopyFormat::Insert => {
            let table = target
                .map(|t| format!("{}.{}", quote_ident(t.schema), quote_ident(t.table)))
                .unwrap_or_else(|| "\"table\"".into());
            let cols = columns
                .iter()
                .map(|c| quote_ident(c))
                .collect::<Vec<_>>()
                .join(", ");
            rows.iter()
                .map(|row| {
                    let vals = (0..columns.len())
                        .map(|i| sql_literal(&get(row, i)))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("INSERT INTO {table} ({cols}) VALUES ({vals});")
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        CopyFormat::Update => {
            let Some(t) = target else {
                return String::new();
            };
            let table = format!("{}.{}", quote_ident(t.schema), quote_ident(t.table));
            rows.iter()
                .map(|row| {
                    let key: Vec<usize> = if t.key.is_empty() {
                        (0..columns.len()).collect()
                    } else {
                        t.key.to_vec()
                    };
                    let set = (0..columns.len())
                        .filter(|i| !t.key.contains(i))
                        .map(|i| {
                            format!(
                                "{} = {}",
                                quote_ident(&columns[i]),
                                sql_literal(&get(row, i))
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    let wh = key
                        .iter()
                        .map(|&i| {
                            let v = get(row, i);
                            if t.key.is_empty() {
                                format!(
                                    "{} IS NOT DISTINCT FROM {}",
                                    quote_ident(&columns[i]),
                                    sql_literal(&v)
                                )
                            } else {
                                format!("{} = {}", quote_ident(&columns[i]), sql_literal(&v))
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(" AND ");
                    format!("UPDATE {table} SET {set} WHERE {wh};")
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        CopyFormat::Markdown => {
            let mut out = vec![
                format!(
                    "| {} |",
                    columns
                        .iter()
                        .map(|c| md_field(c))
                        .collect::<Vec<_>>()
                        .join(" | ")
                ),
                format!(
                    "|{}|",
                    columns
                        .iter()
                        .map(|_| " --- ")
                        .collect::<Vec<_>>()
                        .join("|")
                ),
            ];
            for row in rows {
                out.push(format!(
                    "| {} |",
                    (0..columns.len())
                        .map(|i| md_field(&plain(&get(row, i))))
                        .collect::<Vec<_>>()
                        .join(" | ")
                ));
            }
            out.join("\n")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CopyFormat, Target, format_rows, sql_literal};
    use serde_json::json;

    fn data() -> (Vec<String>, Vec<Vec<serde_json::Value>>) {
        (
            vec!["id".into(), "name".into(), "note".into()],
            vec![vec![json!(1), json!("O'Neil, Jr"), json!(null)]],
        )
    }

    #[test]
    fn every_format() {
        let (cols, rows) = data();
        let t = Target {
            schema: "public",
            table: "people",
            key: &[0],
        };
        assert_eq!(
            format_rows(CopyFormat::Tsv, &cols, &rows, None, true),
            "id\tname\tnote\n1\tO'Neil, Jr\t"
        );
        assert_eq!(
            format_rows(CopyFormat::Csv, &cols, &rows, None, false),
            "1,\"O'Neil, Jr\","
        );
        assert_eq!(
            format_rows(CopyFormat::Insert, &cols, &rows, Some(&t), false),
            "INSERT INTO \"public\".\"people\" (\"id\", \"name\", \"note\") VALUES (1, 'O''Neil, Jr', NULL);"
        );
        assert_eq!(
            format_rows(CopyFormat::Update, &cols, &rows, Some(&t), false),
            "UPDATE \"public\".\"people\" SET \"name\" = 'O''Neil, Jr', \"note\" = NULL WHERE \"id\" = 1;"
        );
        assert!(
            format_rows(CopyFormat::Json, &cols, &rows, None, false)
                .contains("\"name\": \"O'Neil, Jr\"")
        );
        assert_eq!(
            format_rows(CopyFormat::Markdown, &cols, &rows, None, false),
            "| id | name | note |\n| --- | --- | --- |\n| 1 | O'Neil, Jr |  |"
        );
        assert_eq!(sql_literal(&json!(true)), "TRUE");
    }
}
