//! Minimal JSON-RPC 2.0 message model (single messages only; batches are rejected).

use serde::Serialize;
use serde_json::{json, Map, Value};

/// The only accepted value of the `jsonrpc` member.
pub const JSONRPC_VERSION: &str = "2.0";

/// Standard and server-defined JSON-RPC error codes used by this server.
pub mod codes {
    /// Invalid JSON was received.
    pub const PARSE_ERROR: i64 = -32700;
    /// The JSON sent is not a valid request object.
    pub const INVALID_REQUEST: i64 = -32600;
    /// The method does not exist.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// Invalid method parameters (including unknown tool names and bad tool arguments).
    pub const INVALID_PARAMS: i64 = -32602;
    /// Internal error.
    pub const INTERNAL_ERROR: i64 = -32603;
    /// Server-defined: a requested scope would widen the caller's current scope.
    pub const SCOPE_ESCALATION: i64 = -32001;
    /// Server-defined: the entity does not exist *or* is outside the caller's scope.
    /// The two cases are deliberately indistinguishable.
    pub const NOT_FOUND_IN_SCOPE: i64 = -32002;
}

/// A parsed request or notification.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    /// `None` for a notification (no `id` member). Otherwise a string or number.
    pub id: Option<Value>,
    /// Method name.
    pub method: String,
    /// Structured parameters, if present (object or array).
    pub params: Option<Value>,
}

impl Request {
    /// `true` when the message carried no `id` and therefore must not be answered.
    #[must_use]
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

/// JSON-RPC error object.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ErrorObject {
    /// Error code, see [`codes`].
    pub code: i64,
    /// Short description.
    pub message: String,
    /// Optional structured detail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ErrorObject {
    /// Error with no `data`.
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// Attach structured `data`.
    #[must_use]
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

/// Either a `result` or an `error` member.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    /// Successful result.
    Result(Value),
    /// Error object.
    Error(ErrorObject),
}

/// A JSON-RPC response.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Response {
    /// Always `"2.0"`. A compile-time literal, hence `'static`.
    jsonrpc: &'static str,
    /// Echoed request id, or `null` when the request id could not be determined.
    pub id: Value,
    /// Result or error.
    #[serde(flatten)]
    pub outcome: Outcome,
}

impl Response {
    /// Successful response.
    #[must_use]
    pub fn success(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            outcome: Outcome::Result(result),
        }
    }

    /// Error response.
    #[must_use]
    pub fn failure(id: Value, error: ErrorObject) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            outcome: Outcome::Error(error),
        }
    }

    /// The result, if this is a success.
    #[must_use]
    pub fn result(&self) -> Option<&Value> {
        match &self.outcome {
            Outcome::Result(v) => Some(v),
            Outcome::Error(_) => None,
        }
    }

    /// The error, if this is a failure.
    #[must_use]
    pub fn error(&self) -> Option<&ErrorObject> {
        match &self.outcome {
            Outcome::Result(_) => None,
            Outcome::Error(e) => Some(e),
        }
    }
}

fn invalid_request(id: Value, why: &str) -> Response {
    Response::failure(
        id,
        ErrorObject::new(codes::INVALID_REQUEST, "invalid request")
            .with_data(json!({ "detail": why })),
    )
}

/// Parse one line of input into a [`Request`].
///
/// # Errors
///
/// Returns the ready-to-send error [`Response`]: `-32700` for invalid JSON, `-32600` for a
/// well-formed JSON value that is not a valid single request object.
pub fn parse_message(line: &str) -> Result<Request, Response> {
    let value: Value = serde_json::from_str(line).map_err(|e| {
        Response::failure(
            Value::Null,
            ErrorObject::new(codes::PARSE_ERROR, "parse error")
                .with_data(json!({ "detail": e.to_string() })),
        )
    })?;
    let Value::Object(obj) = value else {
        return Err(invalid_request(
            Value::Null,
            "expected a single JSON object; batches are not supported",
        ));
    };
    parse_object(obj)
}

fn parse_object(mut obj: Map<String, Value>) -> Result<Request, Response> {
    let id = match obj.remove("id") {
        None => None,
        Some(v @ (Value::String(_) | Value::Number(_))) => Some(v),
        Some(_) => {
            return Err(invalid_request(
                Value::Null,
                "id must be a string or a number",
            ))
        }
    };
    // Only cloned on the error path, to echo the id back.
    let reply_id = || id.clone().unwrap_or(Value::Null);

    if obj.get("jsonrpc").and_then(Value::as_str) != Some(JSONRPC_VERSION) {
        return Err(invalid_request(reply_id(), "jsonrpc must be \"2.0\""));
    }
    let method = match obj.remove("method") {
        Some(Value::String(m)) => m,
        _ => return Err(invalid_request(reply_id(), "method must be a string")),
    };
    let params = match obj.remove("params") {
        None => None,
        Some(v @ (Value::Object(_) | Value::Array(_))) => Some(v),
        Some(_) => {
            return Err(invalid_request(
                reply_id(),
                "params must be an object or an array",
            ))
        }
    };
    Ok(Request { id, method, params })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_json_is_parse_error_with_null_id() {
        let err = parse_message("{not json").unwrap_err();
        assert_eq!(err.id, Value::Null);
        assert_eq!(err.error().unwrap().code, codes::PARSE_ERROR);
    }

    #[test]
    fn non_object_and_batches_are_invalid_requests() {
        for line in [
            "[]",
            "[{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}]",
            "42",
        ] {
            let err = parse_message(line).unwrap_err();
            assert_eq!(err.error().unwrap().code, codes::INVALID_REQUEST, "{line}");
        }
    }

    #[test]
    fn wrong_version_missing_method_bad_id_are_invalid_requests() {
        let cases = [
            r#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#,
            r#"{"id":1,"method":"ping"}"#,
            r#"{"jsonrpc":"2.0","id":1}"#,
            r#"{"jsonrpc":"2.0","id":{"x":1},"method":"ping"}"#,
            r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#,
            r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":3}"#,
        ];
        for line in cases {
            let err = parse_message(line).unwrap_err();
            assert_eq!(err.error().unwrap().code, codes::INVALID_REQUEST, "{line}");
        }
        // The id is echoed when it was valid.
        let err = parse_message(r#"{"jsonrpc":"1.0","id":7,"method":"ping"}"#).unwrap_err();
        assert_eq!(err.id, json!(7));
    }

    #[test]
    fn notification_has_no_id() {
        let req =
            parse_message(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).unwrap();
        assert!(req.is_notification());
    }

    #[test]
    fn response_serializes_result_xor_error() {
        let ok = serde_json::to_value(Response::success(json!(1), json!({}))).unwrap();
        assert_eq!(ok, json!({"jsonrpc":"2.0","id":1,"result":{}}));
        let err = serde_json::to_value(Response::failure(
            json!("a"),
            ErrorObject::new(codes::METHOD_NOT_FOUND, "method not found"),
        ))
        .unwrap();
        assert_eq!(
            err,
            json!({"jsonrpc":"2.0","id":"a","error":{"code":-32601,"message":"method not found"}})
        );
    }
}
