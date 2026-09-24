//! `codex app-server`: JSON-RPC over stdio (SPEC R4), for live usage and the identity that comes
//! with it (R10).

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

use crate::launch;
use crate::probe;
use crate::registry::Account;

pub const ARGS: &[&str] = &["app-server"];
pub const ACCOUNT_READ: &str = "account/read";
pub const RATE_LIMITS_READ: &str = "account/rateLimits/read";

/// The id of the first request after the handshake; the others follow it.
const FIRST_ID: u64 = 2;

/// `requests` (`(method, params)`, ids 2, 3, … in order) after `initialize` (id 1) and
/// `initialized`, in one `codex app-server` run in `account`'s environment. Waits for every
/// answer; each request's `result`, or its error: "`codex app-server` <method>: <message>"
/// ("error <code>" without a message). Err: "`codex app-server` <why>" when it could not answer
/// them all.
pub fn call(
    program: &Path,
    account: &Account,
    requests: &[(&str, Value)],
    timeout: Duration,
) -> Result<Vec<Result<Value, String>>, String> {
    let ids: Vec<u64> = (FIRST_ID..).take(requests.len()).collect();
    let mut answers = probe::run_json_rpc(
        program,
        ARGS,
        &launch::env_change(account),
        &messages(requests),
        &ids,
        timeout,
    )
    .map_err(|why| format!("`codex app-server` {why}"))?;
    Ok(requests
        .iter()
        .zip(&ids)
        .map(|((method, _), id)| outcome(method, answers.remove(id).unwrap_or(Value::Null)))
        .collect())
}

/// One response: its `result`, or its error for the user.
fn outcome(method: &str, mut response: Value) -> Result<Value, String> {
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

/// The handshake, then the requests.
fn messages(requests: &[(&str, Value)]) -> Vec<String> {
    let handshake = [
        json!({
            "id": 1,
            "method": "initialize",
            "params": {"clientInfo": {"name": "remuda", "version": env!("CARGO_PKG_VERSION")}},
        }),
        json!({"method": "initialized"}),
    ];
    let requests = requests
        .iter()
        .zip(FIRST_ID..)
        .map(|((method, params), id)| json!({"id": id, "method": method, "params": params}));
    handshake
        .into_iter()
        .chain(requests)
        .map(|m| m.to_string())
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
            messages(&[
                (RATE_LIMITS_READ, json!({"excludeResetCreditDetails": true})),
                (ACCOUNT_READ, json!({"refreshToken": false})),
            ]),
            [
                format!(
                    "{{\"id\":1,\"method\":\"initialize\",\"params\":{{\"clientInfo\":\
                     {{\"name\":\"remuda\",\"version\":\"{version}\"}}}}}}"
                ),
                "{\"method\":\"initialized\"}".to_string(),
                "{\"id\":2,\"method\":\"account/rateLimits/read\",\
                 \"params\":{\"excludeResetCreditDetails\":true}}"
                    .to_string(),
                "{\"id\":3,\"method\":\"account/read\",\"params\":{\"refreshToken\":false}}"
                    .to_string(),
            ]
        );
    }

    #[test]
    fn call_returns_each_result_or_error() {
        let dir = tempfile::tempdir().unwrap();
        let account = Account {
            provider: CODEX,
            name: "work".into(),
            home: Home::Path(dir.path().display().to_string()),
        };
        let t = Duration::from_secs(10);
        let one = |name: &str, response: &str| {
            let body = format!(
                "read a; read b; read c; echo '{{\"id\":1,\"result\":{{}}}}'; \
                 echo '{response}'; cat >/dev/null"
            );
            let p = script(dir.path(), name, &body);
            call(&p, &account, &[(ACCOUNT_READ, json!({}))], t)
        };
        assert_eq!(
            one("ok", r#"{"id":2,"result":{"account":null}}"#),
            Ok(vec![Ok(json!({"account": null}))])
        );
        assert_eq!(
            one("err", r#"{"id":2,"error":{"code":-1,"message":"no"}}"#),
            Ok(vec![Err("`codex app-server` account/read: no".to_string())])
        );
        assert_eq!(
            one("code", r#"{"id":2,"error":{"code":-32600}}"#),
            Ok(vec![Err(
                "`codex app-server` account/read: error -32600".to_string()
            )])
        );
        // Two requests in one run: answered out of order, each on its own.
        let p = script(
            dir.path(),
            "two",
            "read a; read b; read c; read d; \
             echo '{\"id\":3,\"error\":{\"code\":-1,\"message\":\"later\"}}'; \
             echo '{\"method\":\"n\",\"params\":{}}'; echo '{\"id\":2,\"result\":7}'; \
             cat >/dev/null",
        );
        assert_eq!(
            call(
                &p,
                &account,
                &[(RATE_LIMITS_READ, json!({})), (ACCOUNT_READ, json!({}))],
                t
            ),
            Ok(vec![
                Ok(json!(7)),
                Err("`codex app-server` account/read: later".to_string())
            ])
        );
        let p = script(dir.path(), "exit", "exit 2");
        assert_eq!(
            call(&p, &account, &[(ACCOUNT_READ, json!({}))], t),
            Err("`codex app-server` exited with status 2".to_string())
        );
    }
}
