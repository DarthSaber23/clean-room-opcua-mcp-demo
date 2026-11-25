//! Read-only tools. Every implementation takes `&Catalog`; there is no mutable access to
//! the dataset anywhere in this crate.

use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use thiserror::Error;

use crate::catalog::{Catalog, Machine};
use crate::oee::{self, OeeError, Totals};
use crate::scope::{Scope, ScopeError, ScopeRequest};

/// Static description of a tool. All fields are compile-time literals, hence `'static`.
#[derive(Debug, Clone, Copy)]
pub struct ToolDef {
    /// Tool name used in `tools/call`.
    pub name: &'static str,
    /// Human-readable title.
    pub title: &'static str,
    /// Description shown to the client.
    pub description: &'static str,
}

/// The complete tool registry. Adding a tool here is the only way to expose one.
pub const TOOLS: [ToolDef; 3] = [
    ToolDef {
        name: "list_machines",
        title: "List machines",
        description: "List machines visible in the effective scope.",
    },
    ToolDef {
        name: "get_machine_status",
        title: "Get machine status",
        description: "State and latest tag readings of one machine in the effective scope.",
    },
    ToolDef {
        name: "get_oee_summary",
        title: "Get OEE summary",
        description: "Per-machine and aggregate OEE (availability x performance x quality) \
                      for the effective scope, computed deterministically from counters.",
    },
];

fn scope_schema() -> Value {
    json!({
        "type": "object",
        "description": "Optional narrowing. Can only restrict the session scope, never widen it.",
        "properties": {
            "sites": { "type": "array", "items": { "type": "string" } },
            "machines": { "type": "array", "items": { "type": "string" } }
        },
        "additionalProperties": false
    })
}

fn input_schema(name: &str) -> Value {
    let mut properties = Map::new();
    let mut required = Vec::new();
    if name == "get_machine_status" {
        properties.insert("machine_id".to_owned(), json!({ "type": "string" }));
        required.push(json!("machine_id"));
    }
    properties.insert("scope".to_owned(), scope_schema());
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

/// The `tools` array returned by `tools/list`. Every tool is annotated read-only,
/// non-destructive, idempotent and closed-world.
#[must_use]
pub fn list() -> Vec<Value> {
    TOOLS
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "title": t.title,
                "description": t.description,
                "inputSchema": input_schema(t.name),
                "annotations": {
                    "readOnlyHint": true,
                    "destructiveHint": false,
                    "idempotentHint": true,
                    "openWorldHint": false
                }
            })
        })
        .collect()
}

/// Why a tool call failed.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ToolError {
    /// No tool with that name.
    #[error("unknown tool: {0}")]
    UnknownTool(String),
    /// Arguments did not match the input schema.
    #[error("invalid arguments: {0}")]
    InvalidArguments(String),
    /// The `scope` argument tried to widen the session scope.
    #[error(transparent)]
    ScopeEscalation(#[from] ScopeError),
    /// The entity is unknown or outside the effective scope.
    #[error("{0} not found in scope")]
    NotFoundInScope(String),
    /// Counters could not produce an OEE value.
    #[error("oee: {0}")]
    Oee(#[from] OeeError),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopeOnlyArgs {
    #[serde(default)]
    scope: Option<ScopeRequest>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MachineArgs {
    machine_id: String,
    #[serde(default)]
    scope: Option<ScopeRequest>,
}

fn parse_args<T: DeserializeOwned>(arguments: Value) -> Result<T, ToolError> {
    serde_json::from_value(arguments).map_err(|e| ToolError::InvalidArguments(e.to_string()))
}

fn effective_scope(
    session_scope: &Scope,
    request: Option<&ScopeRequest>,
    catalog: &Catalog,
) -> Result<Scope, ToolError> {
    match request {
        // Clone: the effective scope is reported back and must not alias the session's.
        None => Ok(session_scope.clone()),
        Some(r) => Ok(session_scope.narrow(r, catalog)?),
    }
}

fn visible<'a>(scope: &'a Scope, catalog: &'a Catalog) -> impl Iterator<Item = &'a Machine> + 'a {
    catalog
        .machines()
        .iter()
        .filter(move |m| scope.permits(&m.site, &m.id))
}

/// Execute a tool against the immutable catalog.
///
/// `arguments` is the (possibly absent, treated as `{}`) `arguments` member of
/// `tools/call`. The returned value is the tool's structured output.
///
/// # Errors
///
/// [`ToolError`] for unknown tools, bad arguments, scope escalation, out-of-scope
/// entities or inconsistent counters.
pub fn call(
    name: &str,
    arguments: Value,
    session_scope: &Scope,
    catalog: &Catalog,
) -> Result<Value, ToolError> {
    match name {
        "list_machines" => {
            let args: ScopeOnlyArgs = parse_args(arguments)?;
            let scope = effective_scope(session_scope, args.scope.as_ref(), catalog)?;
            let machines: Vec<Value> = visible(&scope, catalog)
                .map(|m| {
                    json!({
                        "id": m.id,
                        "site": m.site,
                        "display_name": m.display_name,
                        "state": m.state
                    })
                })
                .collect();
            Ok(json!({ "effective_scope": scope, "machines": machines }))
        }
        "get_machine_status" => {
            let args: MachineArgs = parse_args(arguments)?;
            let scope = effective_scope(session_scope, args.scope.as_ref(), catalog)?;
            let machine = catalog
                .get(&args.machine_id)
                .filter(|m| scope.permits(&m.site, &m.id))
                .ok_or_else(|| {
                    ToolError::NotFoundInScope(format!("machine '{}'", args.machine_id))
                })?;
            Ok(json!({
                "effective_scope": scope,
                "machine": {
                    "id": machine.id,
                    "site": machine.site,
                    "state": machine.state,
                    "tags": machine.tags
                }
            }))
        }
        "get_oee_summary" => {
            let args: ScopeOnlyArgs = parse_args(arguments)?;
            let scope = effective_scope(session_scope, args.scope.as_ref(), catalog)?;
            let mut aggregate = Totals::default();
            let mut per_machine = Vec::new();
            for m in visible(&scope, catalog) {
                let totals = Totals::from_counters(&m.counters);
                aggregate = aggregate.saturating_add(totals);
                let machine_oee = oee::compute(&totals)?;
                per_machine.push(json!({ "id": m.id, "site": m.site, "oee": machine_oee }));
            }
            // An empty scope has no planned time; report null instead of an error.
            let aggregate_oee = if per_machine.is_empty() {
                Value::Null
            } else {
                let value = oee::compute(&aggregate)?;
                json!(value)
            };
            Ok(json!({
                "effective_scope": scope,
                "machines": per_machine,
                "aggregate": { "totals": aggregate, "oee": aggregate_oee }
            }))
        }
        other => Err(ToolError::UnknownTool(other.to_owned())),
    }
}
