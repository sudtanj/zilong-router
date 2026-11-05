//! DeepSeek Web (`deepseek-web`, alias `ds-web`): the free chat.deepseek.com account used
//! the way its web client does. The user pastes the `userToken` from the site's
//! localStorage; each request exchanges it for an access token (`/users/current`), opens
//! or continues a chat session, solves the DeepSeekHashV1 proof of work (`deepseek_pow`)
//! and posts the prompt to `/api/v0/chat/completion`. The reply is turned into chat
//! completion chunks on the way (`translator::deepseek_web`), so the handlers treat the
//! provider as plain OpenAI chat.
//!
//! Chats are kept: a request that continues a conversation whose last reply this router
//! produced goes into the same chat with `parent_message_id` set, and sends only the new
//! turns.
//!
//! Approximated: the web client's signed `x-hif-leim` attestation header is not sent (9router
//! omits it too), the timezone offset is always 0 and `Accept-Encoding` is whatever
//! reqwest negotiates (no brotli/zstd).

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use futures_util::StreamExt;
use parking_lot::Mutex;
use reqwest::{Method, RequestBuilder};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use super::deepseek_pow::{self, Challenge};
use super::util;
use crate::db::ProviderConnection;
use crate::executor::ExecError;
use crate::providers::{Format, Provider};
use crate::sse::{self, Event};
use crate::state::AppState;
use crate::translator::deepseek_web::{FromDeepseek, Request, Role, Turn};

pub const ID: &str = "deepseek-web";
const BASE: &str = "https://chat.deepseek.com";
const COMPLETION_PATH: &str = "/api/v0/chat/completion";
const USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36";
/// Headers the web client sends on every `/api/v0/*` call.
const CLIENT_HEADERS: [(&str, &str); 14] = [
    ("Accept", "*/*"),
    ("Accept-Language", "en-US,en;q=0.9"),
    ("Origin", BASE),
    ("Referer", "https://chat.deepseek.com/"),
    ("User-Agent", USER_AGENT),
    ("X-Client-Bundle-Id", "com.deepseek.chat"),
    ("X-Client-Locale", "en-US"),
    ("X-Client-Platform", "web"),
    ("X-Client-Version", "2.0.0"),
    ("sec-ch-ua", "\"Chromium\";v=\"149\", \"Google Chrome\";v=\"149\", \"Not.A/Brand\";v=\"99\""),
    ("sec-ch-ua-mobile", "?0"),
    ("sec-ch-ua-platform", "\"macOS\""),
    ("Sec-Fetch-Mode", "cors"),
    ("Sec-Fetch-Site", "same-origin"),
];
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const ACCESS_TTL: Duration = Duration::from_secs(60 * 60);
const CHAT_TTL: Duration = Duration::from_secs(6 * 60 * 60);
const MAX_CHATS: usize = 64;
/// How long to wait for trailing events once the message is complete.
const LINGER: Duration = Duration::from_secs(2);
pub const TOKEN_REJECTED: &str =
    "DeepSeek rejected the userToken; sign in at chat.deepseek.com again and paste a fresh one";

struct Account {
    user_token: String,
    access: String,
    expires: Instant,
    /// The site's analytics cookies, made up once per account like a browser keeps them.
    cookie: String,
}

struct Chat {
    session: String,
    /// Message id of the last reply, the parent of the next message.
    parent: i64,
    at: Instant,
}

static ACCOUNTS: LazyLock<Mutex<HashMap<String, Account>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static CHATS: LazyLock<Mutex<HashMap<String, Chat>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// The web API is a plain OpenAI-chat provider as far as the router is concerned; the
/// request is made in `send`, which always streams.
pub fn patch(p: &mut Provider) {
    if let Some(t) = p.transport.as_mut() {
        t.format = Format::Openai;
        t.force_stream = true;
        t.base_url = format!("{BASE}{COMPLETION_PATH}");
    }
}

fn fail(status: u16, msg: impl Into<String>) -> ExecError {
    ExecError::Status { status, body: msg.into() }
}

fn network(e: reqwest::Error) -> ExecError {
    ExecError::Network(e.to_string())
}

