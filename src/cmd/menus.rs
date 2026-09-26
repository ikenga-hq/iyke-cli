//! `iyke menus …` — the Menus noun (WP-62): D-06's `iyke menus show
//! <menu-id>` footer line.
//!
//! Bridge route: `GET /iyke/menus/<menu-id>` → `{ schema_version, menu }`.
//! `<menu-id>` is a full menu id per G-ACTIONS §1.3 and may contain `/`
//! (`section/<sectionId>`, `native/<top>`) — the shell routes it with a
//! tail wildcard, so this noun passes it through verbatim.

use std::fmt::Write as _;

use anyhow::Result;
use clap::Subcommand;
use serde_json::Value;

use crate::api::Client;
use crate::cmd::require_bridge_api;
use crate::output::Format;

#[derive(Subcommand)]
pub enum MenusAction {
    /// Show one effective menu: its items in order (separators included),
    /// and the ids hidden from it. G-ACTIONS §1.3 lists the frozen menu
    /// ids — `files`, `artifacts`, `session`, `palette`,
    /// `section/<sectionId>`, `native/<top>`, …
    Show {
        /// A full menu id, e.g. `files`, `section/automations`, `native/file`.
        menu_id: String,
    },
}

pub fn run(client: &Client, action: MenusAction, fmt: Format) -> Result<()> {
    match action {
        MenusAction::Show { menu_id } => {
            // WP-62 review (C4): this route's own 404 (an unknown menu id)
            // and "shell too old to have it at all" both come back as plain
            // HTTP 404 — tell them apart up front instead of guessing from
            // the status code.
            require_bridge_api(client, 4, "menus show")?;
            let encoded = percent_encode_menu_id(&menu_id);
            let v = client.get_with_query(&format!("/iyke/menus/{encoded}"), &[])?;
            print_menu(&v, fmt);
        }
    }
    Ok(())
}

/// Percent-encode `menu_id` as one opaque path segment, including its own
/// `/` (`section/<sectionId>` — WP-62 review C4). The shell's route is a
/// tail wildcard (`/iyke/menus/*menu_id`) whose `Path<String>` extractor
/// percent-decodes the whole captured tail back to one string, so this
/// round-trips exactly while no longer depending on `menu_id` happening to
/// already look like URL path segments.
fn percent_encode_menu_id(menu_id: &str) -> String {
    let mut out = String::with_capacity(menu_id.len());
    for byte in menu_id.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn print_menu(v: &Value, fmt: Format) {
    print!("{}", render_menu(v, fmt));
}

/// WP-62 review ("make the print tests assert on the output"): renders to a
/// `String` instead of printing directly, so tests can assert on exact
/// content rather than just "did not panic". `print_menu` is the thin
/// runtime wrapper.
fn render_menu(v: &Value, fmt: Format) -> String {
    let mut out = String::new();
    match fmt {
        Format::Json => {
            let _ = writeln!(out, "{v}");
        }
        Format::Human => {
            let menu = v.get("menu").unwrap_or(v);
            let id = menu.get("id").and_then(Value::as_str).unwrap_or("?");
            let _ = writeln!(out, "menu: {id}");
            let items = menu
                .get("items")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if items.is_empty() {
                let _ = writeln!(out, "(no items)");
            }
            for item in &items {
                if item.get("kind").and_then(Value::as_str) == Some("separator") {
                    let _ = writeln!(out, "  ---");
                    continue;
                }
                let item_id = item.get("id").and_then(Value::as_str).unwrap_or("?");
                let name = item.get("name").and_then(Value::as_str).unwrap_or("-");
                let source = item.get("source").and_then(Value::as_str).unwrap_or("?");
                let _ = writeln!(out, "  {item_id:<28} {source:<10} {name}");
            }
            if let Some(hidden) = menu.get("hidden").and_then(Value::as_array) {
                if !hidden.is_empty() {
                    let names: Vec<&str> = hidden.iter().filter_map(Value::as_str).collect();
                    let _ = writeln!(out, "hidden: {}", names.join(", "));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn percent_encode_menu_id_escapes_the_structural_slash() {
        assert_eq!(percent_encode_menu_id("files"), "files");
        assert_eq!(
            percent_encode_menu_id("section/automations"),
            "section%2Fautomations"
        );
        assert_eq!(percent_encode_menu_id("native/file"), "native%2Ffile");
        assert_eq!(
            percent_encode_menu_id("section/com.ikenga.git:panel"),
            "section%2Fcom.ikenga.git%3Apanel"
        );
    }

    #[test]
    fn print_menu_renders_items_separators_and_hidden() {
        let out = render_menu(
            &json!({
                "menu": {
                    "id": "files",
                    "items": [
                        {"kind": "action", "id": "open", "name": "Open", "source": "builtin"},
                        {"kind": "separator"},
                        {"kind": "action", "id": "explain-file", "name": "Explain this file", "source": "personal"}
                    ],
                    "hidden": ["copy-name"]
                }
            }),
            Format::Human,
        );
        let lines: Vec<Vec<&str>> = out.lines().map(|l| l.split_whitespace().collect()).collect();
        assert_eq!(
            lines,
            vec![
                vec!["menu:", "files"],
                vec!["open", "builtin", "Open"],
                vec!["---"],
                vec!["explain-file", "personal", "Explain", "this", "file"],
                vec!["hidden:", "copy-name"],
            ]
        );
    }

    #[test]
    fn print_menu_handles_empty_and_bare_shapes_without_panicking() {
        let empty = render_menu(&json!({"menu": {"id": "empty", "items": [], "hidden": []}}), Format::Human);
        assert_eq!(empty, "menu: empty\n(no items)\n");

        // Accepts the bare menu object too (no `menu` wrapper), same
        // tolerance as `cmd::project::print_sections`.
        let bare = render_menu(&json!({"id": "bare", "items": [], "hidden": []}), Format::Human);
        assert_eq!(bare, "menu: bare\n(no items)\n");

        let as_json = render_menu(&json!({"menu": {"id": "x", "items": []}}), Format::Json);
        assert_eq!(
            as_json.trim(),
            json!({"menu": {"id": "x", "items": []}}).to_string()
        );
    }
}
