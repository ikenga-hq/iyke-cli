//! `iyke project …` — the Project noun (WP-21b).
//!
//! Bridge routes consumed (all live on shell `main` since WP-21's bridge
//! part, ikenga#212):
//!   GET  /iyke/project/active       → `{ project }`
//!   GET  /iyke/project/list         → `{ projects: [Project, …] }`
//!                                   (`?include_archived=true` for `list --all`)
//!   POST /iyke/project/create       `{ id, display_name, root_path, … }` →
//!                                   Project. The shell announces it with
//!                                   `projects:list-changed`, so an open shell
//!                                   shows the new project without a reload.
//!   POST /iyke/project/set-active   `{ id }` → `{ ok, id }` — also emits
//!                                   `projects:active-changed`, which is what
//!                                   re-keys every project-scoped FE surface.
//!   GET  /iyke/explorer/sections    → pending WP-28; `sections` reports the
//!                                   gap via `missing_route` until it lands.

use anyhow::{anyhow, Result};
use clap::Subcommand;
use serde_json::{json, Value};

use crate::api::{Client, HttpStatusError};
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

    /// List registered projects: id, name, root.
    List {
        /// Include archived projects.
        #[arg(long)]
        all: bool,
    },

    /// Register a directory as a project. Idempotent by root path: running
    /// it again for a root that is already registered prints the existing
    /// project instead of failing. Needs the shell's Settings capability
    /// (the `full` tier). Never write ikenga.db directly.
    Create {
        /// Project root directory (must exist).
        path: String,
        /// Project id (slug: a-z, 0-9, `-`, `_`). Defaults to a slug of the
        /// directory name.
        #[arg(long)]
        id: Option<String>,
        /// Display name. Defaults to the directory name.
        #[arg(long)]
        name: Option<String>,
        /// Accent colour, e.g. `#4f8cff`.
        #[arg(long)]
        color: Option<String>,
        #[arg(long)]
        description: Option<String>,
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
        ProjectAction::List { all } => {
            let params: Vec<(&str, String)> = if all {
                vec![("include_archived", "true".to_string())]
            } else {
                Vec::new()
            };
            let v = client.get_with_query("/iyke/project/list", &params)?;
            print_project_list(&v, fmt);
        }
        ProjectAction::Create {
            path,
            id,
            name,
            color,
            description,
        } => {
            let root = absolutize(&path)?;
            let (project, existed) =
                create_idempotent(client, &root, id, name, color, description)?;
            print_created(&project, existed, fmt);
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

/// The directory as an absolute, existing path. `canonicalize` resolves
/// symlinks and `..`; on Windows it yields a `\\?\C:\…` verbatim path that the
/// shell's own project roots never use, so that prefix is dropped.
fn absolutize(path: &str) -> Result<String> {
    let canon = std::fs::canonicalize(path)
        .map_err(|e| anyhow!("project path {path:?} is not usable: {e}"))?;
    if !canon.is_dir() {
        return Err(anyhow!("project path {path:?} is not a directory"));
    }
    Ok(strip_verbatim(&canon.to_string_lossy()))
}

fn strip_verbatim(p: &str) -> String {
    match p.strip_prefix(r"\\?\") {
        Some(rest) if rest.chars().nth(1) == Some(':') => rest.to_string(),
        _ => p.to_string(),
    }
}

/// Last path component, whichever separator the root uses.
fn dir_name(root: &str) -> Option<String> {
    normalize_path(root)
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// `^[a-z0-9][a-z0-9_-]{0,63}$` from free text (the shell's slug rule):
/// lowercase, anything else becomes `-`, runs collapse, leading separators
/// and trailing `-` are dropped, capped at 64. `None` if nothing is left.
fn slugify(name: &str) -> Option<String> {
    let mut out = String::new();
    for c in name.chars().flat_map(char::to_lowercase) {
        let c = if c.is_ascii_alphanumeric() || c == '_' { c } else { '-' };
        if c == '-' && (out.is_empty() || out.ends_with('-')) {
            continue;
        }
        if out.is_empty() && c == '_' {
            continue;
        }
        out.push(c);
    }
    out.truncate(64);
    let out = out.trim_end_matches('-').to_string();
    (!out.is_empty()).then_some(out)
}

fn find_by_root<'a>(projects: &'a [Value], root: &str) -> Option<&'a Value> {
    let want = normalize_path(root);
    projects.iter().find(|p| {
        p.get("root_path")
            .and_then(Value::as_str)
            .map(|r| normalize_path(r) == want)
            .unwrap_or(false)
    })
}

fn build_create_body(
    root: &str,
    id: Option<String>,
    name: Option<String>,
    color: Option<String>,
    description: Option<String>,
) -> Result<Value> {
    let dir = dir_name(root);
    let id = match id {
        Some(id) => id,
        None => dir.as_deref().and_then(slugify).ok_or_else(|| {
            anyhow!("cannot derive a project id from {root:?}; pass --id")
        })?,
    };
    let display_name = name
        .or(dir)
        .ok_or_else(|| anyhow!("cannot derive a project name from {root:?}; pass --name"))?;
    Ok(json!({
        "id": id,
        "display_name": display_name,
        "root_path": root,
        "color": color,
        "description": description,
    }))
}

fn list_of(v: &Value) -> Result<Vec<Value>> {
    v.get("projects")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| anyhow!("unexpected /iyke/project/list response: {v}"))
}

