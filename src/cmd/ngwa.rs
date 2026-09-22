//! `iyke ngwa …` — the Ngwa noun (WP-21b): the unified equipment catalogue
//! (installed pkgs + Ọba-placed primitives + engine config/assets) that the
//! `/ngwa/*` shell surfaces render.
//!
//! All five subcommands read one payload — `GET /iyke/ngwa/snapshot`, the
//! bridge twin of WP-14's `ngwa_snapshot` Tauri command
//! (`shell/src-tauri/src/commands/ngwa.rs`). Each subcommand prints the same
//! `NgwaSnapshot` JSON the shell surface consumes (`{ items, as_of_ms,
//! sources }`); the human format renders the facet that surface shows.
//!
//! The route itself is pending WP-28 — `ngwa_snapshot` is Tauri-only on the
//! shell builds this CLI can reach today. Until it lands every subcommand
//! fails with a `route missing` error via `missing_route` rather than
//! printing a different shape than the shell renders.

use std::time::Duration;

use anyhow::{anyhow, Result};
use clap::Subcommand;
use serde_json::Value;

use crate::api::Client;
use crate::cmd::missing_route;
use crate::output::Format;

const SNAPSHOT_PATH: &str = "/iyke/ngwa/snapshot";
/// First snapshot call on a post-0065 database does a cold transcript scan
/// (~100 s per the WP-14 rollout notes); subsequent calls are cheap. Give
/// the route headroom rather than timing out mid-scan.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(130);

#[derive(Subcommand)]
pub enum NgwaAction {
    /// Installed equipment — every NgwaItem the kernel + Ọba report, as the
    /// `/ngwa/installed` surface renders them. JSON prints the verbatim
    /// snapshot; the human table lists each item's kind, state, and scope.
    Installed,

    /// The `/ngwa/store` catalogue facet — the snapshot the store renders.
    /// Remote-registry rows (`state: available`/`update`) are enriched
    /// frontend-side and are not part of the bridged snapshot.
    Store,

    /// Scope matrix — the snapshot the `/ngwa/scopes` surface renders;
    /// the human view groups items by placement scope.
    Scopes,

    /// Health — the snapshot the `/ngwa/health` surface renders; the human
    /// view leads with the `sources` health rollup plus broken/orphaned
    /// items.
    Health,

    /// One item's full detail — the `/ngwa/item/<id>` surface.
    Item {
        /// NgwaItem id — e.g. `com.ikenga.iyke`, `skill:personal:groundwork`.
        id: String,
    },
}

pub fn run(client: &Client, action: NgwaAction, fmt: Format) -> Result<()> {
    match action {
        NgwaAction::Installed => {
            let snap = snapshot(client)?;
            print_items_table(&snap, fmt, &["ID", "KIND", "STATE", "SCOPE", "NAME"]);
        }
        NgwaAction::Store => {
            let snap = snapshot(client)?;
            print_items_table(&snap, fmt, &["ID", "KIND", "SOURCE", "STATE", "NAME"]);
        }
        NgwaAction::Scopes => {
            let snap = snapshot(client)?;
            print_scopes(&snap, fmt);
        }
        NgwaAction::Health => {
            let snap = snapshot(client)?;
            print_health(&snap, fmt);
        }
        NgwaAction::Item { id } => {
            let snap = snapshot(client)?;
            let item = find_item(&snap, &id)?;
            print_item(&item, fmt);
        }
    }
    Ok(())
}

fn snapshot(client: &Client) -> Result<Value> {
    client
        .get_with_query_timeout(SNAPSHOT_PATH, &[], SNAPSHOT_TIMEOUT)
        .map_err(|e| {
            missing_route(
                SNAPSHOT_PATH,
                "the HTTP twin of WP-14's `ngwa_snapshot` Tauri command",
                e,
            )
        })
}

fn items(snap: &Value) -> Vec<&Value> {
    snap.get("items")
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
        .unwrap_or_default()
}

fn find_item(snap: &Value, id: &str) -> Result<Value> {
    let all = items(snap);
    all.iter()
        .find(|i| i.get("id").and_then(Value::as_str) == Some(id))
        .map(|i| (*i).clone())
        .ok_or_else(|| {
            anyhow!(
                "no ngwa item {id:?} in the snapshot ({} items — `iyke ngwa installed` lists ids)",
                all.len()
            )
        })
}

