//! The pure MCP protocol handler.

use serde_json::{json, Value};

use crate::catalog::Catalog;
use crate::jsonrpc::{codes, ErrorObject, Request, Response};
use crate::scope::{Principal, Scope};
use crate::tools::{self, ToolError};

/// MCP protocol revision this server speaks.
pub const PROTOCOL_VERSION: &str = "2025-06-18";
/// Server name reported in `initialize`.
pub const SERVER_NAME: &str = "readonly-mcp-demo";

/// Per-connection state. The catalog is borrowed immutably for the whole session: the
/// type system, not a convention, guarantees that no request can modify it.
#[derive(Debug)]
pub struct Session<'c> {
    catalog: &'c Catalog,
    principal: String,
    scope: Scope,
    initialized: bool,
}

impl<'c> Session<'c> {
    /// Start a session for `principal` over `catalog`.
    #[must_use]
    pub fn new(catalog: &'c Catalog, principal: Principal) -> Self {
        Self {
            catalog,
            principal: principal.name,
            scope: principal.scope,
            initialized: false,
        }
    }

    /// The session's scope (the principal's scope; this server has no session-level
    /// widening or narrowing, only per-call narrowing).
    #[must_use]
    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    /// Principal name.
    #[must_use]
    pub fn principal(&self) -> &str {
        &self.principal
    }

    /// Whether the client sent `notifications/initialized`.
    #[must_use]
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }
}

/// Handle one parsed message. Returns `None` for notifications (which are never answered).
pub fn handle(session: &mut Session<'_>, request: Request) -> Option<Response> {
    let Request { id, method, params } = request;
    let Some(id) = id else {
        if method == "notifications/initialized" {
            session.initialized = true;
        }
        // Other notifications (e.g. notifications/cancelled) need no action here:
        // requests are processed synchronously, so there is nothing to cancel.
        return None;
    };

    let outcome = match method.as_str() {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "serverInfo": { "name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION") },
            "capabilities": { "tools": { "listChanged": false } },
            "instructions": "Read-only synthetic demo. Results are filtered by the caller's scope."
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools::list() })),
        "tools/call" => tools_call(session, params),
        _ => Err(ErrorObject::new(
            codes::METHOD_NOT_FOUND,
            format!("method not found: {method}"),
        )),
    };
    Some(match outcome {
        Ok(result) => Response::success(id, result),
        Err(error) => Response::failure(id, error),
    })
}

fn invalid_params(detail: impl Into<String>) -> ErrorObject {
    ErrorObject::new(codes::INVALID_PARAMS, "invalid params")
        .with_data(json!({ "detail": detail.into() }))
}

fn tools_call(session: &Session<'_>, params: Option<Value>) -> Result<Value, ErrorObject> {
    let Some(Value::Object(mut params)) = params else {
        return Err(invalid_params(
            "tools/call requires an object with a 'name'",
        ));
    };
    let name = match params.remove("name") {
        Some(Value::String(name)) => name,
        _ => return Err(invalid_params("'name' must be a string")),
    };
    let arguments = match params.remove("arguments") {
        None | Some(Value::Null) => json!({}),
        Some(v @ Value::Object(_)) => v,
        Some(_) => return Err(invalid_params("'arguments' must be an object")),
    };

    match tools::call(&name, arguments, &session.scope, session.catalog) {
        Ok(structured) => Ok(json!({
            "content": [{ "type": "text", "text": structured.to_string() }],
            "structuredContent": structured,
            "isError": false
        })),
        Err(e) => Err(tool_error_to_rpc(&e)),
    }
}