/// The token as pasted: raw, quoted, `Bearer …`, or the `{"value": …}` object localStorage holds.
fn user_token(raw: &str) -> Option<String> {
    let t = raw.trim();
    let t = match serde_json::from_str::<Value>(t) {
        Ok(Value::Object(o)) => o.get("value").and_then(Value::as_str)?.to_string(),
        Ok(Value::String(s)) => s,
        _ => t.to_string(),
    };
    let t = t.trim();
    let t = t.strip_prefix("Bearer ").unwrap_or(t).trim();
    (!t.is_empty()).then(|| t.to_string())
}

fn fake_cookie() -> String {
    let now = time::OffsetDateTime::now_utc();
    let ms = now.unix_timestamp_nanos() / 1_000_000;
    let uuid = || uuid::Uuid::new_v4().to_string();
    format!(
        "intercom-HWWAFSESTIME={ms}; HWWAFSESID={}; Hm_lvt_{}={}; _frid={}",
        util::random_hex(9),
        uuid(),
        now.unix_timestamp(),
        uuid()
    )
}

/// DeepSeek's reply envelope: `{code, msg, data: {biz_code, biz_msg, biz_data}}`.
fn biz_data(v: &Value) -> Result<&Value, ExecError> {
    let code = v.get("code").and_then(Value::as_i64).unwrap_or(0);
    let biz = v.pointer("/data/biz_code").and_then(Value::as_i64).unwrap_or(0);
    if code == 0 && biz == 0 {
        return v
            .pointer("/data/biz_data")
            .or_else(|| v.get("biz_data"))
            .ok_or_else(|| fail(502, "DeepSeek answered without data"));
    }
    let msg = v
        .pointer("/data/biz_msg")
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty())
        .or_else(|| v.get("msg").and_then(Value::as_str))
        .unwrap_or("request refused");
    let lower = msg.to_ascii_lowercase();
    let status =
        if code == 40003 || (lower.contains("token") && (lower.contains("invalid") || lower.contains("expired"))) {
            401
        } else if code == 40002 || lower.contains("frequent") || lower.contains("rate limit") {
            429
        } else {
            502
        };
    Err(if status == 401 {
        fail(401, TOKEN_REJECTED)
    } else {
        fail(status, format!("DeepSeek error {code}/{biz}: {msg}"))
    })
}

fn http_error(status: u16, text: &str) -> ExecError {
    match status {
        401 => fail(401, TOKEN_REJECTED),
        403 => fail(403, "DeepSeek blocked the request (403), probably its anti-bot check; try again later"),
        s => fail(s, format!("DeepSeek HTTP {s}: {}", crate::executor::extract_message(text))),
    }
}

async fn json_of(resp: reqwest::Response) -> Result<Value, ExecError> {
    let status = resp.status().as_u16();
    let text = resp.text().await.map_err(network)?;
    if !(200..300).contains(&status) {
        return Err(http_error(status, &text));
    }
    serde_json::from_str(&text)
        .map_err(|_| fail(502, format!("DeepSeek returned non-JSON: {}", text.chars().take(200).collect::<String>())))
}

fn call(state: &AppState, method: Method, path: &str, token: &str, cookie: &str) -> RequestBuilder {
    let mut r = state.http.request(method, format!("{BASE}{path}")).bearer_auth(token).header("Cookie", cookie);
    for (k, v) in CLIENT_HEADERS {
        r = r.header(k, v);
    }
    r
}

/// Access token and cookie for the account, exchanging the userToken when needed.
async fn access(state: &AppState, conn_id: &str, user_token: &str) -> Result<(String, String), ExecError> {
    let known = {
        let accounts = ACCOUNTS.lock();
        accounts
            .get(conn_id)
            .filter(|a| a.user_token == user_token)
            .map(|a| (a.access.clone(), a.cookie.clone(), a.expires))
    };
    let cookie = match known {
        Some((access, cookie, expires)) if expires > Instant::now() => return Ok((access, cookie)),
        Some((_, cookie, _)) => cookie,
        None => fake_cookie(),
    };
    let resp = call(state, Method::GET, "/api/v0/users/current", user_token, &cookie)
        .timeout(HTTP_TIMEOUT)
        .send()
        .await
        .map_err(network)?;
    let v = json_of(resp).await?;
    let access =
        biz_data(&v)?.get("token").and_then(Value::as_str).ok_or_else(|| fail(401, TOKEN_REJECTED))?.to_string();
    ACCOUNTS.lock().insert(
        conn_id.to_string(),
        Account {
            user_token: user_token.to_string(),
            access: access.clone(),
            expires: Instant::now() + ACCESS_TTL,
            cookie: cookie.clone(),
        },
    );
    Ok((access, cookie))
}

