//! Memory and latency benchmark: the release binary with the `opencode` provider pointed at
//! a local mock that streams like a slow model. Ignored by `cargo test`; run it with
//! `cargo test --release --test memory_bench -- --ignored --nocapture` (Linux only, it reads
//! `/proc`). `.github/workflows/bench.yml` runs it on an arm64 runner and writes the tables
//! to the job summary.
#![cfg(target_os = "linux")]

use std::convert::Infallible;
use std::fmt::Write as _;
use std::io::Write as _;
use std::net::TcpListener as StdListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde_json::{json, Value};

const PROVIDERS: &str = include_str!("../data/providers.json");
const CHAT_PATH: &str = "/inference/v1/chat/completions";
const BIN: &str = env!("CARGO_BIN_EXE_zilong-router");

/// The mock answers with this many content chunks, `CHUNK_DELAY` apart.
const CHUNKS: usize = 8;
const CHUNK_DELAY: Duration = Duration::from_millis(25);
/// Size of the system prompt every request carries (coding agents send large contexts).
const PROMPT_BYTES: usize = 32 * 1024;
const WARMUP: usize = 30;
const REQUESTS: usize = 1000;
const CONCURRENCY: [usize; 3] = [10, 50, 100];

async fn models() -> Json<Value> {
    Json(json!({"object": "list", "data": [{"id": "exo-free", "object": "model"}]}))
}

async fn chat(_body: Bytes) -> Response {
    let chunk = |choices: Value| {
        json!({"id": "chatcmpl-mock", "object": "chat.completion.chunk", "created": 0, "model": "exo-free", "choices": choices})
    };
    let events = futures_util::stream::unfold(0usize, move |i| async move {
        let event = match i {
            0 => chunk(json!([{"index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": null}])),
            n if n <= CHUNKS => {
                tokio::time::sleep(CHUNK_DELAY).await;
                chunk(json!([{"index": 0, "delta": {"content": "pong "}, "finish_reason": null}]))
            }
            n if n == CHUNKS + 1 => chunk(json!([{"index": 0, "delta": {}, "finish_reason": "stop"}])),
            n if n == CHUNKS + 2 => {
                let mut c = chunk(json!([]));
                c["usage"] = json!({"prompt_tokens": 8000, "completion_tokens": CHUNKS, "total_tokens": 8000 + CHUNKS});
                c
            }
            n if n == CHUNKS + 3 => return Some((Ok::<_, Infallible>("data: [DONE]\n\n".to_string()), i + 1)),
            _ => return None,
        };
        Some((Ok(format!("data: {event}\n\n")), i + 1))
    });
    ([(header::CONTENT_TYPE, "text/event-stream")], Body::from_stream(events)).into_response()
}

