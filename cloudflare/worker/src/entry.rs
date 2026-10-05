//! Worker entry points: the HTTP router and the cron handler. Composition
//! root: the only place `D1Store` and the notifier are chosen.
//!
//! Routes (CONTRACT.md):
//!   GET  /api/health                  open
//!   POST /api/listings                bearer INGEST_TOKEN
//!   POST /api/listings/needs-detail   bearer
//!   POST /api/listings/detail         bearer
//!   GET  /api/deals    ?market=&maxPrice=&minDiscount=&limit=
//!   GET  /api/alerts   ?market=&limit=
//!   GET  /api/stats
//! The GET read routes answer only on host `housedeals-api.internal` (the
//! Pages service binding) unless PUBLIC_READ_API is "true".

use serde_json::{json, Value};
use worker::*;

use crate::alerts::{rule_from_vars, AlertsQuery, NoNotifier};
use crate::auth::{bearer_ok, read_allowed};
use crate::d1::D1Store;
use crate::dispatch::{self, DispatchConfig};
use crate::scores::DealsQuery;
use crate::service::{self, ApiError, Config};

/// Request bodies above this are refused before parsing (200 listings with
/// 3 000-character descriptions fit comfortably).
const MAX_BODY_BYTES: usize = 2_000_000;
const DEFAULT_EXPIRE_AFTER_DAYS: f64 = 3.0;

fn now_iso() -> String {
    js_sys::Date::new_0().to_iso_string().as_string().unwrap_or_default()
}

fn iso_days_ago(days: f64) -> String {
    let ms = js_sys::Date::now() - days * 86_400_000.0;
    js_sys::Date::new(&JsValue::from_f64(ms)).to_iso_string().as_string().unwrap_or_default()
}

use worker::wasm_bindgen::JsValue;

fn var(env: &Env, k: &str) -> Option<String> {
    env.secret(k).ok().map(|v| v.to_string()).or_else(|| env.var(k).ok().map(|v| v.to_string()))
}

fn config(env: &Env) -> Config {
    let (rule, errors) = rule_from_vars(|k| var(env, k));
    for e in errors {
        console_error!("alert settings: {e} (default kept)");
    }
    Config { rule }
}

fn public_read_api(env: &Env) -> bool {
    var(env, "PUBLIC_READ_API").is_some_and(|v| v.trim().eq_ignore_ascii_case("true"))
}

fn json_response(status: u16, body: &Value) -> Result<Response> {
    let headers = Headers::new();
    headers.set("Content-Type", "application/json; charset=utf-8")?;
    headers.set("Cache-Control", "no-store")?;
    Ok(Response::from_json(body)?.with_status(status).with_headers(headers))
}

fn error_response(e: &ApiError) -> Result<Response> {
    if e.status >= 500 {
        console_error!("{}", e.message);
    }
    json_response(e.status, &json!({ "error": e.message }))
}

fn store(env: &Env) -> Result<D1Store> {
    Ok(D1Store::new(env.d1("DB")?))
}

fn query_pairs(url: &Url) -> Vec<(String, String)> {
    url.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect()
}

/// Bearer check, then the JSON body (size-capped).
async fn authed_body(req: &mut Request, env: &Env) -> std::result::Result<Value, Result<Response>> {
    let token = env.secret("INGEST_TOKEN").map(|s| s.to_string()).unwrap_or_default();
    if token.is_empty() {
        return Err(error_response(&ApiError { status: 503, message: "ingest is disabled: INGEST_TOKEN is not set".into() }));
    }
    let auth = req.headers().get("Authorization").ok().flatten();
    if !bearer_ok(auth.as_deref(), &token) {
        return Err(error_response(&ApiError { status: 401, message: "missing or wrong bearer token".into() }).map(|r| {
            let _ = r.headers().set("WWW-Authenticate", "Bearer");
            r
        }));
    }
    let len = req.headers().get("Content-Length").ok().flatten().and_then(|v| v.parse::<usize>().ok());
    if len.is_some_and(|n| n > MAX_BODY_BYTES) {
        return Err(error_response(&ApiError { status: 413, message: format!("body over {MAX_BODY_BYTES} bytes") }));
    }
    let text = match req.text().await {
        Ok(t) => t,
        Err(e) => return Err(error_response(&ApiError::bad_request(format!("unreadable body: {e}")))),
    };
    if text.len() > MAX_BODY_BYTES {
        return Err(error_response(&ApiError { status: 413, message: format!("body over {MAX_BODY_BYTES} bytes") }));
    }
    serde_json::from_str(&text).map_err(|e| error_response(&ApiError::bad_request(format!("body is not valid JSON: {e}"))))
}

fn reply<T: serde::Serialize>(r: std::result::Result<T, ApiError>) -> Result<Response> {
    match r {
        Ok(v) => json_response(200, &serde_json::to_value(v)?),
        Err(e) => error_response(&e),
    }
}

