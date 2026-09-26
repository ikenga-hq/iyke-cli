//! Noun subcommands (WP-21b): `iyke project …` and `iyke ngwa …`.
//!
//! Each noun module owns its clap subcommand enum, its bridge calls, and its
//! human/JSON renderers — same split the rest of `main.rs` uses, just lifted
//! out so the file stops growing linearly with the bridge surface.

use anyhow::{anyhow, Error};

pub mod actions;
pub mod keys;
pub mod menus;
pub mod ngwa;
pub mod project;

/// Rewrite a bare HTTP 404 on a route the running shell doesn't expose yet
/// into an actionable error naming the missing route and its owning work
/// package. Any other failure (transport, 500, auth) passes through
/// untouched — a missing route is the only case where "this iyke is newer
/// than this shell" is the right diagnosis.
pub fn missing_route(path: &str, what: &str, e: Error) -> Error {
    if e.to_string().contains("returned HTTP 404") {
        anyhow!(
            "{path} is not exposed by this shell — {what} is pending a WP-28 \
             bridge route (WP-21b filed the gap as needs-decision). \
             Underlying error: {e}"
        )
    } else {
        e
    }
}

/// WP-62 review (C4): for a route whose own legitimate 404 (an unknown menu
/// id, `/iyke/menus/:id`) is indistinguishable by status code alone from
/// "an older shell doesn't have this route at all", [`missing_route`]'s
/// blanket "was this a 404" rewrite would misdiagnose the first as the
/// second. Checking the shell's self-reported `shell.bridge_api`
/// (`GET /iyke/state`, `src-tauri/src/iyke/handlers.rs`'s `BRIDGE_API` doc
/// comment) up front tells the two apart before the request that might 404
/// even runs.
pub fn require_bridge_api(client: &crate::api::Client, min: u32, what: &str) -> Result<(), Error> {
    let state = client.get_state()?;
    let level = state
        .get("shell")
        .and_then(|s| s.get("bridge_api"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    if level < u64::from(min) {
        return Err(anyhow!(
            "{what} needs a newer Ikenga shell (bridge_api >= {min}; this shell reports {level}). \
             Update the Ikenga desktop app."
        ));
    }
    Ok(())
}