async fn start_mock() -> String {
    let app = Router::new().route("/zen/v1/models", get(models)).route(CHAT_PATH, post(chat));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

/// A running router process; killed and its data directory removed on drop.
struct Instance {
    child: Child,
    base: String,
    dir: PathBuf,
}

impl Drop for Instance {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn start_router(opencode_base: &str, workers: &str) -> Instance {
    let dir = std::env::temp_dir().join(format!("zilong-router-bench-{}-{workers}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut file: Value = serde_json::from_str(PROVIDERS).unwrap();
    let opencode = file["providers"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|p| p["id"] == "opencode")
        .expect("opencode is in data/providers.json");
    opencode["transport"]["baseUrl"] = json!(opencode_base);
    std::fs::write(dir.join("providers.json"), file.to_string()).unwrap();

    let port = StdListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let mut cmd = Command::new(BIN);
    cmd.args(["--host", "127.0.0.1", "--port", &port.to_string(), "--data-dir"])
        .arg(&dir)
        .env("LOG_LEVEL", "warn")
        .env("ZILONG_ROUTER_WORKERS", workers)
        .stdout(Stdio::null());
    for var in [
        "ZILONG_ROUTER_CONFIG",
        "REQUIRE_API_KEY",
        "TURSO_DATABASE_URL",
        "TURSO_AUTH_TOKEN",
        "TURSO_ONLY",
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        cmd.env_remove(var);
    }
    let mut instance = Instance { child: cmd.spawn().unwrap(), base: format!("http://127.0.0.1:{port}"), dir };

    let http = reqwest::Client::new();
    for _ in 0..1500 {
        if let Some(status) = instance.child.try_wait().unwrap() {
            panic!("zilong-router exited early: {status}");
        }
        if http.get(format!("{}/api/health", instance.base)).send().await.is_ok_and(|r| r.status().is_success()) {
            return instance;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("zilong-router did not become ready on {}", instance.base);
}

/// Resident and peak resident set size (KiB) and thread count, from `/proc/<pid>/status`.
struct Mem {
    rss_kb: u64,
    hwm_kb: u64,
    threads: u64,
}

fn mem(pid: u32) -> Mem {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    let field = |key: &str| {
        status
            .lines()
            .find_map(|l| l.strip_prefix(key))
            .and_then(|v| v.split_whitespace().next())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };
    Mem { rss_kb: field("VmRSS:"), hwm_kb: field("VmHWM:"), threads: field("Threads:") }
}

/// Polls RSS every 10 ms on its own thread and keeps the highest value seen.
struct Sampler {
    stop: Arc<AtomicBool>,
    peak: Arc<AtomicU64>,
    thread: std::thread::JoinHandle<()>,
}

impl Sampler {
    fn start(pid: u32) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let peak = Arc::new(AtomicU64::new(0));
        let (s, p) = (stop.clone(), peak.clone());
        let thread = std::thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                p.fetch_max(mem(pid).rss_kb, Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        Sampler { stop, peak, thread }
    }

    fn finish(self) -> u64 {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.join().unwrap();
        self.peak.load(Ordering::Relaxed)
    }
}

/// Requests rotate between the three client formats coding tools use.
const KINDS: [&str; 3] = ["chat", "chat-stream", "messages-stream"];

async fn one(http: &reqwest::Client, base: &str, kind: &str, prompt: &str) -> Result<Duration, String> {
    let (path, body) = match kind {
        "chat" | "chat-stream" => (
            "/v1/chat/completions",
            json!({
                "model": "oc/exo-free",
                "stream": kind == "chat-stream",
                "messages": [{"role": "system", "content": prompt}, {"role": "user", "content": "ping"}],
            }),
        ),
        _ => (
            "/v1/messages",
            json!({
                "model": "oc/exo-free",
                "max_tokens": 256,
                "stream": true,
                "system": prompt,
                "messages": [{"role": "user", "content": "ping"}],
            }),
        ),
    };
    let started = Instant::now();
    let resp = http.post(format!("{base}{path}")).json(&body).send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() || !text.contains("pong") {
        return Err(format!("{kind} {status}: {}", text.chars().take(300).collect::<String>()));
    }
    Ok(started.elapsed())
}

struct Run {
    concurrency: usize,
    requests: usize,
    errors: Vec<String>,
    elapsed: Duration,
    latencies: Vec<Duration>,
    peak_kb: u64,
}

impl Run {
    fn percentile(&self, p: f64) -> Duration {
        if self.latencies.is_empty() {
            return Duration::ZERO;
        }
        self.latencies[((self.latencies.len() - 1) as f64 * p).round() as usize]
    }
}

async fn load(pid: u32, base: &str, prompt: &Arc<str>, concurrency: usize, requests: usize) -> Run {
    let http = reqwest::Client::new();
    let sampler = Sampler::start(pid);
    let started = Instant::now();
    let results: Vec<Result<Duration, String>> = futures_util::stream::iter(0..requests)
        .map(|i| {
            let (http, base, prompt) = (http.clone(), base.to_string(), prompt.clone());
            async move { one(&http, &base, KINDS[i % KINDS.len()], &prompt).await }
        })
        .buffer_unordered(concurrency)
        .collect()
        .await;
    let elapsed = started.elapsed();
    let peak_kb = sampler.finish();
    let (mut latencies, mut errors) = (Vec::new(), Vec::new());
    for r in results {
        match r {
            Ok(d) => latencies.push(d),
            Err(e) => errors.push(e),
        }
    }
    latencies.sort();
    Run { concurrency, requests, errors, elapsed, latencies, peak_kb }
}

fn mib(kb: u64) -> String {
    format!("{:.1} MiB", kb as f64 / 1024.0)
}

fn ms(d: Duration) -> String {
    format!("{} ms", d.as_millis())
}

/// Benchmarks one router process and returns its markdown report and error count.
async fn bench(mock: &str, workers: &str) -> (String, usize) {
    let prompt: Arc<str> = "You are a careful coding agent. ".repeat(PROMPT_BYTES / 32).into();
    let started = Instant::now();
    let router = start_router(mock, workers).await;
    let ready = started.elapsed();
    let pid = router.child.id();

    tokio::time::sleep(Duration::from_secs(2)).await;
    let idle = mem(pid);
    let mut runs = vec![load(pid, &router.base, &prompt, 1, WARMUP).await];
    tokio::time::sleep(Duration::from_secs(2)).await;
    let warm = mem(pid);
    for c in CONCURRENCY {
        runs.push(load(pid, &router.base, &prompt, c, REQUESTS).await);
    }
    tokio::time::sleep(Duration::from_secs(5)).await;
    let after = mem(pid);

    let mut md = String::new();
    let _ = writeln!(md, "#### {workers} worker thread(s)\n");
    let _ = writeln!(md, "Ready {} after spawn.\n", ms(ready));
    let _ = writeln!(md, "| State | RSS | Threads |\n|---|---:|---:|");
    let _ = writeln!(md, "| Idle, 2 s after start | {} | {} |", mib(idle.rss_kb), idle.threads);
    let _ = writeln!(md, "| Idle after {WARMUP} sequential requests | {} | {} |", mib(warm.rss_kb), warm.threads);
    let _ = writeln!(md, "| Idle, 5 s after the load runs | {} | {} |", mib(after.rss_kb), after.threads);
    let _ = writeln!(md, "| Peak for the whole run (VmHWM) | {} | |\n", mib(after.hwm_kb));
    let _ = writeln!(md, "| Concurrency | Requests | Errors | Req/s | p50 | p99 | Peak RSS |");
    let _ = writeln!(md, "|---:|---:|---:|---:|---:|---:|---:|");
    for r in &runs {
        let _ = writeln!(
            md,
            "| {} | {} | {} | {:.0} | {} | {} | {} |",
            r.concurrency,
            r.requests,
            r.errors.len(),
            r.latencies.len() as f64 / r.elapsed.as_secs_f64(),
            ms(r.percentile(0.5)),
            ms(r.percentile(0.99)),
            mib(r.peak_kb),
        );
    }
    let errors: Vec<&String> = runs.iter().flat_map(|r| &r.errors).collect();
    if let Some(first) = errors.first() {
        let _ = writeln!(md, "\nFirst error: `{first}`");
    }
    md.push('\n');
    (md, errors.len())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "benchmark; run with --release -- --ignored"]
async fn memory_and_latency() {
    let mock = start_mock().await;
    let workers = std::env::var("BENCH_WORKERS").unwrap_or_else(|_| "1,2".into());
    let size = std::fs::metadata(BIN).map(|m| m.len()).unwrap_or(0);

    let mut md = String::new();
    let _ = writeln!(md, "### zilong-router memory benchmark ({})\n", std::env::consts::ARCH);
    let _ = writeln!(
        md,
        "Binary {:.1} MiB. The mock upstream streams {CHUNKS} chunks {} ms apart (about {} ms per \
         request); every request carries a {} KiB system prompt; requests rotate between OpenAI \
         JSON, OpenAI streaming and Anthropic Messages streaming, and each is logged to SQLite.\n",
        size as f64 / (1024.0 * 1024.0),
        CHUNK_DELAY.as_millis(),
        CHUNK_DELAY.as_millis() * CHUNKS as u128,
        PROMPT_BYTES / 1024,
    );
    let mut errors = 0;
    for w in workers.split(',').map(str::trim).filter(|w| !w.is_empty()) {
        let (section, e) = bench(&mock, w).await;
        md.push_str(&section);
        errors += e;
    }

    println!("{md}");
    if let Ok(path) = std::env::var("GITHUB_STEP_SUMMARY") {
        let mut f = std::fs::OpenOptions::new().append(true).create(true).open(path).unwrap();
        f.write_all(md.as_bytes()).unwrap();
    }
    assert_eq!(errors, 0, "some benchmark requests failed; see the report above");
}
