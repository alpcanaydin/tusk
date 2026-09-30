//! Database-object actions behind the sidebar's right-click menu
//! : "Copy Script As ▸ …", Duplicate, Truncate, CSV import.
//! All SQL identifiers are quoted; generated scripts are text for the user.

use serde_json::Value;
use sqlx::Row;

use crate::db::{self, DbResult, GridColumnMeta, quote_ident};

/// What kind of object a script is for (mirrors `app::TableKind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjKind {
    Table,
    View,
    MatView,
    Function,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Script {
    Create,
    Select,
    Insert,
    Update,
    Delete,
    Drop,
    Truncate,
}

impl Script {
    pub fn label(self) -> &'static str {
        match self {
            Script::Create => "CREATE",
            Script::Select => "SELECT",
            Script::Insert => "INSERT",
            Script::Update => "UPDATE",
            Script::Delete => "DELETE",
            Script::Drop => "DROP",
            Script::Truncate => "TRUNCATE",
        }
    }

    /// Scripts offered for a kind, in menu order.
    pub fn for_kind(kind: ObjKind) -> &'static [Script] {
        match kind {
            ObjKind::Table => &[
                Script::Create,
                Script::Select,
                Script::Insert,
                Script::Update,
                Script::Delete,
                Script::Drop,
                Script::Truncate,
            ],
            ObjKind::View | ObjKind::MatView => &[Script::Create, Script::Select, Script::Drop],
            ObjKind::Function => &[Script::Create, Script::Select, Script::Drop],
        }
    }
}

fn qualified(schema: &str, name: &str) -> String {
    format!("{}.{}", quote_ident(schema), quote_ident(name))
}

/// Script text for an object, from the connection's own driver.
pub async fn script(
    db: &db::Db,
    kind: ObjKind,
    schema: &str,
    name: &str,
    which: Script,
) -> DbResult<String> {
    db.driver()
        .script(kind, schema.to_string(), name.to_string(), which)
        .await
}

/// Postgres: script text for an object (fetches the definition / columns it needs).
pub async fn pg_script(
    pool: &sqlx::PgPool,
    kind: ObjKind,
    schema: &str,
    name: &str,
    which: Script,
) -> DbResult<String> {
    let pool = pool.clone();
    let (schema, name) = (schema.to_string(), name.to_string());
    run(async move { script_inner(&pool, kind, &schema, &name, which).await }).await
}

async fn run<T: Send + 'static>(
    fut: impl std::future::Future<Output = DbResult<T>> + Send + 'static,
) -> DbResult<T> {
    db::runtime()
        .spawn(fut)
        .await
        .map_err(|e| format!("db task failed: {e}"))?
}

fn err(e: sqlx::Error) -> String {
    db::pg_error_message(e)
}

async fn script_inner(
    pool: &sqlx::PgPool,
    kind: ObjKind,
    schema: &str,
    name: &str,
    which: Script,
) -> DbResult<String> {
    let q = qualified(schema, name);
    if kind == ObjKind::Function {
        return match which {
            Script::Create => sqlx::query_scalar::<_, String>(
                "SELECT pg_get_functiondef(p.oid) FROM pg_proc p \
                 JOIN pg_namespace n ON n.oid = p.pronamespace \
                 WHERE n.nspname = $1 AND p.proname = $2 ORDER BY p.oid LIMIT 1",
            )
            .bind(schema)
            .bind(name)
            .fetch_one(pool)
            .await
            .map_err(err),
            Script::Drop => Ok(format!("DROP FUNCTION {q};")),
            _ => Ok(format!("SELECT * FROM {q}();")),
        };
    }
    match which {
        Script::Drop => Ok(format!(
            "DROP {} {q};",
            match kind {
                ObjKind::View => "VIEW",
                ObjKind::MatView => "MATERIALIZED VIEW",
                _ => "TABLE",
            }
        )),
        Script::Truncate => Ok(format!("TRUNCATE TABLE {q};")),
        Script::Create if kind != ObjKind::Table => {
            let def: String = sqlx::query_scalar("SELECT pg_get_viewdef($1::regclass, true)")
                .bind(&q)
                .fetch_one(pool)
                .await
                .map_err(err)?;
            let head = if kind == ObjKind::MatView {
                "CREATE MATERIALIZED VIEW"
            } else {
                "CREATE OR REPLACE VIEW"
            };
            Ok(format!("{head} {q} AS\n{}", def.trim_end()))
        }
        Script::Create => create_table(pool, schema, name).await,
        _ => {
            let cols = db::pg_fetch_columns(pool, schema, name).await?;
            Ok(dml_template(which, &q, &cols))
        }
    }
}