async fn create_session(state: &AppState, access: &str, cookie: &str) -> Result<String, ExecError> {
    let resp = call(state, Method::POST, "/api/v0/chat_session/create", access, cookie)
        .timeout(HTTP_TIMEOUT)
        .json(&json!({}))
        .send()
        .await
        .map_err(network)?;
    let v = json_of(resp).await?;
    let data = biz_data(&v)?;
    data.pointer("/chat_session/id")
        .or_else(|| data.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| fail(502, "DeepSeek returned no chat session id"))
}

async fn pow_challenge(state: &AppState, access: &str, cookie: &str) -> Result<Challenge, ExecError> {
    let body = json!({"target_path": COMPLETION_PATH});
    let resp = call(state, Method::POST, "/api/v0/chat/create_pow_challenge", access, cookie)
        .timeout(HTTP_TIMEOUT)
        .json(&body)
        .send()
        .await
        .map_err(network)?;
    let v = json_of(resp).await?;
    let ch = biz_data(&v)?.get("challenge").ok_or_else(|| fail(502, "DeepSeek returned no proof-of-work challenge"))?;
    Challenge::parse(ch).map_err(|e| fail(502, format!("DeepSeek proof-of-work challenge: {e}")))
}

fn completion_body(req: &Request, session: &str, parent: Option<i64>, prompt: &str) -> Value {
    json!({
        "chat_session_id": session,
        "parent_message_id": parent,
        "model_type": req.model_type,
        "prompt": prompt,
        "ref_file_ids": [],
        "thinking_enabled": req.thinking,
        "search_enabled": req.search,
        "preempt": false,
    })
}

/// Post one prompt to a chat. The reply is still the raw event stream.
async fn complete(
    state: &AppState,
    access: &str,
    cookie: &str,
    req: &Request,
    session: &str,
    parent: Option<i64>,
    prompt: &str,
) -> Result<reqwest::Response, ExecError> {
    let ch = pow_challenge(state, access, cookie).await?;
    let answer = deepseek_pow::solve(ch.clone()).await.map_err(|e| fail(502, format!("proof of work failed: {e}")))?;
    let pow = deepseek_pow::response_header(&ch, answer, COMPLETION_PATH);
    let body = completion_body(req, session, parent, prompt);
    let post = call(state, Method::POST, COMPLETION_PATH, access, cookie)
        .header("x-ds-pow-response", pow)
        .header("X-Client-Timezone-Offset", "0")
        .json(&body)
        .send();
    // Not `.timeout()`: that would also cut the stream that follows.
    let resp = tokio::time::timeout(HTTP_TIMEOUT, post)
        .await
        .map_err(|_| ExecError::Network("timeout error: DeepSeek did not answer".into()))?
        .map_err(network)?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        return Err(http_error(status, &resp.text().await.unwrap_or_default()));
    }
    let json = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("application/json"));
    if json {
        // Refusals (rate limit, bad token) come as a JSON body with status 200.
        let v = json_of(resp).await?;
        return Err(biz_data(&v).err().unwrap_or_else(|| fail(502, "DeepSeek answered a completion with JSON")));
    }
    Ok(resp)
}

fn take_chat(key: &str) -> Option<Chat> {
    let chat = CHATS.lock().remove(key)?;
    (chat.at.elapsed() < CHAT_TTL).then_some(chat)
}

fn keep_chat(key: String, chat: Chat) {
    let mut chats = CHATS.lock();
    chats.retain(|_, c| c.at.elapsed() < CHAT_TTL);
    if chats.len() >= MAX_CHATS {
        if let Some(oldest) = chats.iter().min_by_key(|(_, c)| c.at).map(|(k, _)| k.clone()) {
            chats.remove(&oldest);
        }
    }
    chats.insert(key, chat);
}

