//! MongoDB: databases are schemas, collections are tables. Columns are
//! sampled from documents (`_id` first, as the key); the editor runs
//! shell-style calls (`db.users.find({age: {$gt: 30}}).limit(10)`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use mongodb::bson::{self, Bson, Document, doc, oid::ObjectId};
use mongodb::options::{ClientOptions, Credential, ServerAddress};
use mongodb::{Client, Collection};
use serde_json::{Value, json};

use super::{Db, Driver, Fut, GridOp, WindowReq};
use crate::db::{
    self, DbResult, FilterTerm, GridColumnMeta, IndexDef, ObjectTree, SavedConnection, Stmt,
    WhereClause,
};
use crate::engine::Engine;
use crate::filter::FilterOp;
use crate::objects::{ObjKind, Script};

/// Documents read to infer a collection's columns.
const SAMPLE: i64 = 200;

#[derive(Clone)]
pub struct Mongo {
    client: Client,
    /// The database the sidebar shows: editor calls run there.
    current: Arc<Mutex<String>>,
    /// Sampled columns (name, type) per `db.collection`.
    shapes: super::Shapes,
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

pub async fn connect(
    conn: &SavedConnection,
    host: String,
    port: u16,
    password: String,
) -> DbResult<Db> {
    let url = conn
        .path
        .clone()
        .filter(|p| p.starts_with("mongodb://") || p.starts_with("mongodb+srv://"));
    let user = conn.user.clone();
    let db_name = if conn.database.trim().is_empty() {
        "test".to_string()
    } else {
        conn.database.trim().to_string()
    };
    let client = db::run_db(async move {
        let mut opts = match url {
            Some(u) => ClientOptions::parse(u).await.map_err(err)?,
            None => {
                let mut o = ClientOptions::default();
                o.hosts = vec![ServerAddress::Tcp {
                    host,
                    port: Some(port),
                }];
                o.direct_connection = Some(true);
                o
            }
        };
        // Credentials in a URL win over the form's.
        let has_user = opts
            .credential
            .as_ref()
            .is_some_and(|c| c.username.is_some());
        if !has_user && !user.is_empty() {
            let mut c = Credential::default();
            c.username = Some(user);
            c.password = Some(password).filter(|p| !p.is_empty());
            c.source = Some("admin".into());
            opts.credential = Some(c);
        }
        opts.app_name = Some("Tusk".into());
        opts.connect_timeout = Some(std::time::Duration::from_secs(10));
        opts.server_selection_timeout = Some(std::time::Duration::from_secs(10));
        let client = Client::with_options(opts).map_err(err)?;
        client
            .database("admin")
            .run_command(doc! {"ping": 1})
            .await
            .map_err(err)?;
        Ok(client)
    })
    .await?;
    Ok(Db::new(Mongo {
        client,
        current: Arc::new(Mutex::new(db_name)),
        shapes: Default::default(),
    }))
}

// ---- BSON <-> JSON -------------------------------------------------------

/// A BSON value as the grid's JSON: ObjectIds as hex, dates as RFC 3339.
pub fn to_json(b: &Bson) -> Value {
    match b {
        Bson::Double(f) => Value::from(*f),
        Bson::String(s) => Value::String(s.clone()),
        Bson::Array(a) => Value::Array(a.iter().map(to_json).collect()),
        Bson::Document(d) => doc_json(d),
        Bson::Boolean(v) => Value::Bool(*v),
        Bson::Null | Bson::Undefined => Value::Null,
        Bson::Int32(n) => (*n).into(),
        Bson::Int64(n) => (*n).into(),
        Bson::ObjectId(o) => Value::String(o.to_hex()),
        Bson::DateTime(d) => {
            Value::String(d.try_to_rfc3339_string().unwrap_or_else(|_| d.to_string()))
        }
        Bson::Decimal128(d) => Value::String(d.to_string()),
        Bson::Binary(bin) => super::hex(&bin.bytes),
        Bson::Timestamp(t) => json!({"t": t.time, "i": t.increment}),
        Bson::RegularExpression(r) => Value::String(format!("/{}/{}", r.pattern, r.options)),
        Bson::JavaScriptCode(c) => Value::String(c.clone()),
        Bson::Symbol(s) => Value::String(s.clone()),
        other => Value::String(other.to_string()),
    }
}

fn doc_json(d: &Document) -> Value {
    Value::Object(d.iter().map(|(k, v)| (k.clone(), to_json(v))).collect())
}

/// Plain JSON as BSON (numbers: int32 / int64 / double).
fn from_json(v: &Value) -> Bson {
    match v {
        Value::Null => Bson::Null,
        Value::Bool(b) => Bson::Boolean(*b),
        Value::Number(n) => match n.as_i64() {
            Some(i) if i32::try_from(i).is_ok() => Bson::Int32(i as i32),
            Some(i) => Bson::Int64(i),
            None => Bson::Double(n.as_f64().unwrap_or(0.0)),
        },
        Value::String(s) => Bson::String(s.clone()),
        Value::Array(a) => Bson::Array(a.iter().map(from_json).collect()),
        Value::Object(m) => {
            Bson::Document(m.iter().map(|(k, v)| (k.clone(), from_json(v))).collect())
        }
    }
}

/// The BSON type name used as the column's SQL type.
fn type_name(b: &Bson) -> &'static str {
    match b {
        Bson::Double(_) => "double",
        Bson::String(_) => "string",
        Bson::Array(_) => "array",
        Bson::Document(_) => "object",
        Bson::Boolean(_) => "bool",
        Bson::Null | Bson::Undefined => "null",
        Bson::Int32(_) => "int",
        Bson::Int64(_) => "long",
        Bson::ObjectId(_) => "objectId",
        Bson::DateTime(_) => "date",
        Bson::Decimal128(_) => "decimal",
        Bson::Binary(_) => "binData",
        _ => "mixed",
    }
}

fn grid_type(t: &str) -> String {
    match t {
        "double" => "float8",
        "int" => "int4",
        "long" => "int8",
        "bool" => "bool",
        "date" => "timestamptz",
        "decimal" => "numeric",
        "object" | "array" | "mixed" => "json",
        "binData" => "bytea",
        "string" => "text",
        _ => "varchar",
    }
    .into()
}

