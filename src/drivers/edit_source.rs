//! Editable query results on engines without Postgres' column origins: a
//! plain single-table SELECT (no join / grouping / set operation), whose
//! result columns are the table's own columns (or renamed ones) and include
//! its whole primary key. Expressions stay read-only.

use crate::db::{EditSource, GridColumnMeta};

/// One item of the select list.
#[derive(Debug, PartialEq)]
pub enum Item {
    /// `*` / `t.*`
    Star,
    /// A plain column, as the result names it.
    Column { source: String, result: String },
    /// Anything computed.
    Expr,
}

#[derive(Debug, PartialEq)]
pub struct Select {
    pub schema: Option<String>,
    pub table: String,
    pub items: Vec<Item>,
}

/// Tokens outside strings / parentheses: (text, depth) with quoted
/// identifiers kept whole.
fn tokens(sql: &str) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut cur = String::new();
    let mut chars = sql.chars().peekable();
    let flush = |cur: &mut String, out: &mut Vec<(String, usize)>, depth: usize| {
        if !cur.is_empty() {
            out.push((std::mem::take(cur), depth));
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                flush(&mut cur, &mut out, depth);
                let mut s = String::from('\'');
                while let Some(n) = chars.next() {
                    s.push(n);
                    if n == '\'' {
                        if chars.peek() == Some(&'\'') {
                            s.push(chars.next().unwrap());
                            continue;
                        }
                        break;
                    }
                }
                out.push((s, depth));
            }
            '"' | '`' | '[' => {
                let close = if c == '[' { ']' } else { c };
                let mut s = String::from(c);
                while let Some(n) = chars.next() {
                    s.push(n);
                    if n == close {
                        if chars.peek() == Some(&close) && close != ']' {
                            s.push(chars.next().unwrap());
                            continue;
                        }
                        break;
                    }
                }
                // `a."b"` stays one token.
                cur.push_str(&s);
            }
            '(' => {
                flush(&mut cur, &mut out, depth);
                out.push(("(".into(), depth));
                depth += 1;
            }
            ')' => {
                flush(&mut cur, &mut out, depth);
                depth = depth.saturating_sub(1);
                out.push((")".into(), depth));
            }
            ',' | ';' => {
                flush(&mut cur, &mut out, depth);
                out.push((c.to_string(), depth));
            }
            c if c.is_whitespace() => flush(&mut cur, &mut out, depth),
            c => cur.push(c),
        }
    }
    flush(&mut cur, &mut out, depth);
    out
}

/// `"a"."b"` / `a.b` / `[a].[b]` → ["a", "b"]; `None` for anything that
/// isn't a plain (possibly qualified) name.
fn name_path(tok: &str) -> Option<Vec<String>> {
    let mut parts = Vec::new();
    let mut rest = tok;
    loop {
        let (part, after) = match rest.chars().next()? {
            q @ ('"' | '`' | '[') => {
                let close = if q == '[' { ']' } else { q };
                let body = &rest[1..];
                let mut end = None;
                let mut chars = body.char_indices().peekable();
                let mut out = String::new();
                while let Some((i, c)) = chars.next() {
                    if c == close {
                        if close != ']' && body[i + 1..].starts_with(close) {
                            out.push(c);
                            chars.next();
                            continue;
                        }
                        end = Some(i + 1);
                        break;
                    }
                    out.push(c);
                }
                (out, &body[end?..])
            }
            _ => {
                let end = rest.find('.').unwrap_or(rest.len());
                let p = &rest[..end];
                if p.is_empty()
                    || !p
                        .chars()
                        .all(|c| c.is_alphanumeric() || c == '_' || c == '$' || c == '-')
                {
                    return None;
                }
                (p.to_string(), &rest[end..])
            }
        };
        parts.push(part);
        match after.strip_prefix('.') {
            Some(r) if !r.is_empty() => rest = r,
            None if after.is_empty() => return Some(parts),
            _ => return None,
        }
    }
}

const STOP: &[&str] = &[
    "WHERE", "ORDER", "LIMIT", "OFFSET", "FETCH", "ALLOW", "FOR", "WINDOW", "QUALIFY", "SAMPLE",
    "SETTINGS", "FINAL",
];