/// SELECT / INSERT / UPDATE / DELETE templates.
pub fn dml_template(which: Script, q: &str, cols: &[GridColumnMeta]) -> String {
    dml_template_with(crate::engine::Dialect::Postgres, which, q, cols)
}

/// [`dml_template`] in `d`'s quoting (and row limit syntax).
pub fn dml_template_with(
    d: crate::engine::Dialect,
    which: Script,
    q: &str,
    cols: &[GridColumnMeta],
) -> String {
    use crate::engine::Dialect;
    let quote_ident = |n: &str| d.quote(n);
    let names: Vec<String> = cols.iter().map(|c| quote_ident(&c.name)).collect();
    let pk: Vec<&GridColumnMeta> = cols.iter().filter(|c| c.is_pk).collect();
    let where_pk = if pk.is_empty() {
        "<condition>".to_string()
    } else {
        pk.iter()
            .map(|c| format!("{} = <{}>", quote_ident(&c.name), c.name))
            .collect::<Vec<_>>()
            .join(" AND ")
    };
    match which {
        Script::Select => match d {
            Dialect::MsSql => format!("SELECT TOP 300 {}\nFROM {q};", names.join(", ")),
            Dialect::Oracle => format!(
                "SELECT {}\nFROM {q}\nFETCH FIRST 300 ROWS ONLY;",
                names.join(", ")
            ),
            _ => format!("SELECT {}\nFROM {q}\nLIMIT 300;", names.join(", ")),
        },
        Script::Insert => format!(
            "INSERT INTO {q} ({})\nVALUES ({});",
            names.join(", "),
            cols.iter()
                .map(|c| format!("<{}>", c.name))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Script::Update => format!(
            "UPDATE {q}\nSET {}\nWHERE {where_pk};",
            cols.iter()
                .filter(|c| !c.is_pk)
                .map(|c| format!("{} = <{}>", quote_ident(&c.name), c.name))
                .collect::<Vec<_>>()
                .join(",\n    ")
        ),
        Script::Delete => format!("DELETE FROM {q}\nWHERE {where_pk};"),
        _ => String::new(),
    }
}

/// `CREATE TABLE` from the catalog: columns (type, default, NOT NULL), the
/// table constraints (PK / FK / UNIQUE / CHECK) and extra indexes.
async fn create_table(pool: &sqlx::PgPool, schema: &str, name: &str) -> DbResult<String> {
    let q = qualified(schema, name);
    let cols = db::pg_fetch_columns(pool, schema, name).await?;
    let mut lines: Vec<String> = cols
        .iter()
        .map(|c| {
            let mut l = format!("    {} {}", quote_ident(&c.name), c.sql_type);
            if let Some(d) = &c.default {
                l.push_str(&format!(" DEFAULT {d}"));
            }
            if !c.nullable {
                l.push_str(" NOT NULL");
            }
            l
        })
        .collect();
    let cons = sqlx::query(
        "SELECT conname, pg_get_constraintdef(oid) FROM pg_constraint \
         WHERE conrelid = $1::regclass ORDER BY contype = 'p' DESC, conname",
    )
    .bind(&q)
    .fetch_all(pool)
    .await
    .map_err(err)?;
    for r in &cons {
        let (n, def): (String, String) = (r.get(0), r.get(1));
        lines.push(format!("    CONSTRAINT {} {def}", quote_ident(&n)));
    }
    let mut out = format!("CREATE TABLE {q} (\n{}\n);", lines.join(",\n"));
    let idx: Vec<String> = sqlx::query_scalar(
        "SELECT indexdef FROM pg_indexes i WHERE schemaname = $1 AND tablename = $2 \
         AND NOT EXISTS (SELECT 1 FROM pg_constraint c \
                         WHERE c.conrelid = $3::regclass AND c.conname = i.indexname) \
         ORDER BY indexname",
    )
    .bind(schema)
    .bind(name)
    .bind(&q)
    .fetch_all(pool)
    .await
    .map_err(err)?;
    for i in idx {
        out.push_str(&format!("\n{i};"));
    }
    Ok(out)
}

/// First free `<name>_copy`, `<name>_copy2`… in the schema.
pub async fn free_copy_name(db: &db::Db, schema: &str, name: &str) -> DbResult<String> {
    let tree = db::fetch_objects(db, schema).await?;
    let taken = |n: &str| {
        tree.tables
            .iter()
            .chain(&tree.views)
            .chain(&tree.matviews)
            .any(|t| t.eq_ignore_ascii_case(n))
    };
    let mut n = 1;
    loop {
        let candidate = if n == 1 {
            format!("{name}_copy")
        } else {
            format!("{name}_copy{n}")
        };
        if !taken(&candidate) {
            return Ok(candidate);
        }
        n += 1;
    }
}

/// "Duplicate": the table's structure (as close as the engine allows), plus
/// the rows when `with_data`. Returns the new table's name.
pub async fn duplicate_table(
    db: &db::Db,
    schema: &str,
    name: &str,
    with_data: bool,
) -> DbResult<String> {
    use crate::engine::Dialect;
    let copy = free_copy_name(db, schema, name).await?;
    let (src, dst) = (db.qualified(schema, name), db.qualified(schema, &copy));
    let mut stmts = match db.dialect() {
        Dialect::Postgres => vec![db::Stmt::plain(format!(
            "CREATE TABLE {dst} (LIKE {src} INCLUDING ALL)"
        ))],
        Dialect::MySql => vec![db::Stmt::plain(format!("CREATE TABLE {dst} LIKE {src}"))],
        Dialect::MsSql => {
            let filter = if with_data { "" } else { " WHERE 1 = 0" };
            db::execute_batch(
                db,
                vec![db::Stmt::plain(format!(
                    "SELECT * INTO {dst} FROM {src}{filter}"
                ))],
            )
            .await?;
            return Ok(copy);
        }
        Dialect::ClickHouse => vec![db::Stmt::plain(format!("CREATE TABLE {dst} AS {src}"))],
        _ => {
            let filter = if with_data { "" } else { " WHERE 1 = 0" };
            db::execute_batch(
                db,
                vec![db::Stmt::plain(format!(
                    "CREATE TABLE {dst} AS SELECT * FROM {src}{filter}"
                ))],
            )
            .await?;
            return Ok(copy);
        }
    };
    if with_data {
        stmts.push(db::Stmt::plain(format!(
            "INSERT INTO {dst} SELECT * FROM {src}"
        )));
    }
    db::execute_batch(db, stmts).await?;
    Ok(copy)
}

/// A JSON value as a SQL literal in `d` (numbers / booleans bare, objects as JSON text).
fn json_literal(d: crate::engine::Dialect, v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => {
            if *b {
                "TRUE".into()
            } else {
                "FALSE".into()
            }
        }
        Value::Number(n) => n.to_string(),
        Value::String(s) => d.literal(s),
        other => d.literal(&other.to_string()),
    }
}