/// Grid text as a value of the column's BSON type.
pub fn typed(text: Option<&str>, ty: &str) -> Bson {
    let Some(t) = text else {
        return Bson::Null;
    };
    let parsed = || serde_json::from_str::<Value>(t).ok();
    match ty {
        "objectId" => ObjectId::parse_str(t.trim())
            .map(Bson::ObjectId)
            .unwrap_or_else(|_| Bson::String(t.into())),
        "int" => t
            .trim()
            .parse::<i32>()
            .map(Bson::Int32)
            .unwrap_or_else(|_| Bson::String(t.into())),
        "long" => t
            .trim()
            .parse::<i64>()
            .map(Bson::Int64)
            .unwrap_or_else(|_| Bson::String(t.into())),
        "double" => t
            .trim()
            .parse::<f64>()
            .map(Bson::Double)
            .unwrap_or_else(|_| Bson::String(t.into())),
        "decimal" => t
            .trim()
            .parse::<bson::Decimal128>()
            .map(Bson::Decimal128)
            .unwrap_or_else(|_| Bson::String(t.into())),
        "bool" => match t.trim() {
            "true" | "t" | "1" => Bson::Boolean(true),
            "false" | "f" | "0" => Bson::Boolean(false),
            _ => Bson::String(t.into()),
        },
        "date" => parse_date(t)
            .map(Bson::DateTime)
            .unwrap_or_else(|| Bson::String(t.into())),
        "object" | "array" | "mixed" | "null" => parsed()
            .map(|v| from_json(&v))
            .unwrap_or_else(|| Bson::String(t.into())),
        _ => Bson::String(t.into()),
    }
}

fn parse_date(t: &str) -> Option<bson::DateTime> {
    let t = t.trim();
    if let Ok(d) = chrono::DateTime::parse_from_rfc3339(t) {
        return Some(bson::DateTime::from_millis(d.timestamp_millis()));
    }
    for f in [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(d) = chrono::NaiveDateTime::parse_from_str(t, f) {
            return Some(bson::DateTime::from_millis(d.and_utc().timestamp_millis()));
        }
    }
    chrono::NaiveDate::parse_from_str(t, "%Y-%m-%d")
        .ok()
        .map(|d| {
            bson::DateTime::from_millis(
                d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_millis(),
            )
        })
}

// ---- Shell syntax --------------------------------------------------------

/// A relaxed JavaScript literal parser: unquoted keys, single quotes,
/// trailing commas, `ObjectId("…")`, `ISODate("…")`, `NumberLong(…)`,
/// `/regex/i`.
struct Js<'a> {
    s: &'a [u8],
    i: usize,
}

impl<'a> Js<'a> {
    fn new(s: &'a str) -> Self {
        Js {
            s: s.as_bytes(),
            i: 0,
        }
    }
    fn ws(&mut self) {
        while self.i < self.s.len() {
            match self.s[self.i] {
                b' ' | b'\t' | b'\n' | b'\r' => self.i += 1,
                b'/' if self.s.get(self.i + 1) == Some(&b'/') => {
                    while self.i < self.s.len() && self.s[self.i] != b'\n' {
                        self.i += 1;
                    }
                }
                _ => break,
            }
        }
    }
    fn peek(&mut self) -> Option<u8> {
        self.ws();
        self.s.get(self.i).copied()
    }
    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, c: u8) -> DbResult<()> {
        if self.eat(c) {
            Ok(())
        } else {
            Err(format!("expected '{}' at {}", c as char, self.i))
        }
    }
    fn rest(&self) -> &'a str {
        std::str::from_utf8(&self.s[self.i..]).unwrap_or("")
    }
    fn word(&mut self) -> String {
        self.ws();
        let start = self.i;
        while self.i < self.s.len()
            && (self.s[self.i].is_ascii_alphanumeric() || matches!(self.s[self.i], b'_' | b'$'))
        {
            self.i += 1;
        }
        String::from_utf8_lossy(&self.s[start..self.i]).into_owned()
    }
    fn string(&mut self) -> DbResult<String> {
        let q = self.s[self.i];
        self.i += 1;
        let mut out = Vec::new();
        while self.i < self.s.len() {
            let c = self.s[self.i];
            self.i += 1;
            if c == q {
                return String::from_utf8(out).map_err(err);
            }
            if c == b'\\' && self.i < self.s.len() {
                let n = self.s[self.i];
                self.i += 1;
                out.push(match n {
                    b'n' => b'\n',
                    b't' => b'\t',
                    b'r' => b'\r',
                    o => o,
                });
            } else {
                out.push(c);
            }
        }
        Err("unterminated string".into())
    }
    fn value(&mut self) -> DbResult<Bson> {
        match self.peek().ok_or("unexpected end")? {
            b'{' => Ok(Bson::Document(self.document()?)),
            b'[' => {
                self.i += 1;
                let mut a = Vec::new();
                while !self.eat(b']') {
                    a.push(self.value()?);
                    if !self.eat(b',') {
                        self.expect(b']')?;
                        break;
                    }
                }
                Ok(Bson::Array(a))
            }
            b'"' | b'\'' => Ok(Bson::String(self.string()?)),
            b'/' => {
                self.i += 1;
                let start = self.i;
                while self.i < self.s.len() && self.s[self.i] != b'/' {
                    if self.s[self.i] == b'\\' {
                        self.i += 1;
                    }
                    self.i += 1;
                }
                let pattern =
                    String::from_utf8_lossy(&self.s[start..self.i.min(self.s.len())]).into_owned();
                self.i += 1;
                let flags = self.word();
                Ok(Bson::RegularExpression(bson::Regex {
                    pattern,
                    options: flags,
                }))
            }
            c if c == b'-' || c.is_ascii_digit() => {
                let start = self.i;
                self.i += 1;
                while self.i < self.s.len()
                    && matches!(
                        self.s[self.i],
                        b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-'
                    )
                {
                    self.i += 1;
                }
                let t = std::str::from_utf8(&self.s[start..self.i]).unwrap_or("0");
                Ok(match t.parse::<i64>() {
                    Ok(n) if i32::try_from(n).is_ok() => Bson::Int32(n as i32),
                    Ok(n) => Bson::Int64(n),
                    Err(_) => Bson::Double(t.parse().map_err(|_| format!("bad number {t}"))?),
                })
            }
            _ => {
                let w = self.word();
                match w.as_str() {
                    "true" => Ok(Bson::Boolean(true)),
                    "false" => Ok(Bson::Boolean(false)),
                    "null" => Ok(Bson::Null),
                    "undefined" => Ok(Bson::Undefined),
                    "new" => self.value(),
                    "" => Err(format!(
                        "unexpected '{}'",
                        self.rest().chars().next().unwrap_or(' ')
                    )),
                    f => {
                        self.expect(b'(')?;
                        let arg = if self.peek() == Some(b')') {
                            None
                        } else {
                            Some(self.value()?)
                        };
                        self.expect(b')')?;
                        let text = match &arg {
                            Some(Bson::String(s)) => s.clone(),
                            Some(b) => b.to_string(),
                            None => String::new(),
                        };
                        match f {
                            "ObjectId" => match arg {
                                None => Ok(Bson::ObjectId(ObjectId::new())),
                                _ => ObjectId::parse_str(&text).map(Bson::ObjectId).map_err(err),
                            },
                            "ISODate" | "Date" => match arg {
                                None => Ok(Bson::DateTime(bson::DateTime::now())),
                                _ => parse_date(&text)
                                    .map(Bson::DateTime)
                                    .ok_or_else(|| format!("bad date {text}")),
                            },
                            "NumberLong" | "Long" => {
                                text.parse::<i64>().map(Bson::Int64).map_err(err)
                            }
                            "NumberInt" | "Int32" => {
                                text.parse::<i32>().map(Bson::Int32).map_err(err)
                            }
                            "NumberDecimal" | "Decimal128" => text
                                .parse::<bson::Decimal128>()
                                .map(Bson::Decimal128)
                                .map_err(err),
                            other => Err(format!("unknown function {other}()")),
                        }
                    }
                }
            }
        }
    }
    fn document(&mut self) -> DbResult<Document> {
        self.expect(b'{')?;
        let mut d = Document::new();
        while !self.eat(b'}') {
            let key = match self.peek() {
                Some(b'"' | b'\'') => self.string()?,
                _ => self.word(),
            };
            if key.is_empty() {
                return Err(format!("expected a key at {}", self.i));
            }
            self.expect(b':')?;
            d.insert(key, self.value()?);
            if !self.eat(b',') {
                self.expect(b'}')?;
                break;
            }
        }
        Ok(d)
    }
    /// `(a, b, …)` call arguments.
    fn args(&mut self) -> DbResult<Vec<Bson>> {
        self.expect(b'(')?;
        let mut out = Vec::new();
        while !self.eat(b')') {
            out.push(self.value()?);
            if !self.eat(b',') {
                self.expect(b')')?;
                break;
            }
        }
        Ok(out)
    }
}