/// Parse a single-table SELECT; `Err` says why a result is read-only.
pub fn parse(sql: &str) -> Result<Select, String> {
    let sql = sql.trim().trim_end_matches(';');
    let toks = tokens(sql);
    let up = |t: &str| t.to_ascii_uppercase();
    if toks.first().map(|(t, _)| up(t)).as_deref() != Some("SELECT") {
        return Err("read-only: not a plain SELECT".into());
    }
    for (t, d) in &toks {
        if *d > 0 {
            continue;
        }
        match up(t).as_str() {
            "JOIN" | "UNION" | "INTERSECT" | "EXCEPT" | "MINUS" => {
                return Err("read-only: result joins or combines several queries".into());
            }
            "GROUP" | "HAVING" => return Err("read-only: grouped result".into()),
            "DISTINCT" => return Err("read-only: DISTINCT result".into()),
            _ => {}
        }
    }
    let from = toks
        .iter()
        .position(|(t, d)| *d == 0 && up(t) == "FROM")
        .ok_or("read-only: no table in the result")?;
    // TOP n (SQL Server) before the list.
    let mut start = 1;
    if toks.get(1).is_some_and(|(t, _)| up(t) == "TOP") {
        start = 3;
        if toks.get(2).is_some_and(|(t, _)| t == "(") {
            start = toks
                .iter()
                .position(|(t, d)| t == ")" && *d == 0)
                .map_or(3, |i| i + 1);
        }
    }
    // Table reference.
    let table_tok = toks
        .get(from + 1)
        .ok_or("read-only: no table in the result")?;
    if table_tok.0 == "(" {
        return Err("read-only: result of a subquery".into());
    }
    let path = name_path(&table_tok.0).ok_or("read-only: no table in the result")?;
    let mut i = from + 2;
    let mut alias = None;
    if let Some((t, 0)) = toks.get(i) {
        let u = up(t);
        if u == "AS" {
            alias = toks.get(i + 1).map(|(t, _)| t.clone());
            i += 2;
        } else if !STOP.contains(&u.as_str()) && t != "," && t != ";" {
            alias = Some(t.clone());
            i += 1;
        }
    }
    if toks.get(i).is_some_and(|(t, d)| *d == 0 && t == ",") {
        return Err("read-only: result joins several tables".into());
    }
    let table = path.last().cloned().unwrap_or_default();
    let schema = (path.len() >= 2).then(|| path[path.len() - 2].clone());
    let qualifiers: Vec<String> = [Some(table.clone()), alias.clone()]
        .into_iter()
        .flatten()
        .collect();
    // Select list at depth 0 commas.
    let mut items = Vec::new();
    let mut item: Vec<&(String, usize)> = Vec::new();
    for tok in toks[start..from]
        .iter()
        .chain(std::iter::once(&(",".to_string(), 0)))
    {
        if tok.1 == 0 && tok.0 == "," {
            items.push(classify(&item, &qualifiers));
            item.clear();
        } else {
            item.push(tok);
        }
    }
    Ok(Select {
        schema,
        table,
        items,
    })
}

fn classify(item: &[&(String, usize)], qualifiers: &[String]) -> Item {
    let texts: Vec<&str> = item.iter().map(|(t, _)| t.as_str()).collect();
    let column = |t: &str| -> Option<String> {
        let p = name_path(t)?;
        match p.as_slice() {
            [c] => Some(c.clone()),
            [q, c] if qualifiers.iter().any(|x| x.eq_ignore_ascii_case(q)) => Some(c.clone()),
            _ => None,
        }
    };
    match texts.as_slice() {
        ["*"] => Item::Star,
        [t] if t.ends_with(".*") => Item::Star,
        [t] => match column(t) {
            Some(c) => Item::Column {
                result: c.clone(),
                source: c,
            },
            None => Item::Expr,
        },
        [t, as_, a] if as_.eq_ignore_ascii_case("AS") => match (column(t), name_path(a)) {
            (Some(c), Some(p)) if p.len() == 1 => Item::Column {
                source: c,
                result: p[0].clone(),
            },
            _ => Item::Expr,
        },
        [t, a] => match (column(t), name_path(a)) {
            (Some(c), Some(p)) if p.len() == 1 => Item::Column {
                source: c,
                result: p[0].clone(),
            },
            _ => Item::Expr,
        },
        _ => Item::Expr,
    }
}

/// A single-table SELECT's edit source, reading the table's columns with
/// `columns(schema, table)`. Unquoted names fold to upper case on some
/// engines (Oracle, Snowflake) and lower on others: the exact name is tried
/// first, then the folded ones.
pub fn generic(
    columns: impl Fn(String, String) -> super::Fut<Vec<GridColumnMeta>>,
    sql: &str,
    schema: String,
    result: Vec<String>,
) -> super::Fut<Result<EditSource, String>> {
    let sel = match parse(sql) {
        Ok(s) => s,
        Err(e) => return Box::pin(async move { Ok(Err(e)) }),
    };
    let qualified = sel.schema.is_some();
    let base = sel.schema.clone().unwrap_or(schema);
    let fold = |f: fn(&str) -> String| {
        let s = if qualified { f(&base) } else { base.clone() };
        (s.clone(), f(&sel.table), columns(s, f(&sel.table)))
    };
    let tries = vec![
        fold(|s| s.to_string()),
        fold(str::to_uppercase),
        fold(str::to_lowercase),
    ];
    Box::pin(async move {
        for (schema, table, cols) in tries {
            if let Ok(metas) = cols.await
                && !metas.is_empty()
            {
                let sel = Select { table, ..sel };
                return Ok(resolve(&sel, schema, &metas, &result));
            }
        }
        Ok(Err(format!("read-only: {} isn't a table", sel.table)))
    })
}

