//! axum server for a Web Function, started by `process.execve` from Node.
//!
//! Times are ms since the original `node` process started: `execve` keeps the PID, and the Node
//! side passes `performance.timeOrigin` (Unix ms) in NODE_TIME_ORIGIN and the moment it called
//! `execve` in EXECVE_AT_MS. With CALL_LOAD=1 (the preload variant, which runs before AWS's
//! wrapper) the server first fetches the function's configuration from the runtime API (/load),
//! as the wrapper would have, and takes any environment variables still missing from it. It then
//! listens on AWS_LAMBDA_HTTP_ENDPOINT and reports readiness itself (POST /ready), since the
//! wrapper that would do that is gone. Response headers use the names the in-region client reads.

use aws_sdk_dynamodb::Client;
use aws_sdk_dynamodb::types::AttributeValue;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn now_ms(origin: f64) -> f64 {
    let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs_f64() * 1000.0;
    ((t - origin) * 10.0).round() / 10.0
}

/// User + system CPU of this process (including the Node part before execve), in ms.
fn cpu_ms() -> u64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let after = stat.rsplit_once(')').map(|(_, r)| r).unwrap_or("");
    let f: Vec<&str> = after.split_whitespace().collect();
    // Fields after the command name start at field 3 (state); utime and stime are fields 14 and 15.
    let ticks: u64 = f.get(11).and_then(|v| v.parse().ok()).unwrap_or(0) + f.get(12).and_then(|v| v.parse().ok()).unwrap_or(0);
    ticks * 10
}

fn host_cpu() -> String {
    let info = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let part = info.lines().find(|l| l.starts_with("CPU part")).and_then(|l| l.split(':').nth(1)).map(|s| s.trim().to_string());
    match part.as_deref() {
        Some("0xd0c") => "Graviton2".into(),
        Some("0xd40") => "Graviton3".into(),
        Some("0xd4f") => "Graviton4".into(),
        Some(p) => p.into(),
        None => "unknown".into(),
    }
}

/// POST {} to the runtime API; returns the response body.
fn runtime_api_post(path: &str) -> Result<String, String> {
    let api = std::env::var("AWS_LAMBDA_RUNTIME_API").map_err(|_| "AWS_LAMBDA_RUNTIME_API not set")?;
    let mut s = TcpStream::connect(&api).map_err(|e| format!("connect {api}: {e}"))?;
    write!(s, "POST {path} HTTP/1.1\r\nHost: {api}\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}")
        .map_err(|e| e.to_string())?;
    let mut resp = String::new();
    s.read_to_string(&mut resp).map_err(|e| e.to_string())?;
    let status = resp.split_whitespace().nth(1).unwrap_or("");
    if !status.starts_with('2') {
        return Err(format!("{path}: {}", resp.lines().next().unwrap_or("")));
    }
    Ok(resp.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default())
}

struct Boot {
    origin: f64,
    variant: String,
    boot_id: String,
    cpu: String,
    execve_at: f64,
    main_ms: f64,
    load_ms: Option<f64>,
    table_from: &'static str,
    client_ms: f64,
    listen_ms: f64,
    ready_ms: f64,
}

struct AppState {
    boot: OnceLock<Boot>,
    client: Client,
    table: String,
    key: String,
    req_n: AtomicU64,
    first_req_ms: OnceLock<f64>,
    first_get_done_ms: OnceLock<f64>,
}

#[derive(Deserialize)]
struct Q {
    sleep: Option<u64>,
}

async fn item(State(st): State<Arc<AppState>>, Query(q): Query<Q>) -> impl IntoResponse {
    let b = st.boot.get().expect("boot set before serving");
    let t_in = now_ms(b.origin);
    let first_req = *st.first_req_ms.get_or_init(|| t_in);
    let n = st.req_n.fetch_add(1, Ordering::Relaxed) + 1;
    let out = st.client.get_item().table_name(&st.table).key("pk", AttributeValue::S(st.key.clone())).send().await;
    let t_done = now_ms(b.origin);
    let first_get_done = *st.first_get_done_ms.get_or_init(|| t_done);
    let payload = match &out {
        Ok(o) => o.item().and_then(|i| i.get("payload")).and_then(|v| v.as_s().ok()).cloned(),
        Err(e) => Some(format!("error: {e:?}")),
    };
    if let Some(ms) = q.sleep {
        tokio::time::sleep(Duration::from_millis(ms.min(5000))).await;
    }
    let mut h = HeaderMap::new();
    let mut put = |k: &'static str, v: String| {
        h.insert(k, v.parse().unwrap());
    };
    put("x-boot-id", b.boot_id.clone());
    put("x-req-n", n.to_string());
    put("x-uptime-ms", now_ms(b.origin).to_string());
    put("x-module-start-ms", b.main_ms.to_string());
    put("x-listen-ms", b.listen_ms.to_string());
    put("x-first-req-ms", first_req.to_string());
    put("x-cpu", b.cpu.clone());
    let body = json!({
        "scenario": "oneclient", "payload": payload, "ok": out.is_ok(),
        "variant": b.variant, "bootId": b.boot_id, "cpu": b.cpu, "reqN": n,
        "execveAtMs": b.execve_at, "rustMainMs": b.main_ms, "loadMs": b.load_ms, "tableFrom": b.table_from,
        "clientReadyMs": b.client_ms, "listenMs": b.listen_ms, "readyMs": b.ready_ms,
        "firstReqMs": first_req, "firstGetDoneMs": first_get_done, "thisGetMs": ((t_done - t_in) * 10.0).round() / 10.0,
        "cpuMsNow": cpu_ms(),
    });
    (h, Json::<Value>(body))
}