/// A shell call: `db[.getCollection("c") | .c].method(args)[.chain(args)…]`.
#[derive(Debug, PartialEq)]
pub struct Call {
    pub db: Option<String>,
    pub collection: Option<String>,
    pub method: String,
    pub args: Vec<Bson>,
    pub chain: Vec<(String, Vec<Bson>)>,
}

pub fn parse_call(src: &str) -> DbResult<Call> {
    let src = src.trim().trim_end_matches(';');
    let mut p = Js::new(src);
    if p.word() != "db" {
        return Err("Start with db. — e.g. db.users.find({})".into());
    }
    let mut db_name = None;
    let mut collection = None;
    let mut segments: Vec<(String, Option<Vec<Bson>>)> = Vec::new();
    loop {
        let name = if p.eat(b'.') {
            p.word()
        } else if p.eat(b'[') {
            let s = match p.peek() {
                Some(b'"' | b'\'') => p.string()?,
                _ => return Err("expected a quoted name in [ ]".into()),
            };
            p.expect(b']')?;
            s
        } else {
            break;
        };
        let args = if p.peek() == Some(b'(') {
            Some(p.args()?)
        } else {
            None
        };
        segments.push((name, args));
    }
    if p.peek().is_some() {
        return Err(format!("unexpected text: {}", p.rest()));
    }
    let mut it = segments.into_iter().peekable();
    // db.getSiblingDB("x"), db.getCollection("c"), db.c
    while let Some((name, args)) = it.peek().cloned() {
        match (name.as_str(), &args) {
            ("getSiblingDB" | "getSisterDB", Some(a)) => {
                db_name = a.first().and_then(|b| b.as_str()).map(str::to_string);
                it.next();
            }
            ("getCollection", Some(a)) if collection.is_none() => {
                collection = a.first().and_then(|b| b.as_str()).map(str::to_string);
                it.next();
            }
            (_, None) if collection.is_none() => {
                collection = Some(name);
                it.next();
            }
            _ => break,
        }
    }
    let (method, args) = it.next().ok_or("expected a method call, e.g. .find({})")?;
    let args = args.ok_or("expected ( ) after the method")?;
    let chain = it.map(|(n, a)| (n, a.unwrap_or_default())).collect();
    Ok(Call {
        db: db_name,
        collection,
        method,
        args,
        chain,
    })
}

fn doc_arg(args: &[Bson], i: usize) -> DbResult<Document> {
    match args.get(i) {
        None | Some(Bson::Null) => Ok(Document::new()),
        Some(Bson::Document(d)) => Ok(d.clone()),
        Some(o) => Err(format!("expected an object, got {o}")),
    }
}

/// Terms from the filter bar as a find() filter.
fn filter_doc(filter: &Option<WhereClause>, shape: &[(String, String)]) -> DbResult<Document> {
    let Some(w) = filter else {
        return Ok(Document::new());
    };
    if w.terms.is_empty() && !w.sql.trim().is_empty() {
        // A raw filter: a query document.
        let raw = w.sql.trim().trim_start_matches('(').trim_end_matches(')');
        return Js::new(raw).document();
    }
    let ty = |c: &str| {
        shape
            .iter()
            .find(|(n, _)| n == c)
            .map(|(_, t)| t.as_str())
            .unwrap_or("string")
    };
    let mut all = Vec::new();
    for FilterTerm { column, op, value } in &w.terms {
        let t = ty(column);
        let v = || typed(Some(value), t);
        let re = |p: String| {
            Bson::RegularExpression(bson::Regex {
                pattern: p,
                options: "i".into(),
            })
        };
        let esc = regex_escape(value);
        let cond: Bson = match op {
            FilterOp::Eq => v(),
            FilterOp::Ne => doc! {"$ne": v()}.into(),
            FilterOp::Lt => doc! {"$lt": v()}.into(),
            FilterOp::Gt => doc! {"$gt": v()}.into(),
            FilterOp::Le => doc! {"$lte": v()}.into(),
            FilterOp::Ge => doc! {"$gte": v()}.into(),
            FilterOp::In | FilterOp::NotIn => {
                let items: Vec<Bson> = crate::filter::split_list(value)
                    .iter()
                    .map(|x| typed(Some(x), t))
                    .collect();
                if *op == FilterOp::In {
                    doc! {"$in": items}
                } else {
                    doc! {"$nin": items}
                }
                .into()
            }
            FilterOp::IsNull => Bson::Null,
            FilterOp::IsNotNull => doc! {"$ne": Bson::Null}.into(),
            FilterOp::Between => {
                let items = crate::filter::split_list(value);
                let [lo, hi] = items.as_slice() else {
                    continue;
                };
                doc! {"$gte": typed(Some(lo), t), "$lte": typed(Some(hi), t)}.into()
            }
            FilterOp::Like | FilterOp::ILike => {
                let p = format!(
                    "^{}$",
                    regex_escape(value).replace('%', ".*").replace('_', ".")
                );
                let opts = if *op == FilterOp::ILike { "i" } else { "" };
                Bson::RegularExpression(bson::Regex {
                    pattern: p,
                    options: opts.into(),
                })
            }
            FilterOp::Contains => re(esc),
            FilterOp::NotContains => doc! {"$not": re(esc)}.into(),
            FilterOp::HasPrefix => re(format!("^{esc}")),
            FilterOp::HasSuffix => re(format!("{esc}$")),
        };
        all.push(doc! {column.clone(): cond});
    }
    Ok(match all.len() {
        0 => Document::new(),
        1 => all.pop().unwrap(),
        _ => doc! {"$and": all},
    })
}