/// Forward the event stream as chat completion chunks. A finished message is remembered
/// as a chat under the key of the transcript that will echo it back.
async fn pump(
    resp: reqwest::Response,
    mut tr: FromDeepseek,
    mut req: Request,
    conn_id: String,
    session: String,
    stall: Duration,
    tx: mpsc::Sender<Result<Bytes, std::io::Error>>,
) {
    let mut upstream = resp.bytes_stream();
    let mut parser = sse::Parser::default();
    loop {
        let wait = if tr.finished { LINGER } else { stall };
        let events = match tokio::time::timeout(wait, upstream.next()).await {
            Ok(Some(Ok(chunk))) => parser.push(&chunk),
            Ok(Some(Err(e))) => {
                let _ = tx.send(Err(std::io::Error::other(e))).await;
                return;
            }
            Ok(None) => break,
            Err(_) if tr.finished => break,
            Err(_) => {
                let _ =
                    tx.send(Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "DeepSeek stream stalled"))).await;
                return;
            }
        };
        for out in events.iter().flat_map(|ev| tr.feed(ev)) {
            if tx.send(Ok(out.to_bytes())).await.is_err() {
                return;
            }
        }
    }
    let last: Vec<Event> = parser.finish().iter().flat_map(|ev| tr.feed(ev)).collect();
    if !tr.finished {
        let _ = tx.send(Err(std::io::Error::other("DeepSeek stream ended before the answer was complete"))).await;
        return;
    }
    let closing = tr.finish();
    if let Some(parent) = tr.response_id {
        req.turns.push(Turn { role: Role::Assistant, text: tr.transcript_text() });
        keep_chat(req.key(&conn_id, req.turns.len()), Chat { session, parent, at: Instant::now() });
    }
    for out in last.iter().chain(&closing).chain(&[Event::data("[DONE]")]) {
        if tx.send(Ok(out.to_bytes())).await.is_err() {
            return;
        }
    }
}

/// Stand-in for an upstream response whose body is the translated stream.
fn relay(
    resp: reqwest::Response,
    tr: FromDeepseek,
    req: Request,
    conn_id: String,
    session: String,
    stall: Duration,
) -> reqwest::Response {
    let (tx, rx) = mpsc::channel(32);
    tokio::spawn(pump(resp, tr, req, conn_id, session, stall, tx));
    let body = reqwest::Body::wrap_stream(ReceiverStream::new(rx));
    let resp =
        axum::http::Response::builder().status(200).header("content-type", "text/event-stream").body(body).unwrap();
    reqwest::Response::from(resp)
}

async fn run(
    state: &AppState,
    conn: &ProviderConnection,
    model: &str,
    body: &Value,
) -> Result<reqwest::Response, ExecError> {
    let token = conn.secret().and_then(user_token).ok_or_else(|| {
        fail(401, "this account has no DeepSeek userToken; paste the userToken from chat.deepseek.com localStorage")
    })?;
    let req = Request::from_body(model, body);
    if req.prompt(0).trim().is_empty() {
        return Err(fail(400, "no message to send to DeepSeek"));
    }
    let (access, cookie) = access(state, &conn.id, &token).await?;

    let mut started = None;
    if let Some((k, chat)) = req.resume_point().and_then(|k| take_chat(&req.key(&conn.id, k)).map(|c| (k, c))) {
        match complete(state, &access, &cookie, &req, &chat.session, Some(chat.parent), &req.prompt(k)).await {
            Ok(resp) => started = Some((resp, chat.session)),
            Err(e) if matches!(e.status(), 401 | 403 | 429) => return Err(e),
            Err(e) => tracing::info!("deepseek-web: chat cannot be continued ({}), starting a new one", e.message()),
        }
    }
    let (resp, session) = match started {
        Some(s) => s,
        None => {
            let session = create_session(state, &access, &cookie).await?;
            (complete(state, &access, &cookie, &req, &session, None, &req.prompt(0)).await?, session)
        }
    };
    let tr = FromDeepseek::new(model, &req);
    Ok(relay(resp, tr, req, conn.id.clone(), session, Duration::from_millis(state.cfg.stall_timeout_ms)))
}