#[tokio::main]
async fn main() {
    let origin: f64 = std::env::var("NODE_TIME_ORIGIN").ok().and_then(|v| v.parse().ok()).unwrap_or(f64::NAN);
    let main_ms = now_ms(origin);
    let execve_at: f64 = std::env::var("EXECVE_AT_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(f64::NAN);
    let variant = std::env::var("VARIANT").unwrap_or_default();

    // Preload variant: the wrapper never ran, so fetch the configuration it would have fetched.
    let mut load_ms = None;
    let table_present_at_start = std::env::var("LAMBDABENCH_TABLE").is_ok();
    if std::env::var("CALL_LOAD").as_deref() == Ok("1") {
        let body = runtime_api_post("/2025-09-01/load").unwrap_or_else(|e| panic!("load failed: {e}"));
        load_ms = Some(now_ms(origin));
        let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        for kv in v["applicationConfiguration"].as_array().into_iter().flatten().filter_map(|x| x.as_str()) {
            if let Some((k, val)) = kv.split_once('=') {
                if std::env::var_os(k).is_none() {
                    // SAFETY: single-threaded at this point apart from tokio workers that read no env.
                    unsafe { std::env::set_var(k, val) };
                }
            }
        }
    }
    let table_from = if table_present_at_start { "env" } else if std::env::var("LAMBDABENCH_TABLE").is_ok() { "load" } else { "missing" };

    // Same client setup as scenarios/rust/oneclient: default config, retries disabled.
    let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .retry_config(aws_config::retry::RetryConfig::disabled())
        .load()
        .await;
    let client = Client::new(&config);
    let client_ms = now_ms(origin);
    let table = std::env::var("LAMBDABENCH_TABLE").expect("LAMBDABENCH_TABLE not set");
    let key = std::env::var("LAMBDABENCH_KEY").unwrap_or_else(|_| "seed".into());

    let state = Arc::new(AppState {
        boot: OnceLock::new(),
        client,
        table,
        key,
        req_n: AtomicU64::new(0),
        first_req_ms: OnceLock::new(),
        first_get_done_ms: OnceLock::new(),
    });
    let app = Router::new().route("/item", get(item)).route("/", get(item)).with_state(state.clone());

    let addr = std::env::var("AWS_LAMBDA_HTTP_ENDPOINT").unwrap_or_else(|_| "0.0.0.0:3000".into());
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind");
    let listen_ms = now_ms(origin);
    runtime_api_post("/2025-09-01/ready").unwrap_or_else(|e| panic!("ready failed: {e}"));
    let ready_ms = now_ms(origin);

    let boot_id = format!("{:08x}", (SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64 ^ std::process::id() as u64) & 0xffff_ffff);
    let _ = state.boot.set(Boot {
        origin, variant, boot_id, cpu: host_cpu(), execve_at, main_ms, load_ms, table_from, client_ms, listen_ms, ready_ms,
    });
    let b = state.boot.get().unwrap();
    println!(
        "{}",
        json!({"evt":"ready","variant":b.variant,"execveAtMs":b.execve_at,"rustMainMs":b.main_ms,"loadMs":b.load_ms,"tableFrom":b.table_from,"clientReadyMs":b.client_ms,"listenMs":b.listen_ms,"readyMs":b.ready_ms})
    );

    let shutdown = async {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
        term.recv().await;
        println!("{}", json!({"evt":"sigterm"}));
    };
    axum::serve(listener, app).with_graceful_shutdown(shutdown).await.expect("serve");
}