fn regex_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if "\\.+*?()|[]{}^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `ORDER BY "a" DESC, "b"` → `{a: -1, b: 1}`.
fn sort_doc(order: Option<&str>) -> Document {
    let mut d = Document::new();
    let Some(o) = order else {
        return d;
    };
    for part in o.trim().trim_start_matches("ORDER BY").split(',') {
        let part = part.trim();
        let desc = part.ends_with(" DESC");
        let name = part
            .trim_end_matches(" DESC")
            .trim_end_matches(" ASC")
            .trim();
        let name = name
            .strip_prefix('"')
            .and_then(|n| n.strip_suffix('"'))
            .unwrap_or(name)
            .replace("\"\"", "\"");
        if !name.is_empty() {
            d.insert(name, if desc { -1 } else { 1 });
        }
    }
    d
}

async fn collect(mut cursor: mongodb::Cursor<Document>, limit: usize) -> DbResult<Vec<Document>> {
    let mut out = Vec::new();
    while out.len() < limit && cursor.advance().await.map_err(err)? {
        out.push(cursor.deserialize_current().map_err(err)?);
    }
    Ok(out)
}

impl Mongo {
    fn current(&self) -> String {
        self.current.lock().unwrap().clone()
    }

    fn coll(&self, db: &str, name: &str) -> Collection<Document> {
        self.client.database(db).collection(name)
    }

    /// Sampled (name, type) columns, `_id` first; cached.
    async fn shape(&self, db_name: &str, coll: &str) -> DbResult<Vec<(String, String)>> {
        let cache_key = format!("{db_name}.{coll}");
        if let Some(s) = self.shapes.lock().unwrap().get(&cache_key) {
            return Ok(s.clone());
        }
        let c = self.coll(db_name, coll);
        let docs = collect(
            c.find(doc! {}).limit(SAMPLE).await.map_err(err)?,
            SAMPLE as usize,
        )
        .await?;
        let mut order: Vec<String> = vec!["_id".into()];
        let mut types: HashMap<String, HashMap<&'static str, usize>> = HashMap::new();
        for d in &docs {
            for (k, v) in d {
                if !order.contains(k) {
                    order.push(k.clone());
                }
                if !matches!(v, Bson::Null) {
                    *types
                        .entry(k.clone())
                        .or_default()
                        .entry(type_name(v))
                        .or_default() += 1;
                }
            }
        }
        let shape: Vec<(String, String)> = order
            .into_iter()
            .map(|k| {
                let t = types.get(&k).map(|m| {
                    if m.len() > 1 {
                        "mixed"
                    } else {
                        m.keys().next().copied().unwrap_or("string")
                    }
                });
                let t = t.unwrap_or(if k == "_id" { "objectId" } else { "string" });
                (k, t.to_string())
            })
            .collect();
        self.shapes.lock().unwrap().insert(cache_key, shape.clone());
        Ok(shape)
    }

