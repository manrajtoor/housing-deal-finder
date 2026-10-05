//! Cloudflare D1 implementation of [`Store`]. The only module that knows
//! about D1; everything it runs was planned elsewhere as `Stmt`s.

use serde_json::Value;
use worker::wasm_bindgen::JsValue;
use worker::{D1Database, D1PreparedStatement};

use crate::service::Store;
use crate::sql::{Param, Stmt, Written};

pub struct D1Store {
    db: D1Database,
}

impl D1Store {
    pub fn new(db: D1Database) -> D1Store {
        D1Store { db }
    }

    fn prepare(&self, s: &Stmt) -> Result<D1PreparedStatement, String> {
        let args: Vec<JsValue> = s
            .params
            .iter()
            .map(|p| match p {
                Param::Null => JsValue::null(),
                Param::Int(i) => JsValue::from_f64(*i as f64),
                Param::Real(f) => JsValue::from_f64(*f),
                Param::Text(t) => JsValue::from_str(t),
            })
            .collect();
        self.db.prepare(s.sql.as_str()).bind(&args).map_err(err)
    }
}

fn err(e: worker::Error) -> String {
    e.to_string()
}

impl Store for D1Store {
    async fn query(&self, s: &Stmt) -> Result<Vec<Value>, String> {
        self.prepare(s)?.all().await.map_err(err)?.results::<Value>().map_err(err)
    }

    async fn body(&self, s: &Stmt) -> Result<Option<String>, String> {
        self.prepare(s)?.first::<String>(Some("body")).await.map_err(err)
    }

    async fn batch(&self, stmts: Vec<Stmt>) -> Result<Vec<Written>, String> {
        if stmts.is_empty() {
            return Ok(Vec::new());
        }
        let prepared = stmts.iter().map(|s| self.prepare(s)).collect::<Result<Vec<_>, _>>()?;
        // A D1 batch runs as one transaction: all of it lands or none does.
        let mut out = Vec::with_capacity(prepared.len());
        for r in self.db.batch(prepared).await.map_err(err)? {
            if let Some(e) = r.error() {
                return Err(e);
            }
            let meta = r.meta().ok().flatten();
            out.push(Written {
                changes: meta.as_ref().and_then(|m| m.changes).unwrap_or(0) as u64,
                rows_written: meta.as_ref().and_then(|m| m.rows_written).unwrap_or(0) as u64,
            });
        }
        Ok(out)
    }
}