#[event(fetch)]
async fn fetch(mut req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let url = req.url()?;
    let path = url.path().trim_end_matches('/').to_string();
    let method = req.method();

    let is_read = matches!(path.as_str(), "/api/deals" | "/api/alerts" | "/api/stats");
    if is_read && !read_allowed(url.host_str(), public_read_api(&env)) {
        return error_response(&ApiError { status: 404, message: "not found".into() });
    }

    match (method, path.as_str()) {
        (Method::Get, "/api/health") => json_response(200, &json!({ "ok": true, "time": now_iso() })),

        (Method::Post, "/api/listings") => {
            let body = match authed_body(&mut req, &env).await {
                Ok(b) => b,
                Err(r) => return r,
            };
            let out = service::ingest(&store(&env)?, &NoNotifier, &config(&env), &body, &now_iso()).await;
            if let Ok(o) = &out {
                if let Some(e) = &o.error {
                    console_error!("ingest: {e}");
                }
                console_log!(
                    "ingest: seen {} added {} updated {} unchanged {} scored {} alerts {} rows written {}",
                    o.stats.seen, o.stats.added, o.stats.updated, o.stats.unchanged, o.scored, o.new_alerts, o.rows_written
                );
            }
            reply(out)
        }

        (Method::Post, "/api/listings/needs-detail") => {
            let body = match authed_body(&mut req, &env).await {
                Ok(b) => b,
                Err(r) => return r,
            };
            reply(service::needs_detail(&store(&env)?, &config(&env), &body, &now_iso()).await)
        }

        (Method::Post, "/api/listings/detail") => {
            let body = match authed_body(&mut req, &env).await {
                Ok(b) => b,
                Err(r) => return r,
            };
            let out = service::detail(&store(&env)?, &NoNotifier, &config(&env), &body, &now_iso()).await;
            if let Ok(Some(e)) = out.as_ref().map(|o| &o.error) {
                console_error!("detail: {e}");
            }
            reply(out)
        }

        (Method::Get, "/api/deals") => {
            let pairs = query_pairs(&url);
            match DealsQuery::from_pairs(pairs.iter().map(|(k, v)| (k.as_str(), v.as_str()))) {
                Ok(q) => reply(service::deals(&store(&env)?, &q, &now_iso()).await),
                Err(m) => error_response(&ApiError::bad_request(m)),
            }
        }

        (Method::Get, "/api/alerts") => {
            let pairs = query_pairs(&url);
            match AlertsQuery::from_pairs(pairs.iter().map(|(k, v)| (k.as_str(), v.as_str()))) {
                Ok(q) => reply(service::recent_alerts(&store(&env)?, &q, &now_iso()).await),
                Err(m) => error_response(&ApiError::bad_request(m)),
            }
        }

        (Method::Get, "/api/stats") => reply(service::stats(&store(&env)?, &now_iso()).await),

        (_, "/api/health" | "/api/listings" | "/api/listings/needs-detail" | "/api/listings/detail" | "/api/deals"
        | "/api/alerts" | "/api/stats") => error_response(&ApiError { status: 405, message: "method not allowed".into() }),
        _ => error_response(&ApiError { status: 404, message: "not found".into() }),
    }
}

/// Asks GitHub to start `crawl.yml` with `mode`. Logs and returns on any failure.
async fn dispatch_crawl(env: &Env, mode: &str) {
    let Some(cfg) = DispatchConfig::from_vars(|k| var(env, k), mode) else {
        console_log!("crawl dispatch ({mode}): off (GITHUB_DISPATCH_TOKEN, GITHUB_REPO or GITHUB_WORKFLOW not set)");
        return;
    };
    let result = async {
        let headers = Headers::new();
        for (k, v) in cfg.headers() {
            headers.set(k, &v)?;
        }
        let mut init = RequestInit::new();
        init.with_method(Method::Post).with_headers(headers).with_body(Some(JsValue::from_str(&cfg.body().to_string())));
        let mut res = Fetch::Request(Request::new_with_init(&cfg.url(), &init)?).send().await?;
        let status = res.status_code();
        let text = res.text().await.unwrap_or_default();
        Ok::<_, worker::Error>((status, text))
    }
    .await;
    match result {
        Ok((status, text)) => match dispatch::started(status, &text) {
            Ok(()) => console_log!("crawl dispatch: started {} mode {} on {}", cfg.workflow, cfg.mode, cfg.repo),
            Err(e) => console_error!("crawl dispatch ({mode}): {e}"),
        },
        Err(e) => console_error!("crawl dispatch ({mode}): {e}"),
    }
}

async fn expire_listings(env: &Env) {
    let days = var(env, "EXPIRE_AFTER_DAYS")
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|d| d.is_finite() && *d >= 1.0)
        .unwrap_or(DEFAULT_EXPIRE_AFTER_DAYS);
    let db = match store(env) {
        Ok(db) => db,
        Err(e) => return console_error!("expiry: no D1 binding: {e}"),
    };
    match service::expire(&db, &now_iso(), &iso_days_ago(days)).await {
        Ok(n) => console_log!("expiry: {n} listing(s) unseen for {days} days marked removed"),
        Err(e) => console_error!("expiry failed: {e}"),
    }
}

#[event(scheduled)]
async fn scheduled(event: ScheduledEvent, env: Env, _ctx: ScheduleContext) {
    let cron = event.cron();
    if dispatch::expires(&cron) {
        expire_listings(&env).await;
    }
    match dispatch::mode_for(&cron) {
        Some(mode) => dispatch_crawl(&env, mode).await,
        None => console_error!("scheduled: unknown cron {cron:?}"),
    }
}