    /// Run a shell call; rows as JSON objects.
    async fn call(&self, src: &str, limit: i64) -> DbResult<Vec<Value>> {
        let c = parse_call(src)?;
        let db_name = c.db.clone().unwrap_or_else(|| self.current());
        let database = self.client.database(&db_name);
        let limit = limit.max(0) as usize;
        let docs_rows = |docs: Vec<Document>| docs.iter().map(doc_json).collect::<Vec<_>>();
        let one = |v: Value| vec![v];
        let Some(coll_name) = c.collection.clone() else {
            return match c.method.as_str() {
                "runCommand" | "adminCommand" => {
                    let cmd = doc_arg(&c.args, 0)?;
                    let target = if c.method == "adminCommand" {
                        self.client.database("admin")
                    } else {
                        database
                    };
                    Ok(one(doc_json(&target.run_command(cmd).await.map_err(err)?)))
                }
                "getCollectionNames" => {
                    let names = database.list_collection_names().await.map_err(err)?;
                    Ok(names.into_iter().map(|n| json!({ "name": n })).collect())
                }
                "createCollection" => {
                    let name = c
                        .args
                        .first()
                        .and_then(|b| b.as_str())
                        .ok_or("createCollection(\"name\")")?;
                    database.create_collection(name).await.map_err(err)?;
                    Ok(one(json!({ "ok": 1 })))
                }
                "dropDatabase" => {
                    database.drop().await.map_err(err)?;
                    Ok(one(json!({ "ok": 1 })))
                }
                "stats" => Ok(one(doc_json(
                    &database
                        .run_command(doc! {"dbStats": 1})
                        .await
                        .map_err(err)?,
                ))),
                m => Err(format!("db.{m}() isn't supported")),
            };
        };
        let coll = self.coll(&db_name, &coll_name);
        self.shapes
            .lock()
            .unwrap()
            .remove(&format!("{db_name}.{coll_name}"));
        let chain = |name: &str| {
            c.chain
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, a)| a.clone())
        };
        let num = |a: Option<Vec<Bson>>| {
            a.and_then(|a| {
                a.first()
                    .and_then(|b| b.as_i64().or(b.as_i32().map(i64::from)))
            })
        };
        match c.method.as_str() {
            "find" | "findOne" => {
                let mut f = coll.find(doc_arg(&c.args, 0)?);
                if let Some(Bson::Document(p)) = c.args.get(1) {
                    f = f.projection(p.clone());
                }
                if let Some(s) = chain("sort") {
                    f = f.sort(doc_arg(&s, 0)?);
                }
                if let Some(n) = num(chain("skip")) {
                    f = f.skip(n as u64);
                }
                let mut cap = limit;
                if let Some(n) = num(chain("limit")).filter(|n| *n > 0) {
                    cap = cap.min(n as usize);
                }
                if c.method == "findOne" {
                    cap = 1;
                }
                f = f.limit(cap as i64);
                if chain("count").is_some() || chain("countDocuments").is_some() {
                    let n = coll
                        .count_documents(doc_arg(&c.args, 0)?)
                        .await
                        .map_err(err)?;
                    return Ok(one(json!({ "count": n })));
                }
                Ok(docs_rows(collect(f.await.map_err(err)?, cap).await?))
            }
            "aggregate" => {
                let pipeline: Vec<Document> = match c.args.first() {
                    Some(Bson::Array(a)) => {
                        a.iter().filter_map(|b| b.as_document().cloned()).collect()
                    }
                    None => Vec::new(),
                    _ => return Err("aggregate([...]) takes an array of stages".into()),
                };
                Ok(docs_rows(
                    collect(coll.aggregate(pipeline).await.map_err(err)?, limit).await?,
                ))
            }
            "countDocuments" | "count" => Ok(one(
                json!({ "count": coll.count_documents(doc_arg(&c.args, 0)?).await.map_err(err)? }),
            )),
            "estimatedDocumentCount" => Ok(one(
                json!({ "count": coll.estimated_document_count().await.map_err(err)? }),
            )),
            "distinct" => {
                let field = c
                    .args
                    .first()
                    .and_then(|b| b.as_str())
                    .ok_or("distinct(\"field\")")?;
                let vals = coll
                    .distinct(field, doc_arg(&c.args, 1)?)
                    .await
                    .map_err(err)?;
                Ok(vals
                    .iter()
                    .take(limit)
                    .map(|v| json!({ field: to_json(v) }))
                    .collect())
            }
            "insertOne" => {
                let r = coll.insert_one(doc_arg(&c.args, 0)?).await.map_err(err)?;
                Ok(one(json!({ "insertedId": to_json(&r.inserted_id) })))
            }
            "insertMany" => {
                let docs: Vec<Document> = match c.args.first() {
                    Some(Bson::Array(a)) => {
                        a.iter().filter_map(|b| b.as_document().cloned()).collect()
                    }
                    _ => return Err("insertMany([...]) takes an array".into()),
                };
                let r = coll.insert_many(docs).await.map_err(err)?;
                Ok(one(json!({ "insertedCount": r.inserted_ids.len() })))
            }
            "updateOne" | "updateMany" | "replaceOne" => {
                let (f, u) = (doc_arg(&c.args, 0)?, doc_arg(&c.args, 1)?);
                let r = match c.method.as_str() {
                    "updateOne" => coll.update_one(f, u).await,
                    "updateMany" => coll.update_many(f, u).await,
                    _ => coll.replace_one(f, u).await,
                }
                .map_err(err)?;
                Ok(one(
                    json!({ "matchedCount": r.matched_count, "modifiedCount": r.modified_count }),
                ))
            }
            "deleteOne" | "deleteMany" => {
                let f = doc_arg(&c.args, 0)?;
                let r = if c.method == "deleteOne" {
                    coll.delete_one(f).await
                } else {
                    coll.delete_many(f).await
                }
                .map_err(err)?;
                Ok(one(json!({ "deletedCount": r.deleted_count })))
            }
            "drop" => {
                coll.drop().await.map_err(err)?;
                Ok(one(json!({ "ok": 1 })))
            }
            "getIndexes" => {
                let r = database
                    .run_command(doc! {"listIndexes": &coll_name})
                    .await
                    .map_err(err)?;
                let batch = r
                    .get_document("cursor")
                    .ok()
                    .and_then(|c| c.get_array("firstBatch").ok())
                    .cloned()
                    .unwrap_or_default();
                Ok(batch.iter().map(to_json).collect())
            }
            "createIndex" => {
                let keys = doc_arg(&c.args, 0)?;
                let mut spec = doc_arg(&c.args, 1)?;
                if !spec.contains_key("name") {
                    let name: Vec<String> = keys.iter().map(|(k, v)| format!("{k}_{v}")).collect();
                    spec.insert("name", name.join("_"));
                }
                spec.insert("key", keys);
                let r = database
                    .run_command(doc! {"createIndexes": &coll_name, "indexes": [spec]})
                    .await
                    .map_err(err)?;
                Ok(one(doc_json(&r)))
            }
            "dropIndex" => {
                let name = c
                    .args
                    .first()
                    .and_then(|b| b.as_str())
                    .ok_or("dropIndex(\"name\")")?;
                let r = database
                    .run_command(doc! {"dropIndexes": &coll_name, "index": name})
                    .await
                    .map_err(err)?;
                Ok(one(doc_json(&r)))
            }
            m => Err(format!("{m}() isn't supported")),
        }
    }

    async fn grid_op(&self, op: GridOp) -> DbResult<u64> {
        let (schema, table) = match &op {
            GridOp::Insert { schema, table, .. }
            | GridOp::Update { schema, table, .. }
            | GridOp::Delete { schema, table, .. } => (
                schema.clone().unwrap_or_else(|| self.current()),
                table.clone(),
            ),
        };
        let shape = self.shape(&schema, &table).await?;
        let ty = |c: &str| {
            shape
                .iter()
                .find(|(n, _)| n == c)
                .map(|(_, t)| t.clone())
                .unwrap_or_else(|| "mixed".into())
        };
        let to_doc = |pairs: &[(String, Option<String>)]| -> Document {
            pairs
                .iter()
                .map(|(c, v)| (c.clone(), typed(v.as_deref(), &ty(c))))
                .collect()
        };
        let coll = self.coll(&schema, &table);
        match op {
            GridOp::Insert { values, .. } => {
                let mut d = to_doc(&values);
                if matches!(d.get("_id"), Some(Bson::Null) | Some(Bson::String(_)))
                    && values
                        .iter()
                        .any(|(c, v)| c == "_id" && v.as_deref().is_none_or(str::is_empty))
                {
                    d.remove("_id");
                }
                coll.insert_one(d).await.map_err(err)?;
                Ok(1)
            }
            GridOp::Update { set, key, .. } => {
                let r = coll
                    .update_one(to_doc(&key), doc! {"$set": to_doc(&set)})
                    .await
                    .map_err(err)?;
                if r.matched_count == 0 {
                    return Err("The document was not found (changed or deleted elsewhere?)".into());
                }
                Ok(r.modified_count)
            }
            GridOp::Delete { key, .. } => Ok(coll
                .delete_one(to_doc(&key))
                .await
                .map_err(err)?
                .deleted_count),
        }
    }
}

