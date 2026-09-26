//! `iyke menus …` — the Menus noun (WP-62): D-06's `iyke menus show
//! <menu-id>` footer line.
//!
//! Bridge route: `GET /iyke/menus/<menu-id>` → `{ schema_version, menu }`.
//! `<menu-id>` is a full menu id per G-ACTIONS §1.3 and may contain `/`
//! (`section/<sectionId>`, `native/<top>`) — the shell routes it with a
//! tail wildcard, so this noun passes it through verbatim.

use anyhow::Result;
use clap::Subcommand;
use serde_json::Value;

use crate::api::Client;
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
            let v = client.get_with_query(&format!("/iyke/menus/{menu_id}"), &[])?;
            print_menu(&v, fmt);
        }
    }
    Ok(())
}

fn print_menu(v: &Value, fmt: Format) {
    match fmt {
        Format::Json => println!("{v}"),
        Format::Human => {
            let menu = v.get("menu").unwrap_or(v);
            let id = menu.get("id").and_then(Value::as_str).unwrap_or("?");
            println!("menu: {id}");
            let items = menu
                .get("items")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if items.is_empty() {
                println!("(no items)");
            }
            for item in &items {
                if item.get("kind").and_then(Value::as_str) == Some("separator") {
                    println!("  ---");
                    continue;
                }
                let item_id = item.get("id").and_then(Value::as_str).unwrap_or("?");
                let name = item.get("name").and_then(Value::as_str).unwrap_or("-");
                let source = item.get("source").and_then(Value::as_str).unwrap_or("?");
                println!("  {item_id:<28} {source:<10} {name}");
            }
            if let Some(hidden) = menu.get("hidden").and_then(Value::as_array) {
                if !hidden.is_empty() {
                    let names: Vec<&str> = hidden.iter().filter_map(Value::as_str).collect();
                    println!("hidden: {}", names.join(", "));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn print_menu_renders_items_separators_and_hidden() {
        print_menu(
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
    }

    #[test]
    fn print_menu_handles_empty_and_bare_shapes_without_panicking() {
        print_menu(&json!({"menu": {"id": "empty", "items": [], "hidden": []}}), Format::Human);
        // Accepts the bare menu object too (no `menu` wrapper), same
        // tolerance as `cmd::project::print_sections`.
        print_menu(&json!({"id": "bare", "items": [], "hidden": []}), Format::Human);
        print_menu(&json!({"menu": {"id": "x", "items": []}}), Format::Json);
    }
}