/// Match the result's columns (`result`, as returned) to the table's
/// (`metas`) by name through the parsed select list. A result name that
/// occurs twice, or comes from an expression, stays read-only.
pub fn resolve(
    sel: &Select,
    schema: String,
    metas: &[GridColumnMeta],
    result: &[String],
) -> Result<EditSource, String> {
    if metas.is_empty() {
        return Err(format!("read-only: {} isn't a table", sel.table));
    }
    let find = |name: &str| {
        metas
            .iter()
            .find(|m| m.name == name)
            .or_else(|| metas.iter().find(|m| m.name.eq_ignore_ascii_case(name)))
    };
    // Result name → table column, from `*` and plain / renamed columns.
    let mut available: Vec<(String, &GridColumnMeta)> = Vec::new();
    for it in &sel.items {
        match it {
            Item::Star => available.extend(metas.iter().map(|m| (m.name.clone(), m))),
            Item::Column { source, result } => {
                if let Some(m) = find(source) {
                    available.push((result.clone(), m));
                }
            }
            Item::Expr => {}
        }
    }
    let columns: Vec<Option<GridColumnMeta>> = result
        .iter()
        .map(|name| {
            let twice = result
                .iter()
                .filter(|r| r.eq_ignore_ascii_case(name))
                .count()
                > 1;
            let mut hits = available
                .iter()
                .filter(|(n, _)| n.eq_ignore_ascii_case(name));
            match (hits.next(), hits.next(), twice) {
                (Some((_, m)), None, false) => Some((*m).clone()),
                _ => None,
            }
        })
        .collect();
    let pk: Vec<&GridColumnMeta> = metas.iter().filter(|m| m.is_pk).collect();
    if pk.is_empty() {
        return Err(format!("read-only: {} has no primary key", sel.table));
    }
    let mut key = Vec::new();
    for p in pk {
        match columns
            .iter()
            .position(|c| c.as_ref().is_some_and(|c| c.name == p.name))
        {
            Some(ix) => key.push(ix),
            None => {
                return Err(format!(
                    "read-only: select the primary key ({}) to edit",
                    p.name
                ));
            }
        }
    }
    Ok(EditSource {
        schema,
        table: sel.table.clone(),
        columns,
        key,
        engine: Default::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::{Item, parse};

    #[test]
    fn selects_parse() {
        let s = parse("SELECT id, email AS e, upper(name), c.* FROM shop.customers c WHERE id > 1 ORDER BY id;").unwrap();
        assert_eq!(
            (s.schema.as_deref(), s.table.as_str()),
            (Some("shop"), "customers")
        );
        assert_eq!(
            s.items,
            [
                Item::Column {
                    source: "id".into(),
                    result: "id".into()
                },
                Item::Column {
                    source: "email".into(),
                    result: "e".into()
                },
                Item::Expr,
                Item::Star,
            ]
        );
        let s = parse(r#"SELECT TOP 100 * FROM [dbo].[people]"#).unwrap();
        assert_eq!(
            (s.schema.as_deref(), s.table.as_str(), &s.items[..]),
            (Some("dbo"), "people", &[Item::Star][..])
        );
        let s = parse("SELECT * FROM `proj`.`ds`.`t` LIMIT 5").unwrap();
        assert_eq!((s.schema.as_deref(), s.table.as_str()), (Some("ds"), "t"));
        assert!(parse("SELECT a FROM t JOIN u ON t.id = u.id").is_err());
        assert!(parse("SELECT a, count(*) FROM t GROUP BY a").is_err());
        assert!(parse("SELECT * FROM (SELECT 1) x").is_err());
        assert!(parse("SELECT * FROM a, b").is_err());
        assert!(parse("SELECT DISTINCT a FROM t").is_err());
        assert!(parse("SELECT * FROM t WHERE x IN (SELECT y FROM u JOIN v ON 1=1)").is_ok());
        assert!(parse("SELECT 'a, b' AS s, id FROM t").is_ok());
    }
}