/// Send a chat completion request (OpenAI shape) to DeepSeek Web.
pub async fn send(
    state: &AppState,
    conn: &ProviderConnection,
    model: &str,
    body: &Value,
) -> Result<reqwest::Response, ExecError> {
    let result = run(state, conn, model, body).await;
    if matches!(&result, Err(e) if e.status() == 401) {
        ACCOUNTS.lock().remove(&conn.id);
    }
    result
}

/// Mark the account as needing a new token, after the router moved on from a 401.
pub fn mark_expired(state: &AppState, conn: &ProviderConnection, msg: &str) {
    let patch = json!({"testStatus": "expired", "lastError": msg, "lastErrorAt": crate::db::now_iso()});
    if let Err(e) = state.db.patch_connection_data(&conn.id, &patch) {
        tracing::warn!("failed to mark {} as expired: {e}", conn.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_taken_from_what_the_user_pasted() {
        assert_eq!(user_token("abc123").as_deref(), Some("abc123"));
        assert_eq!(user_token("  \"abc123\"\n").as_deref(), Some("abc123"));
        assert_eq!(user_token(r#"{"value":"abc123","__version":"0"}"#).as_deref(), Some("abc123"));
        assert_eq!(user_token("Bearer abc123").as_deref(), Some("abc123"));
        assert_eq!(user_token(r#"{"other":1}"#), None);
        assert_eq!(user_token("  "), None);
    }

    #[test]
    fn replies_map_to_statuses() {
        let ok = json!({"code": 0, "msg": "", "data": {"biz_code": 0, "biz_msg": "", "biz_data": {"token": "t"}}});
        assert_eq!(biz_data(&ok).unwrap()["token"], "t");
        let status = |v: Value| biz_data(&v).unwrap_err().status();
        assert_eq!(status(json!({"code": 40003, "msg": "Authorization Failed"})), 401);
        assert_eq!(status(json!({"code": 40002, "msg": "x"})), 429);
        assert_eq!(status(json!({"code": 0, "data": {"biz_code": 3, "biz_msg": "Messages too frequently"}})), 429);
        assert_eq!(status(json!({"code": 0, "data": {"biz_code": 9, "biz_msg": "Token expired"}})), 401);
        assert_eq!(status(json!({"code": 5, "msg": "boom"})), 502);
        assert_eq!(http_error(401, "").status(), 401);
        assert_eq!(http_error(403, "<html>").status(), 403);
    }

    #[test]
    fn completion_request_matches_the_web_client() {
        let req = Request::from_body(
            "deepseek-v4-pro-think-search",
            &json!({"messages": [{"role": "user", "content": "hi"}]}),
        );
        let body = completion_body(&req, "sid", Some(7), "hi");
        assert_eq!(
            body,
            json!({
                "chat_session_id": "sid", "parent_message_id": 7, "model_type": "expert", "prompt": "hi",
                "ref_file_ids": [], "thinking_enabled": true, "search_enabled": true, "preempt": false
            })
        );
        assert!(completion_body(&req, "sid", None, "hi")["parent_message_id"].is_null());
    }

    #[test]
    fn chats_are_found_once_and_the_cache_is_bounded() {
        keep_chat("k1".into(), Chat { session: "s1".into(), parent: 2, at: Instant::now() });
        let c = take_chat("k1").unwrap();
        assert_eq!((c.session.as_str(), c.parent), ("s1", 2));
        assert!(take_chat("k1").is_none());
        for i in 0..MAX_CHATS + 5 {
            keep_chat(format!("bounded{i}"), Chat { session: "s".into(), parent: 1, at: Instant::now() });
        }
        assert!(CHATS.lock().len() <= MAX_CHATS);
    }

    #[test]
    fn registry_entry_is_a_streamed_openai_provider() {
        let reg = crate::providers::Registry::load(None).unwrap();
        let p = reg.get(ID).unwrap();
        let t = p.transport_for(Format::Claude, "deepseek-chat").unwrap();
        assert!(t.force_stream && t.format == Format::Openai);
        assert!(p.accepts_api_key() && p.model("deepseek-v4-pro-think").is_some());
        assert_eq!(reg.resolve_alias("ds-web"), Some(ID));
    }
}