/// Register `root`, or return the project that already has it.
/// Returns `(project, already_existed)`.
fn create_idempotent(
    client: &Client,
    root: &str,
    id: Option<String>,
    name: Option<String>,
    color: Option<String>,
    description: Option<String>,
) -> Result<(Value, bool)> {
    let listed = list_of(&client.get_with_query("/iyke/project/list", &[])?)?;
    if let Some(p) = find_by_root(&listed, root) {
        return Ok((p.clone(), true));
    }
    let body = build_create_body(root, id, name, color, description)?;
    match client.post("/iyke/project/create", body) {
        Ok(project) => Ok((project, false)),
        Err(e) => {
            // Lost a race with another caller registering the same root
            // between our list and our create: that is still "already
            // registered", not a failure.
            if is_duplicate_root(&e) {
                let listed = list_of(&client.get_with_query("/iyke/project/list", &[])?)?;
                if let Some(p) = find_by_root(&listed, root) {
                    return Ok((p.clone(), true));
                }
            }
            Err(explain_create_error(e))
        }
    }
}

fn http_error(e: &anyhow::Error) -> Option<&HttpStatusError> {
    e.downcast_ref::<HttpStatusError>()
}

fn is_duplicate_root(e: &anyhow::Error) -> bool {
    http_error(e).is_some_and(|h| h.body.contains("project root already exists"))
}

/// Turn the bridge's raw `HTTP <code>: <body>` into something actionable.
fn explain_create_error(e: anyhow::Error) -> anyhow::Error {
    let Some(h) = http_error(&e) else { return e };
    let body = h.body.trim();
    if h.status == 403 || body.to_lowercase().contains("forbidden") {
        anyhow!(
            "the shell refused to create a project for this credential ({body}). \
             Creating a project needs the Settings capability, which only the \
             `full` tier carries; ask the owner to register it from the shell"
        )
    } else if body.contains("project id already exists") {
        anyhow!("{body}. A different project already uses that id; pass --id to choose another")
    } else {
        e
    }
}

fn print_project_list(v: &Value, fmt: Format) {
    match fmt {
        Format::Json => println!("{v}"),
        Format::Human => {
            let projects = v
                .get("projects")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if projects.is_empty() {
                println!("(no projects)");
                return;
            }
            println!("{:<28} {:<28} {}", "ID", "NAME", "ROOT");
            for p in projects {
                let field = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or("-");
                let archived = if p.get("archived_at").map_or(false, |a| !a.is_null()) {
                    "  [archived]"
                } else {
                    ""
                };
                println!(
                    "{:<28} {:<28} {}{archived}",
                    field("id"),
                    field("display_name"),
                    field("root_path")
                );
            }
        }
    }
}