/// INSERTs of `rows` (column names + literal SQL values), 200 rows each.
fn insert_batches(
    db: &db::Db,
    schema: &str,
    name: &str,
    cols: &[String],
    rows: Vec<Vec<String>>,
) -> Vec<db::Stmt> {
    let target = db.qualified(schema, name);
    let names = cols
        .iter()
        .map(|c| db.quote(c))
        .collect::<Vec<_>>()
        .join(", ");
    rows.chunks(200)
        .map(|chunk| {
            let values = chunk
                .iter()
                .map(|r| format!("({})", r.join(", ")))
                .collect::<Vec<_>>()
                .join(",\n");
            db::Stmt::plain(format!("INSERT INTO {target} ({names}) VALUES\n{values}"))
        })
        .collect()
}

/// Import ▸ From JSON: an array of objects, one row each; only the keys
/// the file uses are inserted (other columns get their defaults).
pub async fn import_json(db: &db::Db, schema: &str, name: &str, data: Vec<u8>) -> DbResult<u64> {
    let rows: Vec<serde_json::Map<String, serde_json::Value>> =
        serde_json::from_slice(&data).map_err(|e| format!("Not a JSON array of objects: {e}"))?;
    if rows.is_empty() {
        return Ok(0);
    }
    let mut keys: Vec<String> = Vec::new();
    for r in &rows {
        for k in r.keys() {
            if !keys.contains(k) {
                keys.push(k.clone());
            }
        }
    }
    if let Some(pool) = db.pg().cloned() {
        let cols = keys
            .iter()
            .map(|k| quote_ident(k))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "INSERT INTO {t} ({cols}) SELECT {cols} FROM json_populate_recordset(NULL::{t}, $1::json)",
            t = qualified(schema, name)
        );
        let body = serde_json::to_string(&rows).map_err(|e| e.to_string())?;
        return run(async move {
            let done = sqlx::query(&sql)
                .bind(body)
                .execute(&pool)
                .await
                .map_err(err)?;
            Ok(done.rows_affected())
        })
        .await;
    }
    let d = db.dialect();
    let values: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            keys.iter()
                .map(|k| json_literal(d, r.get(k).unwrap_or(&Value::Null)))
                .collect()
        })
        .collect();
    db::execute_batch(db, insert_batches(db, schema, name, &keys, values)).await
}