impl Driver for Mongo {
    fn engine(&self) -> Engine {
        Engine::MongoDb
    }
    fn default_schema(&self) -> Option<String> {
        Some(self.current())
    }
    fn row_key(&self) -> bool {
        false
    }
    fn version(&self) -> Fut<String> {
        let this = self.clone();
        Box::pin(async move {
            let d = db::run_db(async move {
                this.client
                    .database("admin")
                    .run_command(doc! {"buildInfo": 1})
                    .await
                    .map_err(err)
            })
            .await?;
            Ok(format!("MongoDB {}", d.get_str("version").unwrap_or("?")))
        })
    }
    fn databases(&self) -> Fut<Vec<String>> {
        self.schemas()
    }
    fn schemas(&self) -> Fut<Vec<String>> {
        let this = self.clone();
        Box::pin(async move {
            let cur = this.current();
            let mut names = db::run_logged(
                "listDatabases".into(),
                crate::console::Source::Meta,
                async move { this.client.list_database_names().await.map_err(err) },
            )
            .await?;
            if !names.contains(&cur) {
                names.push(cur);
                names.sort();
            }
            Ok(names)
        })
    }
    fn objects(&self, schema: String) -> Fut<ObjectTree> {
        let this = self.clone();
        Box::pin(async move {
            *this.current.lock().unwrap() = schema.clone();
            let label = format!("listCollections ({schema})");
            db::run_logged(label, crate::console::Source::Meta, async move {
                let r = this
                    .client
                    .database(&schema)
                    .run_command(doc! {"listCollections": 1, "nameOnly": true})
                    .await
                    .map_err(err)?;
                let batch = r
                    .get_document("cursor")
                    .ok()
                    .and_then(|c| c.get_array("firstBatch").ok())
                    .cloned()
                    .unwrap_or_default();
                let mut tree = ObjectTree::default();
                for b in batch {
                    let Some(d) = b.as_document() else { continue };
                    let name = d.get_str("name").unwrap_or_default().to_string();
                    if name.starts_with("system.") {
                        continue;
                    }
                    match d.get_str("type").unwrap_or("collection") {
                        "view" => tree.views.push(name),
                        _ => tree.tables.push(name),
                    }
                }
                tree.tables.sort();
                tree.views.sort();
                Ok(tree)
            })
            .await
        })
    }
    fn columns(&self, schema: String, table: String) -> Fut<Vec<GridColumnMeta>> {
        let this = self.clone();
        Box::pin(async move {
            this.shapes
                .lock()
                .unwrap()
                .remove(&format!("{schema}.{table}"));
            let label = format!("db.{table}.find().limit({SAMPLE})");
            let shape = db::run_logged(label, crate::console::Source::Meta, async move {
                this.shape(&schema, &table).await
            })
            .await?;
            Ok(shape
                .into_iter()
                .map(|(name, ty)| GridColumnMeta {
                    is_pk: name == "_id",
                    nullable: name != "_id",
                    pg_type: grid_type(&ty),
                    sql_type: ty,
                    default: None,
                    comment: None,
                    foreign_key: None,
                    enum_values: Vec::new(),
                    name,
                })
                .collect())
        })
    }
    fn count(&self, schema: String, table: String, filter: Option<WhereClause>) -> Fut<i64> {
        let this = self.clone();
        Box::pin(async move {
            let label = format!("db.{table}.countDocuments()");
            db::run_logged(label, crate::console::Source::Data, async move {
                let shape = this.shape(&schema, &table).await?;
                let f = filter_doc(&filter, &shape)?;
                let c = this.coll(&schema, &table);
                let n = if f.is_empty() {
                    c.estimated_document_count().await
                } else {
                    c.count_documents(f).await
                };
                Ok(n.map_err(err)? as i64)
            })
            .await
        })
    }
    fn window(&self, req: WindowReq) -> Fut<Vec<Value>> {
        let this = self.clone();
        Box::pin(async move {
            let label = format!(
                "db.{}.find().skip({}).limit({})",
                req.table, req.offset, req.limit
            );
            db::run_logged(label, crate::console::Source::Data, async move {
                let shape = this.shape(&req.schema, &req.table).await?;
                let f = filter_doc(&req.filter, &shape)?;
                let cap = req.limit.clamp(0, 1_000_000);
                let cursor = this
                    .coll(&req.schema, &req.table)
                    .find(f)
                    .sort(sort_doc(req.order_by.as_deref()))
                    .skip(req.offset.max(0) as u64)
                    .limit(cap)
                    .await
                    .map_err(err)?;
                let docs = collect(cursor, cap as usize).await?;
                // Every row in the column order (fields a document lacks are null).
                Ok(docs
                    .iter()
                    .map(|d| {
                        Value::Object(
                            shape
                                .iter()
                                .map(|(k, _)| {
                                    (k.clone(), d.get(k).map(to_json).unwrap_or(Value::Null))
                                })
                                .collect(),
                        )
                    })
                    .collect())
            })
            .await
        })
    }
    fn query_rows(&self, sql: String, limit: i64) -> Fut<Vec<Value>> {
        let this = self.clone();
        Box::pin(async move {
            let src = super::trim_sql(&sql);
            if let Some(name) = src.strip_prefix("use ").map(str::trim) {
                *this.current.lock().unwrap() = name.to_string();
                return Ok(vec![json!({ "result": format!("switched to db {name}") })]);
            }
            let label = src.clone();
            let rows = db::run_logged(label, crate::console::Source::Data, async move {
                this.call(&src, limit).await
            })
            .await?;
            Ok(super::uniform_rows(rows))
        })
    }
    fn query_columns(&self, sql: String) -> Fut<Vec<String>> {
        let this = self.clone();
        Box::pin(async move {
            let c = parse_call(&sql)?;
            if !matches!(c.method.as_str(), "find" | "findOne" | "aggregate") {
                return Ok(vec!["result".into()]);
            }
            let rows = db::run_db(async move { this.call(&sql, 1).await }).await?;
            Ok(rows
                .first()
                .and_then(|r| r.as_object())
                .map(|o| o.keys().cloned().collect())
                .unwrap_or_default())
        })
    }
    fn exec(&self, sql: String) -> Fut<u64> {
        let f = self.query_rows(sql, i64::MAX);
        Box::pin(async move { Ok(f.await?.len() as u64) })
    }
    fn batch(&self, stmts: Vec<Stmt>) -> Fut<u64> {
        let this = self.clone();
        Box::pin(db::run_db(async move {
            let mut n = 0;
            for st in stmts {
                let started = std::time::Instant::now();
                let r = match super::parse_grid_stmt(&st) {
                    Some(op) => this.grid_op(op).await,
                    None => this.call(&st.sql, i64::MAX).await.map(|r| r.len() as u64),
                };
                crate::console::record(
                    &st.sql,
                    started,
                    crate::console::Source::Data,
                    r.as_ref().err().map(String::as_str),
                );
                n += r?;
            }
            Ok(n)
        }))
    }
    fn script(&self, _kind: ObjKind, schema: String, name: String, which: Script) -> Fut<String> {
        let this = self.clone();
        Box::pin(async move {
            let target = if name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                format!("db.{name}")
            } else {
                format!("db.getCollection({})", json!(name))
            };
            let fields = || async {
                let shape = this.shape(&schema, &name).await.unwrap_or_default();
                shape
                    .into_iter()
                    .filter(|(k, _)| k != "_id")
                    .map(|(k, t)| format!("  {}: {}", json!(k), placeholder(&t)))
                    .collect::<Vec<_>>()
                    .join(",\n")
            };
            Ok(match which {
                Script::Select => format!("{target}.find({{}}).limit(100)"),
                Script::Insert => format!("{target}.insertOne({{\n{}\n}})", fields().await),
                Script::Update => format!(
                    "{target}.updateOne(\n  {{ _id: ObjectId(\"\") }},\n  {{ $set: {{ }} }}\n)"
                ),
                Script::Delete => format!("{target}.deleteOne({{ _id: ObjectId(\"\") }})"),
                Script::Drop => format!("{target}.drop()"),
                Script::Truncate => format!("{target}.deleteMany({{}})"),
                _ => format!("db.createCollection({})", json!(name)),
            })
        })
    }
    /// A `find` on one collection (no projection) edits like its grid.
    fn edit_source(
        &self,
        sql: String,
        _schema: String,
        result: Vec<String>,
    ) -> Fut<Result<crate::db::EditSource, String>> {
        let call = parse_call(&super::trim_sql(&sql));
        let (db_name, coll) = match &call {
            Ok(c)
                if matches!(c.method.as_str(), "find" | "findOne")
                    && c.args.len() <= 1
                    && c.collection.is_some() =>
            {
                (
                    c.db.clone().unwrap_or_else(|| self.current()),
                    c.collection.clone().unwrap_or_default(),
                )
            }
            Ok(c) if matches!(c.method.as_str(), "find" | "findOne") => {
                return Box::pin(async { Ok(Err("read-only: projected result".to_string())) });
            }
            _ => {
                return Box::pin(async {
                    Ok(Err(
                        "read-only: only find() results can be edited".to_string()
                    ))
                });
            }
        };
        let cols = self.columns(db_name.clone(), coll.clone());
        Box::pin(async move {
            let metas = cols.await?;
            let columns: Vec<Option<GridColumnMeta>> = result
                .iter()
                .map(|r| metas.iter().find(|m| &m.name == r).cloned())
                .collect();
            let Some(key) = result.iter().position(|r| r == "_id") else {
                return Ok(Err("read-only: select _id to edit".to_string()));
            };
            Ok(Ok(crate::db::EditSource {
                schema: db_name,
                table: coll,
                columns,
                key: vec![key],
                engine: Engine::MongoDb,
            }))
        })
    }
    fn sessions(&self) -> Fut<Vec<Value>> {
        let this = self.clone();
        Box::pin(async move {
            let r = db::run_logged(
                "currentOp".into(),
                crate::console::Source::Meta,
                async move {
                    this.client
                        .database("admin")
                        .run_command(doc! {"currentOp": 1})
                        .await
                        .map_err(err)
                },
            )
            .await?;
            Ok(r.get_array("inprog")
                .map(|a| {
                    a.iter()
                        .filter_map(|b| b.as_document())
                        .map(op_row)
                        .collect()
                })
                .unwrap_or_default())
        })
    }
    fn signal_session(&self, id: String, _kill: bool) -> Fut<()> {
        let this = self.clone();
        Box::pin(async move {
            // An op id is a number (replica set) or `shard:number` (mongos).
            let op: Bson = match id.trim().parse::<i64>() {
                Ok(n) if i32::try_from(n).is_ok() => Bson::Int32(n as i32),
                Ok(n) => Bson::Int64(n),
                Err(_) => Bson::String(id.trim().to_string()),
            };
            db::run_db(async move {
                this.client
                    .database("admin")
                    .run_command(doc! {"killOp": 1, "op": op})
                    .await
                    .map_err(err)?;
                Ok(())
            })
            .await
        })
    }
    fn indexes(&self, schema: String, table: String) -> Fut<Vec<IndexDef>> {
        let this = self.clone();
        Box::pin(async move {
            let r = db::run_db(async move {
                this.client
                    .database(&schema)
                    .run_command(doc! {"listIndexes": &table})
                    .await
                    .map_err(err)
            })
            .await?;
            let batch = r
                .get_document("cursor")
                .ok()
                .and_then(|c| c.get_array("firstBatch").ok())
                .cloned()
                .unwrap_or_default();
            Ok(batch
                .iter()
                .filter_map(|b| b.as_document())
                .map(|d| {
                    let keys = d.get_document("key").cloned().unwrap_or_default();
                    let name = d.get_str("name").unwrap_or_default().to_string();
                    let algorithm = keys
                        .iter()
                        .find_map(|(_, v)| v.as_str().map(str::to_string))
                        .unwrap_or_else(|| "btree".into());
                    IndexDef {
                        primary: name == "_id_",
                        unique: d.get_bool("unique").unwrap_or(name == "_id_"),
                        columns: keys
                            .iter()
                            .map(|(k, v)| {
                                if v.as_i32() == Some(-1) || v.as_i64() == Some(-1) {
                                    format!("{k} DESC")
                                } else {
                                    k.clone()
                                }
                            })
                            .collect::<Vec<_>>()
                            .join(", "),
                        include: String::new(),
                        condition: d
                            .get_document("partialFilterExpression")
                            .ok()
                            .map(|p| doc_json(p).to_string()),
                        comment: None,
                        constraint: None,
                        algorithm,
                        name,
                    }
                })
                .collect())
        })
    }
}

