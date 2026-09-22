//! `iyke project …` — the Project noun (WP-21b).
//!
//! Bridge routes consumed (all live on shell `main` since WP-21's bridge
//! part, ikenga#212):
//!   GET  /iyke/project/active       → `{ project }`
//!   GET  /iyke/project/list         → `{ projects: [Project, …] }`
//!   POST /iyke/project/set-active   `{ id }` → `{ ok, id }` — also emits
//!                                   `projects:active-changed`, which is what
//!                                   re-keys every project-scoped FE surface.
//!   GET  /iyke/explorer/sections    → pending WP-28; `sections` reports the
//!                                   gap via `missing_route` until it lands.

use anyhow::{anyhow, Result};
use clap::Subcommand;
use serde_json::{json, Value};

use crate::api::Client;
use crate::cmd::missing_route;
use crate::output::{print_write_result, Format};

#[derive(Subcommand)]
pub enum ProjectAction {
    /// Print the shell's active project.
    Show,

    /// Switch the active project. Accepts a project root path (`switch
    /// <path>`) or, as a convenience, a bare project id (`switch default`).
    Switch {
        /// Root path of the project to activate (or its id).
        path: String,
    },

    /// List the Explorer sidebar's registered sections for the active
    /// project (G-STATE `ExplorerSectionState[]` — id, source, order,
    /// collapsed). Requires the pending `GET /iyke/explorer/sections`
    /// bridge route (WP-28).
    Sections,
}

pub fn run(client: &Client, action: ProjectAction, fmt: Format) -> Result<()> {
    match action {
        ProjectAction::Show => {
            let v = client.get_with_query("/iyke/project/active", &[])?;
            print_project(&v, fmt);
        }
        ProjectAction::Switch { path } => {
            let id = resolve_project_id(client, &path)?;
            let v = client.post("/iyke/project/set-active", json!({ "id": id }))?;
            print_write_result(&format!("project switch {id}"), &v, fmt);
        }
        ProjectAction::Sections => {
            let v = client
                .get_with_query("/iyke/explorer/sections", &[])
                .map_err(|e| {
                    missing_route(
                        "/iyke/explorer/sections",
                        "the Explorer section registry",
                        e,
                    )
                })?;
            print_sections(&v, fmt);
        }
    }
    Ok(())
}

/// `switch <path>` resolves its argument against `GET /iyke/project/list`:
/// an exact `id` match wins (so `switch default` works), then a `root_path`
/// match after separator/trailing-slash normalization, then a
/// case-insensitive pass for Windows paths. Anything else is an error that
/// lists the candidates the caller could have meant.
fn resolve_project_id(client: &Client, target: &str) -> Result<String> {
    let v = client.get_with_query("/iyke/project/list", &[])?;
    let projects = v
        .get("projects")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| anyhow!("unexpected /iyke/project/list response: {v}"))?;
    resolve_in(&projects, target)
}

fn id_of(p: &Value) -> Option<&str> {
    p.get("id").and_then(Value::as_str)
}

fn resolve_in(projects: &[Value], target: &str) -> Result<String> {

    if projects.iter().any(|p| id_of(p) == Some(target)) {
        return Ok(target.to_string());
    }

    let want = normalize_path(target);
    let by_path: Vec<&str> = projects
        .iter()
        .filter(|p| {
            p.get("root_path")
                .and_then(Value::as_str)
                .map(|r| normalize_path(r) == want)
                .unwrap_or(false)
        })
        .filter_map(|p| id_of(p))
        .collect();
    match by_path.len() {
        1 => return Ok(by_path[0].to_string()),
        n if n > 1 => {
            return Err(anyhow!(
                "ambiguous project path {target:?} — matches {} projects; switch by id instead",
                by_path.len()
            ))
        }
        _ => {}
    }

    // Case-insensitive pass — Windows callers can't be expected to reproduce
    // the stored path's casing byte-for-byte.
    let want_lower = want.to_lowercase();
    let ci: Vec<&str> = projects
        .iter()
        .filter(|p| {
            p.get("root_path")
                .and_then(Value::as_str)
                .map(|r| normalize_path(r).to_lowercase() == want_lower)
                .unwrap_or(false)
        })
        .filter_map(|p| id_of(p))
        .collect();
    match ci.len() {
        1 => Ok(ci[0].to_string()),
        n if n > 1 => Err(anyhow!(
            "ambiguous project path {target:?} — matches {} projects; switch by id instead",
            ci.len()
        )),
        _ => Err(no_match(projects, target)),
    }
}

fn normalize_path(p: &str) -> String {
    let s = p.trim().replace('\\', "/");
    s.trim_end_matches('/').to_string()
}

fn no_match(projects: &[Value], target: &str) -> anyhow::Error {
    let candidates = projects
        .iter()
        .map(|p| {
            let id = p.get("id").and_then(Value::as_str).unwrap_or("?");
            let root = p.get("root_path").and_then(Value::as_str).unwrap_or("-");
            format!("  {id}  {root}")
        })
        .collect::<Vec<_>>()
        .join("\n");
    anyhow!("no project with id or root_path matching {target:?}. Known projects:\n{candidates}")
}

