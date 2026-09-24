//! `codex app-server`: JSON-RPC over stdio (SPEC R4), for identity (R10a) and live usage (R10).

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

use crate::launch;
use crate::probe;
use crate::registry::Account;

pub const ARGS: &[&str] = &["app-server"];
pub const ACCOUNT_READ: &str = "account/read";
pub const RATE_LIMITS_READ: &str = "account/rateLimits/read";

/// The id of the one request after the handshake.
const REQUEST_ID: u64 = 2;

/// `method` with `params` (id 2) after `initialize` (id 1) and `initialized`, in `account`'s
/// environment; the `result`. Err: "`codex app-server` <why>" when it could not answer, or
/// "`codex app-server` <method>: <message>" for an error response ("error <code>" without message).
pub fn call(
    program: &Path,
    account: &Account,
    method: &str,
    params: Value,
    timeout: Duration,
) -> Result<Value, String> {
    let lines = messages(method, params);
    let mut answers = probe::run_json_rpc(
        program,
        ARGS,
        &launch::env_change(account),
        &lines,
        &[REQUEST_ID],
        timeout,
    )
    .map_err(|why| format!("`codex app-server` {why}"))?;
    let mut response = answers.remove(&REQUEST_ID).unwrap_or(Value::Null);
    if let Some(error) = response.get("error") {
        let message = match error.get("message").and_then(Value::as_str) {
            Some(message) => message.to_string(),
            None => match error.get("code") {
                Some(code) => format!("error {code}"),
                None => "error".to_string(),
            },
        };
        return Err(format!("`codex app-server` {method}: {message}"));
    }
    Ok(response
        .get_mut("result")
        .map(Value::take)
        .unwrap_or(Value::Null))
}

/// The handshake, then the request.
fn messages(method: &str, params: Value) -> Vec<String> {
    [
        json!({
            "id": 1,
            "method": "initialize",
            "params": {"clientInfo": {"name": "remuda", "version": env!("CARGO_PKG_VERSION")}},
        }),
        json!({"method": "initialized"}),
        json!({"id": REQUEST_ID, "method": method, "params": params}),
    ]
    .iter()
    .map(Value::to_string)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::script;
    use crate::registry::{CODEX, Home};

    #[test]
    fn the_handshake_comes_first() {
        let version = env!("CARGO_PKG_VERSION");
        assert_eq!(
            messages(ACCOUNT_READ, json!({"refreshToken": false})),
            [
                format!(
                    "{{\"id\":1,\"method\":\"initialize\",\"params\":{{\"clientInfo\":\
                     {{\"name\":\"remuda\",\"version\":\"{version}\"}}}}}}"
                ),
                "{\"method\":\"initialized\"}".to_string(),
                "{\"id\":2,\"method\":\"account/read\",\"params\":{\"refreshToken\":false}}"
                    .to_string(),
            ]
        );
    }

    #[test]
    fn call_returns_the_result_or_the_error() {
        let dir = tempfile::tempdir().unwrap();
        let account = Account {
            provider: CODEX,
            name: "work".into(),
            home: Home::Path(dir.path().display().to_string()),
        };
        let t = Duration::from_secs(10);
        let answer = |name: &str, response: &str| {
            let body = format!(
                "read a; read b; read c; echo '{{\"id\":1,\"result\":{{}}}}'; \
                 echo '{response}'; cat >/dev/null"
            );
            let p = script(dir.path(), name, &body);
            call(&p, &account, ACCOUNT_READ, json!({}), t)
        };
        assert_eq!(
            answer("ok", r#"{"id":2,"result":{"account":null}}"#),
            Ok(json!({"account": null}))
        );
        assert_eq!(
            answer("err", r#"{"id":2,"error":{"code":-1,"message":"no"}}"#),
            Err("`codex app-server` account/read: no".to_string())
        );
        assert_eq!(
            answer("code", r#"{"id":2,"error":{"code":-32600}}"#),
            Err("`codex app-server` account/read: error -32600".to_string())
        );
        let p = script(dir.path(), "exit", "exit 2");
        assert_eq!(
            call(&p, &account, ACCOUNT_READ, json!({}), t),
            Err("`codex app-server` exited with status 2".to_string())
        );
    }
}
