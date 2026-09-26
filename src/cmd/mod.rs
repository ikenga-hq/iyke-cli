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
