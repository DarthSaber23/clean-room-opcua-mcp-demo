//! Read-only MCP server over stdio.
//!
//! The principal is chosen by the `DEMO_PRINCIPAL` environment variable (a synthetic
//! stand-in for a token). There is no default: an unset or unknown value refuses to start.
//! stdout carries protocol messages only; diagnostics go to stderr.

use std::process::ExitCode;

use readonly_mcp::catalog::Catalog;
use readonly_mcp::scope::{lookup_principal, SYNTHETIC_PRINCIPALS};
use readonly_mcp::server::Session;
use readonly_mcp::stdio::serve;
use tokio::io::BufReader;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let token = std::env::var("DEMO_PRINCIPAL").unwrap_or_default();
    let Some(principal) = lookup_principal(&token) else {
        eprintln!(
            "readonly-mcp: set DEMO_PRINCIPAL to one of {SYNTHETIC_PRINCIPALS:?} (refusing to start without a principal)"
        );
        return ExitCode::from(2);
    };
    eprintln!("readonly-mcp: serving principal '{}'", principal.name);

    // The catalog lives for the whole of `main`, so the session simply borrows it.
    let catalog = Catalog::synthetic();
    let mut session = Session::new(&catalog, principal);
    let reader = BufReader::with_capacity(64 * 1024, tokio::io::stdin());
    match serve(&mut session, reader, tokio::io::stdout()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("readonly-mcp: i/o error: {e}");
            ExitCode::FAILURE
        }
    }
}