/// Stable facet key for a `NgwaScope`: `personal` or `project:<id>` — same
/// key the store groups by.
pub fn scope_key(item: &Value) -> String {
    match item.get("scope") {
        Some(s) if s.get("kind").and_then(Value::as_str) == Some("project") => {
            format!(
                "project:{}",
                s.get("project_id").and_then(Value::as_str).unwrap_or("?")
            )
        }
        _ => "personal".to_string(),
    }
}

fn str_field<'v>(v: &'v Value, key: &str) -> &'v str {
    v.get(key).and_then(Value::as_str).unwrap_or("-")
}

fn print_items_table(snap: &Value, fmt: Format, cols: &[&str; 5]) {
    match fmt {
        Format::Json => println!("{snap}"),
        Format::Human => {
            let all = items(snap);
            if all.is_empty() {
                println!("(no ngwa items)");
                return;
            }
            println!(
                "{:<44} {:<10} {:<10} {:<22} {}",
                cols[0], cols[1], cols[2], cols[3], cols[4]
            );
            for i in all {
                let (c3, c4) = if cols[2] == "SOURCE" {
                    (
                        str_field(i.get("origin").unwrap_or(&Value::Null), "source").to_string(),
                        str_field(i, "state").to_string(),
                    )
                } else {
                    (str_field(i, "state").to_string(), scope_key(i))
                };
                let name = i
                    .get("display_name")
                    .and_then(Value::as_str)
                    .or_else(|| i.get("name").and_then(Value::as_str))
                    .unwrap_or("-");
                println!(
                    "{:<44} {:<10} {:<10} {:<22} {}",
                    str_field(i, "id"),
                    str_field(i, "kind"),
                    c3,
                    c4,
                    name
                );
            }
        }
    }
}

fn print_scopes(snap: &Value, fmt: Format) {
    match fmt {
        Format::Json => println!("{snap}"),
        Format::Human => {
            let mut groups: std::collections::BTreeMap<String, Vec<&Value>> =
                Default::default();
            for i in items(snap) {
                groups.entry(scope_key(i)).or_default().push(i);
            }
            for (scope, list) in groups {
                println!("{scope} ({} items)", list.len());
                for i in list {
                    println!(
                        "  {:<44} {:<10} {}",
                        str_field(i, "id"),
                        str_field(i, "kind"),
                        str_field(i, "state")
                    );
                }
            }
        }
    }
}

fn print_health(snap: &Value, fmt: Format) {
    match fmt {
        Format::Json => println!("{snap}"),
        Format::Human => {
            println!("{:<16} {:<5} {:>5}  {}", "SOURCE", "OK", "COUNT", "ERROR");
            if let Some(sources) = snap.get("sources").and_then(Value::as_object) {
                for (name, row) in sources {
                    let ok = row.get("ok").and_then(Value::as_bool).unwrap_or(false);
                    let count = row
                        .get("count")
                        .and_then(Value::as_i64)
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| "-".into());
                    let err = row.get("error").and_then(Value::as_str).unwrap_or("");
                    println!("{name:<16} {ok:<5} {count:>5}  {err}");
                }
            }
            let problems: Vec<&Value> = items(snap)
                .into_iter()
                .filter(|i| {
                    matches!(
                        i.get("state").and_then(Value::as_str),
                        Some("broken") | Some("orphaned")
                    )
                })
                .collect();
            if !problems.is_empty() {
                println!("\nitems needing attention:");
                for i in problems {
                    println!("  {:<44} {}", str_field(i, "id"), str_field(i, "state"));
                }
            }
        }
    }
}