/// Split CSV text into records (quoted fields, doubled quotes, CRLF).
pub fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, quoted) {
            ('"', true) if chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            ('"', true) => quoted = false,
            ('"', false) if field.is_empty() => quoted = true,
            (',', false) => row.push(std::mem::take(&mut field)),
            ('\r', false) => {}
            ('\n', false) => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            (c, _) => field.push(c),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

/// CSV import: the first line names the columns. Postgres streams it
/// through `COPY … FROM STDIN`; other engines get INSERT batches (empty
/// fields become NULL).
pub async fn import_csv(db: &db::Db, schema: &str, name: &str, data: Vec<u8>) -> DbResult<u64> {
    let header = String::from_utf8_lossy(data.split(|b| *b == b'\n').next().unwrap_or_default())
        .trim_end_matches('\r')
        .to_string();
    if header.trim().is_empty() {
        return Err("The CSV file has no header line.".into());
    }
    if let Some(pool) = db.pg().cloned() {
        let cols: Vec<String> = header
            .split(',')
            .map(|c| quote_ident(c.trim().trim_matches('"')))
            .collect();
        let stmt = format!(
            "COPY {} ({}) FROM STDIN WITH (FORMAT csv, HEADER true)",
            qualified(schema, name),
            cols.join(", ")
        );
        return run(async move {
            let mut conn = pool.acquire().await.map_err(err)?;
            let mut copy = conn.copy_in_raw(&stmt).await.map_err(err)?;
            if let Err(e) = copy.send(data).await {
                let _ = copy.abort("send failed").await;
                return Err(err(e));
            }
            copy.finish().await.map_err(err)
        })
        .await;
    }
    let text = String::from_utf8_lossy(&data).to_string();
    let mut records = parse_csv(&text).into_iter();
    let cols: Vec<String> = records
        .next()
        .unwrap_or_default()
        .into_iter()
        .map(|c| c.trim().to_string())
        .collect();
    let d = db.dialect();
    let rows: Vec<Vec<String>> = records
        .filter(|r| r.iter().any(|f| !f.is_empty()))
        .map(|r| {
            (0..cols.len())
                .map(|i| match r.get(i) {
                    Some(f) if !f.is_empty() => d.literal(f),
                    _ => "NULL".to_string(),
                })
                .collect()
        })
        .collect();
    db::execute_batch(db, insert_batches(db, schema, name, &cols, rows)).await
}

/// Every row of a table (for Export), as JSON values in column order.
pub async fn fetch_all_rows(
    pool: &db::Db,
    schema: &str,
    name: &str,
    where_sql: Option<db::WhereClause>,
) -> DbResult<(Vec<String>, Vec<Vec<Value>>)> {
    let cols = db::fetch_columns(pool, schema, name).await?;
    let names: Vec<String> = cols.iter().map(|c| c.name.clone()).collect();
    // Primary-key order, like the grid, so exports don't come out in heap order.
    let order = crate::grid::pk_order(pool.dialect(), &cols);
    let rows = db::fetch_window(
        pool,
        schema,
        name,
        where_sql.as_ref(),
        order.as_deref(),
        false,
        i64::MAX,
        0,
    )
    .await?;
    Ok((names, crate::sql::rows_to_vec(rows)))
}

#[cfg(test)]
mod tests {
    use super::{Script, dml_template};
    use crate::db::GridColumnMeta;

    fn col(name: &str, pk: bool) -> GridColumnMeta {
        GridColumnMeta {
            name: name.into(),
            pg_type: "int4".into(),
            sql_type: "integer".into(),
            nullable: true,
            default: None,
            comment: None,
            is_pk: pk,
            foreign_key: None,
            enum_values: Vec::new(),
        }
    }

    #[test]
    fn dml_templates_key_on_the_primary_key() {
        let cols = [col("id", true), col("qty", false)];
        let q = "\"public\".\"t\"";
        assert_eq!(
            dml_template(Script::Select, q, &cols),
            "SELECT \"id\", \"qty\"\nFROM \"public\".\"t\"\nLIMIT 300;"
        );
        assert_eq!(
            dml_template(Script::Update, q, &cols),
            "UPDATE \"public\".\"t\"\nSET \"qty\" = <qty>\nWHERE \"id\" = <id>;"
        );
        assert_eq!(
            dml_template(Script::Delete, q, &cols),
            "DELETE FROM \"public\".\"t\"\nWHERE \"id\" = <id>;"
        );
    }
}

// ---- sidebar table groups ----

/// A user folder in the sidebar: object keys `kind:name` (e.g. `table:orders`).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ObjectGroup {
    pub name: String,
    pub members: Vec<String>,
}

type GroupFile = std::collections::BTreeMap<String, Vec<ObjectGroup>>;

fn groups_path() -> std::path::PathBuf {
    db::connections_path().with_file_name("sidebar_groups.json")
}

fn read_groups() -> GroupFile {
    std::fs::read_to_string(groups_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Groups of one `connection/database/schema`.
pub fn load_object_groups(scope: &str) -> Vec<ObjectGroup> {
    read_groups().remove(scope).unwrap_or_default()
}

pub fn save_object_groups(scope: &str, groups: &[ObjectGroup]) {
    let mut all = read_groups();
    if groups.is_empty() {
        all.remove(scope);
    } else {
        all.insert(scope.to_string(), groups.to_vec());
    }
    if let Ok(text) = serde_json::to_string_pretty(&all) {
        let _ = std::fs::write(groups_path(), text);
    }
}