/// One `currentOp` entry as a process-list row (op id first).
fn op_row(d: &Document) -> Value {
    let text = |k: &str| d.get(k).map(to_json).unwrap_or(Value::Null);
    json!({
        "id": text("opid"),
        "client": text("client"),
        "application": d.get_document("clientMetadata").ok().and_then(|m| m.get_document("application").ok()).and_then(|a| a.get_str("name").ok()).unwrap_or_default(),
        "active": text("active"),
        "op": text("op"),
        "namespace": text("ns"),
        "seconds": text("secs_running"),
        "description": text("desc"),
        "command": d.get_document("command").map(|c| doc_json(c).to_string()).unwrap_or_default(),
    })
}

fn placeholder(t: &str) -> &'static str {
    match t {
        "int" | "long" | "double" | "decimal" => "0",
        "bool" => "false",
        "date" => "new Date()",
        "object" => "{}",
        "array" => "[]",
        "objectId" => "ObjectId()",
        _ => "\"\"",
    }
}

#[cfg(test)]
mod tests {
    use mongodb::bson::{Bson, doc};

    use crate::drivers::live;
    use crate::engine::Engine;

    #[test]
    fn shell_calls_parse() {
        let c = super::parse_call(
            r#"db.users.find({age: {$gt: 30}, 'n': "a"}).sort({age: -1}).limit(5);"#,
        )
        .unwrap();
        assert_eq!(c.collection.as_deref(), Some("users"));
        assert_eq!(c.method, "find");
        assert_eq!(
            c.args,
            vec![Bson::Document(doc! {"age": {"$gt": 30}, "n": "a"})]
        );
        assert_eq!(
            c.chain[0],
            ("sort".into(), vec![Bson::Document(doc! {"age": -1})])
        );
        let c = super::parse_call(r#"db.getSiblingDB("x").getCollection("a b").deleteOne({_id: ObjectId("65a1b2c3d4e5f60718293a4b")})"#).unwrap();
        assert_eq!(
            (c.db.as_deref(), c.collection.as_deref()),
            (Some("x"), Some("a b"))
        );
        assert!(matches!(
            c.args[0].as_document().unwrap().get("_id"),
            Some(Bson::ObjectId(_))
        ));
        let c = super::parse_call("db.runCommand({ping: 1})").unwrap();
        assert_eq!((c.collection, c.method.as_str()), (None, "runCommand"));
        assert!(super::parse_call("SELECT 1").is_err());
    }

    #[test]
    fn live_mongo() {
        if !live::reachable(37017) {
            return;
        }
        let c = live::conn(Engine::MongoDb, 37017, "", "tusk_scratch");
        let rt = crate::db::runtime();
        let db = rt
            .block_on(crate::drivers::connect(
                &c,
                c.host.clone(),
                c.port,
                String::new(),
            ))
            .unwrap();
        let d = db.driver();
        rt.block_on(d.exec("db.people.drop()".into())).ok();
        rt.block_on(d.exec(
            r#"db.people.insertMany([{_id: 1, email: "a@x", age: 31, tags: ["x"]}, {_id: 2, email: "b@x", age: 25}, {_id: 3, email: "c@y", age: 40, at: ISODate("2024-01-02T03:04:05Z")}])"#.into(),
        ))
        .unwrap();
        assert!(rt.block_on(d.version()).unwrap().starts_with("MongoDB "));
        assert!(
            rt.block_on(d.schemas())
                .unwrap()
                .contains(&"tusk_scratch".to_string())
        );
        assert_eq!(
            rt.block_on(d.objects("tusk_scratch".into()))
                .unwrap()
                .tables,
            ["people"]
        );
        let cols = rt
            .block_on(d.columns("tusk_scratch".into(), "people".into()))
            .unwrap();
        let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["_id", "email", "age", "tags", "at"]);
        assert!(cols[0].is_pk);
        assert_eq!(
            (cols[2].pg_type.as_str(), cols[4].pg_type.as_str()),
            ("int4", "timestamptz")
        );
        let req = |filter, order: Option<&str>| crate::drivers::WindowReq {
            schema: "tusk_scratch".into(),
            table: "people".into(),
            filter,
            order_by: order.map(str::to_string),
            with_key: false,
            limit: 10,
            offset: 0,
        };
        let rows = rt
            .block_on(d.window(req(None, Some(r#"ORDER BY "age" DESC"#))))
            .unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["email"], "c@y");
        assert_eq!(rows[0]["at"], "2024-01-02T03:04:05Z");
        assert_eq!(rows[1]["tags"], serde_json::json!(["x"]));
        assert_eq!(rows[2]["tags"], serde_json::Value::Null);
        let term = |col: &str, op, v: &str| crate::db::FilterTerm {
            column: col.into(),
            op,
            value: v.into(),
        };
        let f = crate::db::WhereClause {
            terms: vec![
                term("age", crate::filter::FilterOp::Gt, "30"),
                term("email", crate::filter::FilterOp::HasSuffix, "@x"),
            ],
            ..Default::default()
        };
        assert_eq!(
            rt.block_on(d.count("tusk_scratch".into(), "people".into(), Some(f)))
                .unwrap(),
            1
        );
        let st = |sql: &str, p: &[Option<&str>]| crate::db::Stmt {
            sql: sql.into(),
            params: p.iter().map(|v| v.map(str::to_string)).collect(),
        };
        rt.block_on(d.batch(vec![
            st(
                r#"UPDATE "tusk_scratch"."people" SET "age" = ?, "tags" = ? WHERE "_id" = ?"#,
                &[Some("26"), Some(r#"["y","z"]"#), Some("2")],
            ),
            st(
                r#"DELETE FROM "tusk_scratch"."people" WHERE "_id" = ?"#,
                &[Some("3")],
            ),
            st(
                r#"INSERT INTO "tusk_scratch"."people" ("_id", "email", "age") VALUES (?, ?, ?)"#,
                &[Some("4"), Some("d@x"), Some("50")],
            ),
        ]))
        .unwrap();
        let r = rt
            .block_on(d.query_rows("db.people.find({_id: 2})".into(), 10))
            .unwrap();
        assert_eq!(
            (r[0]["age"].clone(), r[0]["tags"].clone()),
            (serde_json::json!(26), serde_json::json!(["y", "z"]))
        );
        let r = rt
            .block_on(d.query_rows(
                "db.people.aggregate([{$group: {_id: null, n: {$sum: 1}}}])".into(),
                10,
            ))
            .unwrap();
        assert_eq!(r[0]["n"], 3);
        let ix = rt
            .block_on(d.indexes("tusk_scratch".into(), "people".into()))
            .unwrap();
        assert!(ix[0].primary);
        rt.block_on(d.exec("db.dropDatabase()".into())).unwrap();
    }
}
