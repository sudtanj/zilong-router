//! ChatGPT-subscription Codex (`codex`, alias `cx`): OpenAI's Auth0 login as the Codex
//! CLI does it (authorization code + PKCE against `auth.openai.com`, loopback redirect
//! on port 1455 that the user pastes back), the account id inside the id token that
//! every request must carry, and the registry fix-ups. The upstream speaks the OpenAI
//! Responses API; the translation lives in `translator::codex`.

use serde_json::Value;

use crate::providers::{Format, Provider};

pub const ID: &str = "codex";
pub const CLIENT: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const AUTHORIZE: &str = "https://auth.openai.com/oauth/authorize";
pub const TOKEN: &str = "https://auth.openai.com/oauth/token";
/// Fixed loopback redirect registered for the Codex CLI client.
pub const REDIRECT: &str = "http://localhost:1455/auth/callback";
pub const SCOPE: &str = "openid profile email offline_access";
pub const CLI_VERSION: &str = "0.159.0";
const AUTH_CLAIM: &str = "https://api.openai.com/auth";
const PROFILE_CLAIM: &str = "https://api.openai.com/profile";

/// What the id token (or the access token) says about the account.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct AccountInfo {
    pub email: Option<String>,
    pub account_id: Option<String>,
    pub plan: Option<String>,
}

fn claim(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

/// Claims of one JWT: `chatgpt_account_id` and the plan live under the
/// `https://api.openai.com/auth` claim, the email at the top level (id token) or under
/// `https://api.openai.com/profile` (access token).
pub fn account_info(token: &str) -> AccountInfo {
    let Some(p) = super::util::jwt_payload(token) else { return AccountInfo::default() };
    let auth = p.get(AUTH_CLAIM).cloned().unwrap_or(Value::Null);
    AccountInfo {
        email: claim(&p, "email").or_else(|| p.get(PROFILE_CLAIM).and_then(|pr| claim(pr, "email"))),
        account_id: claim(&auth, "chatgpt_account_id").or_else(|| claim(&p, "account_id")),
        plan: claim(&auth, "chatgpt_plan_type").or_else(|| claim(&p, "plan_type")),
    }
}

/// Id token first, the access token for whatever it leaves out.
pub fn account_info_for(id_token: Option<&str>, access: &str) -> AccountInfo {
    let mut info = id_token.map(account_info).unwrap_or_default();
    let from_access = account_info(access);
    info.email = info.email.or(from_access.email);
    info.account_id = info.account_id.or(from_access.account_id);
    info.plan = info.plan.or(from_access.plan);
    info
}

/// The upstream is the Responses API, always streamed; the chat <-> Responses
/// translation happens around the request, so the registry entry is treated as
/// `openai` for routing.
pub fn patch(p: &mut Provider) {
    if let Some(t) = p.transport.as_mut() {
        t.format = Format::Openai;
        t.force_stream = true;
        t.headers.entry("originator".into()).or_insert_with(|| "codex_cli_rs".into());
        t.headers.entry("User-Agent".into()).or_insert_with(|| format!("codex_cli_rs/{CLI_VERSION}"));
        t.headers.entry("version".into()).or_insert_with(|| CLI_VERSION.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn jwt(payload: &Value) -> String {
        format!("eyJhbGciOiJub25lIn0.{}.sig", super::super::util::b64url(payload.to_string().as_bytes()))
    }

    #[test]
    fn reads_account_from_id_token() {
        let t = jwt(&json!({
            "email": "me@example.com",
            "https://api.openai.com/auth": {"chatgpt_account_id": "acc-123", "chatgpt_plan_type": "plus"}
        }));
        let i = account_info(&t);
        assert_eq!(i.email.as_deref(), Some("me@example.com"));
        assert_eq!(i.account_id.as_deref(), Some("acc-123"));
        assert_eq!(i.plan.as_deref(), Some("plus"));
        assert_eq!(account_info("not-a-jwt"), AccountInfo::default());
    }

    #[test]
    fn falls_back_to_access_token() {
        let access = jwt(&json!({
            "https://api.openai.com/profile": {"email": "a@b.c"},
            "https://api.openai.com/auth": {"chatgpt_account_id": "acc-9"}
        }));
        let i = account_info_for(None, &access);
        assert_eq!((i.email.as_deref(), i.account_id.as_deref()), (Some("a@b.c"), Some("acc-9")));
        let id = jwt(&json!({"email": "x@y.z"}));
        let i = account_info_for(Some(&id), &access);
        assert_eq!((i.email.as_deref(), i.account_id.as_deref()), (Some("x@y.z"), Some("acc-9")));
    }
}