fn print_created(project: &Value, existed: bool, fmt: Format) {
    let wrapped = json!({ "created": !existed, "project": project });
    match fmt {
        Format::Json => println!("{wrapped}"),
        Format::Human => {
            println!(
                "{}",
                if existed {
                    "already registered"
                } else {
                    "created"
                }
            );
            print_project(&wrapped, fmt);
        }
    }
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

    // ── create / list ───────────────────────────────────────────────────────

    #[test]
    fn slugify_follows_the_shell_slug_rule() {
        assert_eq!(slugify("My Repo").as_deref(), Some("my-repo"));
        assert_eq!(slugify("royalti-co").as_deref(), Some("royalti-co"));
        assert_eq!(slugify("  --Foo__Bar!! ").as_deref(), Some("foo__bar"));
        assert_eq!(slugify("_x").as_deref(), Some("x"));
        assert_eq!(slugify("2026.album").as_deref(), Some("2026-album"));
        assert_eq!(slugify("日本語"), None);
        assert_eq!(slugify("---"), None);
        assert_eq!(slugify(&"a".repeat(100)).unwrap().len(), 64);
    }

    #[test]
    fn dir_name_takes_the_last_component_of_either_separator() {
        assert_eq!(dir_name("/home/x/My Repo/").as_deref(), Some("My Repo"));
        assert_eq!(dir_name("C:\\Users\\ned\\ikenga").as_deref(), Some("ikenga"));
        assert_eq!(dir_name("/"), None);
    }

    #[test]
    fn strip_verbatim_only_drops_the_windows_drive_prefix() {
        assert_eq!(strip_verbatim(r"\\?\C:\Users\ned"), r"C:\Users\ned");
        assert_eq!(strip_verbatim(r"\\?\UNC\srv\share"), r"\\?\UNC\srv\share");
        assert_eq!(strip_verbatim("/home/x"), "/home/x");
    }

    #[test]
    fn create_body_defaults_id_and_name_from_the_directory() {
        let b = build_create_body("/home/x/My Repo", None, None, None, None).unwrap();
        assert_eq!(b["id"], "my-repo");
        assert_eq!(b["display_name"], "My Repo");
        assert_eq!(b["root_path"], "/home/x/My Repo");
        let b = build_create_body(
            "/home/x/r",
            Some("custom".into()),
            Some("Nice".into()),
            Some("#fff".into()),
            Some("d".into()),
        )
        .unwrap();
        assert_eq!(b["id"], "custom");
        assert_eq!(b["display_name"], "Nice");
        assert_eq!(b["color"], "#fff");
        assert_eq!(b["description"], "d");
        let e = build_create_body("/home/x/日本語", None, None, None, None)
            .unwrap_err()
            .to_string();
        assert!(e.contains("pass --id"), "{e}");
    }

    #[test]
    fn find_by_root_normalizes_like_switch() {
        let ps = projects_fixture();
        assert_eq!(
            find_by_root(&ps, "C:/Users/ned/royalti-co/").unwrap()["id"],
            "proj-royalti"
        );
        assert!(find_by_root(&ps, "C:/Users/ned/other").is_none());
    }

    fn http_err(status: u16, body: &str) -> anyhow::Error {
        anyhow::Error::new(HttpStatusError {
            path: "/iyke/project/create".into(),
            status,
            body: body.into(),
        })
    }

    #[test]
    fn forbidden_names_the_missing_capability_not_a_raw_dump() {
        let e = explain_create_error(http_err(403, "forbidden: missing=settings")).to_string();
        assert!(e.contains("Settings capability"), "{e}");
        assert!(e.contains("missing=settings"), "{e}");
        assert!(!e.contains("returned HTTP"), "{e}");

        let e = explain_create_error(http_err(400, "project id already exists: repo")).to_string();
        assert!(e.contains("pass --id"), "{e}");

        // Anything else passes through untouched.
        let e = explain_create_error(http_err(500, "boom")).to_string();
        assert!(e.contains("returned HTTP 500: boom"), "{e}");
    }

    #[test]
    fn absolutize_requires_an_existing_directory() {
        let dir = std::env::temp_dir().join(format!("iyke-proj-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let got = absolutize(dir.to_str().unwrap()).unwrap();
        assert!(std::path::Path::new(&got).is_dir());
        assert!(absolutize(dir.join("missing").to_str().unwrap()).is_err());
        let file = dir.join("f.txt");
        std::fs::write(&file, "x").unwrap();
        assert!(absolutize(file.to_str().unwrap())
            .unwrap_err()
            .to_string()
            .contains("not a directory"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A one-thread HTTP stub standing in for the bridge: answers each request
    /// with the next canned `(status, body)` and records `"METHOD path"`.
    struct Stub {
        client: Client,
        seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    fn stub(replies: Vec<(u16, String)>) -> Stub {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for (status, body) in replies {
                let Ok((mut conn, _)) = listener.accept() else { return };
                let mut reader = BufReader::new(conn.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let mut parts = line.split_whitespace();
                let (method, path) = (parts.next().unwrap(), parts.next().unwrap());
                log.lock().unwrap().push(format!("{method} {path}"));
                let mut len = 0usize;
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).unwrap();
                    if h.trim().is_empty() {
                        break;
                    }
                    if let Some(v) = h.to_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut buf = vec![0u8; len];
                reader.read_exact(&mut buf).unwrap();
                let reason = if status == 200 { "OK" } else { "Error" };
                write!(
                    conn,
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        let cf = crate::control::ControlFile {
            schema_version: 1,
            port,
            token: "t".into(),
            pid: 0,
            started_at_unix_ms: 0,
            identifier: "app.ikenga".into(),
        };
        Stub {
            client: Client::new(&cf),
            seen,
        }
    }

    fn proj(id: &str, root: &str) -> Value {
        json!({ "id": id, "display_name": id, "root_path": root, "archived_at": null })
    }

    #[test]
    fn create_registers_once_and_a_second_run_returns_the_same_project() {
        let created = proj("repo", "/home/x/repo");
        let s = stub(vec![
            (200, json!({ "projects": [proj("default", "")] }).to_string()),
            (200, created.to_string()),
            // second run: the list now has it, so no POST follows
            (
                200,
                json!({ "projects": [proj("default", ""), created.clone()] }).to_string(),
            ),
        ]);

        let (p1, existed1) =
            create_idempotent(&s.client, "/home/x/repo", None, None, None, None).unwrap();
        assert!(!existed1);
        assert_eq!(p1["id"], "repo");

        let (p2, existed2) =
            create_idempotent(&s.client, "/home/x/repo/", None, None, None, None).unwrap();
        assert!(existed2);
        assert_eq!(p2["id"], p1["id"]);

        assert_eq!(
            *s.seen.lock().unwrap(),
            vec![
                "GET /iyke/project/list",
                "POST /iyke/project/create",
                "GET /iyke/project/list"
            ]
        );
    }

    #[test]
    fn losing_a_create_race_returns_the_winner_not_an_error() {
        let winner = proj("repo", "/home/x/repo");
        let s = stub(vec![
            (200, json!({ "projects": [] }).to_string()),
            (400, "project root already exists: /home/x/repo (project repo)".into()),
            (200, json!({ "projects": [winner.clone()] }).to_string()),
        ]);
        let (p, existed) =
            create_idempotent(&s.client, "/home/x/repo", None, None, None, None).unwrap();
        assert!(existed);
        assert_eq!(p["id"], "repo");
    }

    #[test]
    fn a_forbidden_create_surfaces_the_explained_error() {
        let s = stub(vec![
            (200, json!({ "projects": [] }).to_string()),
            (403, "forbidden: missing=settings".into()),
        ]);
        let e = create_idempotent(&s.client, "/home/x/repo", None, None, None, None)
            .unwrap_err()
            .to_string();
        assert!(e.contains("Settings capability") && e.contains("missing=settings"), "{e}");
    }

    #[test]
    fn project_list_renders_archived_and_empty() {
        let v = json!({ "projects": [
            proj("a", "/x"),
            { "id": "b", "display_name": "B", "root_path": null, "archived_at": 5 }
        ]});
        print_project_list(&v, Format::Human);
        print_project_list(&v, Format::Json);
        print_project_list(&json!({ "projects": [] }), Format::Human);
        print_created(&proj("a", "/x"), false, Format::Human);
        print_created(&proj("a", "/x"), true, Format::Json);
    }
}