fn print_item(item: &Value, fmt: Format) {
    match fmt {
        Format::Json => println!("{item}"),
        Format::Human => {
            println!("id:          {}", str_field(item, "id"));
            println!("kind:        {}", str_field(item, "kind"));
            println!("name:        {}", str_field(item, "name"));
            if let Some(d) = item.get("display_name").and_then(Value::as_str) {
                println!("display:     {d}");
            }
            if let Some(d) = item.get("description").and_then(Value::as_str) {
                println!("desc:        {d}");
            }
            println!("state:       {}", str_field(item, "state"));
            println!("scope:       {}", scope_key(item));
            if let Some(v) = item.get("version").and_then(Value::as_str) {
                println!("version:     {v}");
            }
            if let Some(v) = item.get("latest_version").and_then(Value::as_str) {
                println!("latest:      {v}");
            }
            if let Some(rt) = item.get("runtime") {
                println!("runtime:     {rt}");
            }
            if let Some(t) = item.get("trust") {
                println!("trust:       {t}");
            }
            if let Some(p) = item.get("placements").and_then(Value::as_array) {
                if !p.is_empty() {
                    println!("placements:  {}", serde_json::to_string(p).unwrap_or_default());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The shell's committed NgwaSnapshot golden (WP-14/WP-17 shape-locked):
    /// `shell/src/lib/ngwa/__fixtures__/ngwa-snapshot.golden.json`, copied
    /// verbatim so the CLI's contract test diff-fails if the wire shape
    /// drifts.
    const GOLDEN: &str = include_str!("../../tests/fixtures/ngwa-snapshot.golden.json");

    fn golden() -> Value {
        serde_json::from_str(GOLDEN).unwrap()
    }

    #[test]
    fn golden_snapshot_is_the_ngwa_wire_shape() {
        let snap = golden();
        assert!(snap.get("as_of_ms").and_then(Value::as_i64).is_some());
        assert!(snap.get("sources").and_then(Value::as_object).is_some());
        let items = snap.get("items").and_then(Value::as_array).unwrap();
        assert_eq!(items.len(), 13);
        for i in items {
            for k in ["id", "kind", "state", "scope", "origin", "name"] {
                assert!(i.get(k).is_some(), "item missing {k}: {i}");
            }
            // NgwaScope is an externally-tagged enum.
            assert!(i["scope"].get("kind").is_some(), "scope.kind: {i}");
        }
        // Source health rows: { ok, error, count }.
        for (name, row) in snap["sources"].as_object().unwrap() {
            assert!(row.get("ok").is_some(), "sources.{name}.ok");
            assert!(row.get("count").is_some(), "sources.{name}.count");
        }
    }

    #[test]
    fn find_item_extracts_known_item() {
        let snap = golden();
        let item = find_item(&snap, "agent:personal:reviewer").unwrap();
        assert_eq!(item["kind"], "agent");
        assert_eq!(item["scope"]["kind"], "personal");
    }

    #[test]
    fn find_item_unknown_id_errors() {
        let snap = golden();
        let err = find_item(&snap, "nope:missing").unwrap_err().to_string();
        assert!(err.contains("no ngwa item"), "{err}");
        assert!(err.contains("13 items"), "{err}");
    }

    #[test]
    fn scope_key_matches_ngwa_scope_wire() {
        let snap = golden();
        let all = items(&snap);
        let project_scoped: Vec<&&Value> = all
            .iter()
            .filter(|i| i["scope"]["kind"] == "project")
            .collect();
        assert!(!project_scoped.is_empty());
        for i in project_scoped {
            let key = scope_key(i);
            assert!(key.starts_with("project:"), "{key}");
        }
        let personal: Vec<&&Value> = all
            .iter()
            .filter(|i| i["scope"]["kind"] == "personal")
            .collect();
        assert!(!personal.is_empty());
        for i in personal {
            assert_eq!(scope_key(i), "personal");
        }
    }

    #[test]
    fn renderers_accept_the_golden_shape() {
        let snap = golden();
        for cols in [
            ["ID", "KIND", "STATE", "SCOPE", "NAME"],
            ["ID", "KIND", "SOURCE", "STATE", "NAME"],
        ] {
            print_items_table(&snap, Format::Human, &cols);
            print_items_table(&snap, Format::Json, &cols);
        }
        print_scopes(&snap, Format::Human);
        print_health(&snap, Format::Human);
        let item = find_item(&snap, "com.ikenga.iyke").unwrap();
        print_item(&item, Format::Human);
        print_item(&item, Format::Json);
        // A health view with no `sources` object must not panic.
        print_health(&json!({ "items": [] }), Format::Human);
    }
}