fn tool_error_to_rpc(e: &ToolError) -> ErrorObject {
    let code = match e {
        ToolError::UnknownTool(_) | ToolError::InvalidArguments(_) => codes::INVALID_PARAMS,
        ToolError::ScopeEscalation(_) => codes::SCOPE_ESCALATION,
        ToolError::NotFoundInScope(_) => codes::NOT_FOUND_IN_SCOPE,
        ToolError::Oee(_) => codes::INTERNAL_ERROR,
    };
    ErrorObject::new(code, e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jsonrpc::parse_message;
    use crate::scope::lookup_principal;

    fn session<'c>(catalog: &'c Catalog, principal: &str) -> Session<'c> {
        Session::new(catalog, lookup_principal(principal).unwrap())
    }

    fn rpc(session: &mut Session<'_>, line: &str) -> Option<Response> {
        handle(session, parse_message(line).unwrap())
    }

    fn call(session: &mut Session<'_>, name: &str, args: &Value) -> Response {
        let line = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": name, "arguments": args }
        })
        .to_string();
        rpc(session, &line).unwrap()
    }

    fn structured(resp: &Response) -> &Value {
        &resp.result().unwrap()["structuredContent"]
    }

    fn machine_ids(v: &Value) -> Vec<String> {
        v["machines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn initialize_reports_version_server_info_and_tools_capability() {
        let catalog = Catalog::synthetic();
        let mut s = session(&catalog, "viewer-all");
        let resp = rpc(
            &mut s,
            r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
        )
        .unwrap();
        let r = resp.result().unwrap();
        assert_eq!(r["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(r["serverInfo"]["name"], SERVER_NAME);
        assert!(r["capabilities"]["tools"].is_object());
    }

    #[test]
    fn notification_produces_no_response() {
        let catalog = Catalog::synthetic();
        let mut s = session(&catalog, "viewer-all");
        assert!(!s.is_initialized());
        assert!(rpc(
            &mut s,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
        )
        .is_none());
        assert!(s.is_initialized());
        // Unknown notifications are also silent, never answered with -32601.
        assert!(rpc(
            &mut s,
            r#"{"jsonrpc":"2.0","method":"notifications/unknown"}"#
        )
        .is_none());
    }

    #[test]
    fn ping_returns_empty_object() {
        let catalog = Catalog::synthetic();
        let mut s = session(&catalog, "viewer-all");
        let resp = rpc(&mut s, r#"{"jsonrpc":"2.0","id":"p","method":"ping"}"#).unwrap();
        assert_eq!(resp.id, json!("p"));
        assert_eq!(resp.result().unwrap(), &json!({}));
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let catalog = Catalog::synthetic();
        let mut s = session(&catalog, "viewer-all");
        let resp = rpc(
            &mut s,
            r#"{"jsonrpc":"2.0","id":9,"method":"resources/write"}"#,
        )
        .unwrap();
        assert_eq!(resp.error().unwrap().code, codes::METHOD_NOT_FOUND);
        assert_eq!(resp.id, json!(9));
    }

    #[test]
    fn tools_list_is_all_read_only_and_has_no_write_tool() {
        let catalog = Catalog::synthetic();
        let mut s = session(&catalog, "viewer-all");
        let resp = rpc(&mut s, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#).unwrap();
        let tools = resp.result().unwrap()["tools"].as_array().unwrap().clone();
        assert_eq!(tools.len(), tools::TOOLS.len());
        for t in &tools {
            assert_eq!(t["annotations"]["readOnlyHint"], json!(true), "{t}");
            assert_eq!(t["annotations"]["destructiveHint"], json!(false), "{t}");
            let name = t["name"].as_str().unwrap();
            assert!(
                name.starts_with("list_") || name.starts_with("get_"),
                "tool {name} is not named as a read"
            );
            assert_eq!(t["inputSchema"]["additionalProperties"], json!(false));
        }
    }

    #[test]
    fn tool_calls_never_change_the_catalog() {
        let catalog = Catalog::synthetic();
        let snapshot = catalog.clone();
        let mut s = session(&catalog, "viewer-all");
        for name in tools::TOOLS.iter().map(|t| t.name) {
            let _ = call(&mut s, name, &json!({ "machine_id": "line-a1" }));
            let _ = call(&mut s, name, &json!({}));
        }
        assert_eq!(catalog, snapshot);
    }

    #[test]
    fn bad_params_are_invalid_params() {
        let catalog = Catalog::synthetic();
        let mut s = session(&catalog, "viewer-all");
        let cases = [
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call"}"#,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"arguments":{}}}"#,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"no_such_tool"}}"#,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"get_machine_status","arguments":{}}}"#,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_machines","arguments":{"surprise":1}}}"#,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_machines","arguments":{"scope":{"role":"admin"}}}}"#,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_machines","arguments":[1]}}"#,
        ];
        for line in cases {
            let resp = rpc(&mut s, line).unwrap();
            assert_eq!(resp.error().unwrap().code, codes::INVALID_PARAMS, "{line}");
        }
    }

    #[test]
    fn results_are_filtered_by_principal_scope() {
        let catalog = Catalog::synthetic();
        let mut all = session(&catalog, "viewer-all");
        assert_eq!(
            machine_ids(structured(&call(&mut all, "list_machines", &json!({})))),
            ["line-a1", "line-a2", "line-b1", "line-b2", "line-c1"]
        );
        let mut alpha = session(&catalog, "operator-site-alpha");
        assert_eq!(
            machine_ids(structured(&call(&mut alpha, "list_machines", &json!({})))),
            ["line-a1", "line-a2"]
        );
        let mut b1 = session(&catalog, "operator-line-b1");
        assert_eq!(
            machine_ids(structured(&call(&mut b1, "get_oee_summary", &json!({})))),
            ["line-b1"]
        );
    }

    #[test]
    fn per_call_scope_narrows_results() {
        let catalog = Catalog::synthetic();
        let mut s = session(&catalog, "viewer-all");
        let resp = call(
            &mut s,
            "list_machines",
            &json!({ "scope": { "sites": ["site-beta"] } }),
        );
        assert_eq!(machine_ids(structured(&resp)), ["line-b1", "line-b2"]);
        // The narrowing is per call: the session scope is unchanged afterwards.
        let resp = call(&mut s, "list_machines", &json!({}));
        assert_eq!(machine_ids(structured(&resp)).len(), 5);
    }

    #[test]
    fn escalation_via_scope_argument_is_rejected() {
        let catalog = Catalog::synthetic();
        let mut s = session(&catalog, "operator-site-alpha");
        let resp = call(
            &mut s,
            "list_machines",
            &json!({ "scope": { "sites": ["site-alpha", "site-gamma"] } }),
        );
        assert_eq!(resp.error().unwrap().code, codes::SCOPE_ESCALATION);
        let resp = call(
            &mut s,
            "get_oee_summary",
            &json!({ "scope": { "machines": ["line-c1"] } }),
        );
        assert_eq!(resp.error().unwrap().code, codes::SCOPE_ESCALATION);
    }

    #[test]
    fn out_of_scope_machine_is_indistinguishable_from_unknown() {
        let catalog = Catalog::synthetic();
        let mut s = session(&catalog, "operator-site-alpha");
        let hidden = call(
            &mut s,
            "get_machine_status",
            &json!({ "machine_id": "line-b1" }),
        );
        let missing = call(
            &mut s,
            "get_machine_status",
            &json!({ "machine_id": "line-zz" }),
        );
        assert_eq!(hidden.error().unwrap().code, codes::NOT_FOUND_IN_SCOPE);
        assert_eq!(missing.error().unwrap().code, codes::NOT_FOUND_IN_SCOPE);
        let visible = call(
            &mut s,
            "get_machine_status",
            &json!({ "machine_id": "line-a1" }),
        );
        assert_eq!(
            structured(&visible)["machine"]["tags"][0]["name"],
            "filler.speed"
        );
    }

    #[test]
    fn oee_summary_on_known_synthetic_numbers() {
        let catalog = Catalog::synthetic();
        let mut s = session(&catalog, "operator-line-b1");
        let resp = call(&mut s, "get_oee_summary", &json!({}));
        let v = structured(&resp);
        let oee = v["machines"][0]["oee"]["oee"].as_f64().unwrap();
        assert!((oee - 0.675).abs() < 1e-12, "got {oee}");
        let agg = v["aggregate"]["oee"]["oee"].as_f64().unwrap();
        assert!((agg - 0.675).abs() < 1e-12, "got {agg}");

        // Empty intersection: no machines and a null aggregate, not an error.
        let mut all = session(&catalog, "viewer-all");
        let resp = call(
            &mut all,
            "get_oee_summary",
            &json!({ "scope": { "sites": [] } }),
        );
        assert!(structured(&resp)["aggregate"]["oee"].is_null());
    }
}