fn print_project(v: &Value, fmt: Format) {
    match fmt {
        Format::Json => println!("{v}"),
        Format::Human => {
            let p = v.get("project").unwrap_or(v);
            let field = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or("-");
            println!("id:      {}", field("id"));
            println!("name:    {}", field("display_name"));
            println!("root:    {}", field("root_path"));
            if p.get("is_default").and_then(Value::as_bool) == Some(true) {
                println!("default: yes");
            }
            if let Some(desc) = p.get("description").and_then(Value::as_str) {
                if !desc.is_empty() {
                    println!("desc:    {desc}");
                }
            }
        }
    }
}

/// Renders the G-STATE `ExplorerSectionState[]` — `{ id, source, order,
/// collapsed }`. Accepts either `{ "sections": [...] }` or a bare array so
/// the command doesn't pin a wrapper shape the WP-28 route hasn't frozen yet.
fn print_sections(v: &Value, fmt: Format) {
    match fmt {
        Format::Json => println!("{v}"),
        Format::Human => {
            let sections = v
                .get("sections")
                .and_then(Value::as_array)
                .or_else(|| v.as_array())
                .cloned()
                .unwrap_or_default();
            if sections.is_empty() {
                println!("(no explorer sections)");
                return;
            }
            println!("{:<24} {:<20} {:>5}  {}", "ID", "SOURCE", "ORDER", "STATE");
            for s in sections {
                let id = s.get("id").and_then(Value::as_str).unwrap_or("?");
                let source = s.get("source").and_then(Value::as_str).unwrap_or("shell");
                let order = s
                    .get("order")
                    .and_then(Value::as_i64)
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "-".into());
                let state = match s.get("collapsed").and_then(Value::as_bool) {
                    Some(true) => "collapsed",
                    Some(false) => "open",
                    None => "-",
                };
                println!("{id:<24} {source:<20} {order:>5}  {state}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn projects_fixture() -> Vec<Value> {
        serde_json::from_value::<Vec<Value>>(json!([
            {
                "id": "default",
                "display_name": "Default",
                "root_path": null,
                "position": 0,
                "is_default": true,
                "created_at": 1,
                "archived_at": null
            },
            {
                "id": "proj-royalti",
                "display_name": "Royalti",
                "root_path": "C:\\Users\\ned\\royalti-co",
                "position": 1,
                "is_default": false,
                "created_at": 2,
                "archived_at": null
            },
            {
                "id": "proj-ikenga",
                "display_name": "Ikenga",
                "root_path": "C:/Users/ned/ikenga",
                "position": 2,
                "is_default": false,
                "created_at": 3,
                "archived_at": null
            }
        ]))
        .unwrap()
    }

    #[test]
    fn resolve_by_exact_id() {
        let projects = projects_fixture();
        assert_eq!(resolve_in(&projects, "proj-royalti").unwrap(), "proj-royalti");
        // `default` has no root_path — id match is the only way to reach it.
        assert_eq!(resolve_in(&projects, "default").unwrap(), "default");
    }

    #[test]
    fn resolve_by_root_path_normalizes_separators_and_trailing_slash() {
        let projects = projects_fixture();
        // Forward-slash arg matching a backslash-stored root.
        assert_eq!(
            resolve_in(&projects, "C:/Users/ned/royalti-co").unwrap(),
            "proj-royalti"
        );
        // Trailing slash tolerated.
        assert_eq!(
            resolve_in(&projects, "C:\\Users\\ned\\ikenga\\").unwrap(),
            "proj-ikenga"
        );
    }

    #[test]
    fn resolve_by_root_path_case_insensitive_last_chance() {
        let projects = projects_fixture();
        assert_eq!(
            resolve_in(&projects, "c:/users/NED/royalti-co").unwrap(),
            "proj-royalti"
        );
    }

    #[test]
    fn resolve_unknown_target_lists_candidates() {
        let projects = projects_fixture();
        let err = resolve_in(&projects, "C:/nowhere").unwrap_err().to_string();
        assert!(err.contains("no project"), "{err}");
        assert!(err.contains("proj-royalti"), "{err}");
    }

    #[test]
    fn resolve_ambiguous_path_errors() {
        let projects: Vec<Value> = serde_json::from_value(json!([
            { "id": "a", "root_path": "/x" },
            { "id": "b", "root_path": "/x/" }
        ]))
        .unwrap();
        let err = resolve_in(&projects, "/x").unwrap_err().to_string();
        assert!(err.contains("ambiguous"), "{err}");
    }

    #[test]
    fn sections_accepts_wrapped_and_bare_arrays() {
        // G-STATE ExplorerSectionState shape: { id, source, order, collapsed }.
        let wrapped = json!({ "sections": [
            { "id": "files", "source": "shell", "order": 0, "collapsed": false }
        ]});
        print_sections(&wrapped, Format::Human);
        print_sections(&wrapped, Format::Json);
        let bare = json!([
            { "id": "files", "source": "shell", "order": 0, "collapsed": false }
        ]);
        print_sections(&bare, Format::Human);
    }
}
