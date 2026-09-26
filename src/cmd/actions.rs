//! `iyke actions …` — the Actions noun (WP-62): the D-06 Actions tab's
//! `iyke actions list|set|import` footer lines
//! (`plans/shell-ux-rearchitecture/designs/actions.html`, `#iykeCmd`).
//!
//! Bridge routes consumed (shell `feat/phase-6-actions`, WP-62):
//!   GET  /iyke/actions?scope=<personal|project|all>  → { schema_version, count, actions }
//!   POST /iyke/actions/set     { scope, action }      → { ok, ... } (422 on a validator refusal)
//!   POST /iyke/actions/import  { scope, actions }     → { ok, added, skipped, errors }
//!
//! Writes round-trip through the running shell into the frontend's
//! `saveUserAction` (G-ACTIONS-API) — the same call the D-06 Editor tab's
//! Save button makes — so a CLI-authored action goes through the identical
//! WP-50 validator the UI does. An invalid document comes back as a plain
//! HTTP error naming the validator's own `E_*` code.

use std::fmt::Write as _;
use std::fs;
use std::io::Read as _;
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use clap::{Subcommand, ValueEnum};
use serde_json::{json, Value};

use crate::api::Client;
use crate::output::{print_write_result, Format};

#[derive(Subcommand)]
pub enum ActionsAction {
    /// List the effective actions the shell currently merges (built-in,
    /// package, personal, project — G-ACTIONS §2.1).
    List {
        /// Filter by source: `personal`, `project`, `package`, `builtin`,
        /// or `all` (default). Matches D-06's `actions list --scope
        /// <scope>` footer line — `scope` here names the file being
        /// viewed, not a merge-layer selector.
        #[arg(long)]
        scope: Option<String>,
    },

    /// Upsert one user action by id (G-ACTIONS §1.2): `{id, name, icon?,
    /// description?, run, placements?, scope}` — the same `UserAction`
    /// shape the D-06 Editor tab writes.
    Set {
        /// The action id. Checked against the document's own `id` when both
        /// are given (they must agree); injected into the document when it
        /// has none.
        id: String,
        /// `personal` or `project`.
        #[arg(long)]
        scope: String,
        /// Path to a JSON file holding one UserAction document.
        #[arg(long, conflicts_with = "doc")]
        file: Option<PathBuf>,
        /// The UserAction document as an inline JSON string, or `-` to read
        /// it from stdin.
        #[arg(long, conflicts_with = "file")]
        doc: Option<String>,
    },

    /// Upsert a batch of user actions from a JSON file: either
    /// `{"actions": [...]}` (the same envelope `actions list` prints) or a
    /// bare `[...]` array of UserAction documents.
    ///
    /// An id already present in `--scope`'s own file is skipped and listed
    /// in the result unless `--overwrite` is given (D-06 import's add/skip
    /// behaviour, G-ACTIONS §1.6). Exits non-zero if the write is refused
    /// (`ok: false`) or any item errored.
    Import {
        /// Import source. Only `team` is implemented this phase; `pkg` and
        /// `vscode` land with the Import tab (WP-61).
        #[arg(long, value_enum)]
        from: ImportSource,
        #[arg(long)]
        file: PathBuf,
        /// `personal` or `project`. Defaults to `project` — a teammate's
        /// file always lands as project-scope actions (G-ACTIONS §8.3).
        #[arg(long)]
        scope: Option<String>,
        /// Overwrite an id already present in the scope's file instead of
        /// skipping it.
        #[arg(long)]
        overwrite: bool,
    },
}

#[derive(Copy, Clone, ValueEnum)]
pub enum ImportSource {
    Pkg,
    Vscode,
    Team,
}

impl std::fmt::Display for ImportSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Pkg => "pkg",
            Self::Vscode => "vscode",
            Self::Team => "team",
        })
    }
}

pub fn run(client: &Client, action: ActionsAction, fmt: Format) -> Result<()> {
    match action {
        ActionsAction::List { scope } => {
            let params: Vec<(&str, String)> = scope.map(|s| vec![("scope", s)]).unwrap_or_default();
            let v = client.get_with_query("/iyke/actions", &params)?;
            print_actions(&v, fmt);
        }
        ActionsAction::Set {
            id,
            scope,
            file,
            doc,
        } => {
            let mut action = read_one_action(file, doc)?;
            apply_action_id(&mut action, &id)?;
            let label = format!("actions set {}", action_id(&action));
            let v = client.post("/iyke/actions/set", json!({ "scope": scope, "action": action }))?;
            print_write_result(&label, &v, fmt);
        }
        ActionsAction::Import {
            from,
            file,
            scope,
            overwrite,
        } => {
            match from {
                ImportSource::Pkg | ImportSource::Vscode => {
                    return Err(anyhow!(
                        "actions import --from {from}: not yet supported — lands with the Import \
                         tab (WP-61)"
                    ));
                }
                ImportSource::Team => {}
            }
            let actions = read_action_batch(&file)?;
            let scope = scope.unwrap_or_else(|| "project".to_string());
            let v = client.post(
                "/iyke/actions/import",
                json!({ "scope": scope, "actions": actions, "overwrite": overwrite }),
            )?;
            print_import_result(&v, fmt);
            // WP-62 review (C3): follow the v0.3.1 `terminal-send` convention
            // (main.rs) — a write that came back `ok: false`, or came back
            // `ok: true` with per-item errors, is a hard failure for the
            // caller, not a silent success with a printed complaint.
            let ok = v.get("ok").and_then(Value::as_bool).unwrap_or(false);
            let has_errors = v
                .get("errors")
                .and_then(Value::as_array)
                .map(|e| !e.is_empty())
                .unwrap_or(false);
            if !ok || has_errors {
                return Err(anyhow!("actions import: one or more items failed"));
            }
        }
    }
    Ok(())
}

fn action_id(action: &Value) -> &str {
    action.get("id").and_then(Value::as_str).unwrap_or("?")
}

/// The positional id (C1) either agrees with the document's own `id`, or
/// fills it in when the document has none — never silently overridden.
fn apply_action_id(action: &mut Value, id: &str) -> Result<()> {
    let obj = action
        .as_object_mut()
        .ok_or_else(|| anyhow!("the action document must be a JSON object"))?;
    match obj.get("id") {
        None => {
            obj.insert("id".to_string(), Value::String(id.to_string()));
        }
        Some(Value::String(existing)) if existing == id => {}
        Some(existing) => {
            return Err(anyhow!(
                "positional id {id:?} does not match the document's own id {existing}"
            ));
        }
    }
    Ok(())
}

fn read_one_action(file: Option<PathBuf>, doc: Option<String>) -> Result<Value> {
    let text = match (file, doc) {
        (Some(path), None) => {
            fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?
        }
        (None, Some(inline)) if inline == "-" => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("read the action document from stdin")?;
            buf
        }
        (None, Some(inline)) => inline,
        (None, None) => {
            return Err(anyhow!(
                "actions set needs one of --file <path> or --doc '<json>' (or --doc - for stdin)"
            ))
        }
        (Some(_), Some(_)) => unreachable!("clap enforces --file/--doc are mutually exclusive"),
    };
    parse_one_action(&text)
}

fn parse_one_action(text: &str) -> Result<Value> {
    serde_json::from_str(text).context("parse the action document as JSON")
}

/// Accepts `{"actions": [...]}` (the same envelope `GET /iyke/actions`
/// returns) or a bare `[...]` array — the same "wrapped or bare" tolerance
/// `cmd::project::print_sections` uses for `ExplorerSectionState[]`.
fn read_action_batch(path: &PathBuf) -> Result<Vec<Value>> {
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    parse_action_batch(&text)
}

fn parse_action_batch(text: &str) -> Result<Vec<Value>> {
    let v: Value = serde_json::from_str(text).context("parse the actions batch as JSON")?;
    if let Some(actions) = v.get("actions").and_then(Value::as_array) {
        return Ok(actions.clone());
    }
    if let Some(arr) = v.as_array() {
        return Ok(arr.clone());
    }
    Err(anyhow!(
        "the import file is neither {{\"actions\": [...]}} nor a bare [...] array"
    ))
}

fn print_actions(v: &Value, fmt: Format) {
    print!("{}", render_actions(v, fmt));
}

/// WP-62 review ("make the print tests assert on the output"): renders to a
/// `String` instead of printing directly. `print_actions` is the thin
/// runtime wrapper.
fn render_actions(v: &Value, fmt: Format) -> String {
    let mut out = String::new();
    match fmt {
        Format::Json => {
            let _ = writeln!(out, "{v}");
        }
        Format::Human => {
            let actions = v.get("actions").and_then(Value::as_array);
            let Some(actions) = actions else {
                let _ = writeln!(out, "(no actions)");
                return out;
            };
            if actions.is_empty() {
                let _ = writeln!(out, "(no actions)");
                return out;
            }
            let _ = writeln!(out, "{:<28} {:<10} {:<8} {}", "ID", "SOURCE", "RUN", "NAME");
            for a in actions {
                let id = a.get("id").and_then(Value::as_str).unwrap_or("?");
                let source = a.get("source").and_then(Value::as_str).unwrap_or("?");
                let run_kind = a.get("run_kind").and_then(Value::as_str).unwrap_or("-");
                let name = a.get("name").and_then(Value::as_str).unwrap_or("-");
                let _ = writeln!(out, "{id:<28} {source:<10} {run_kind:<8} {name}");
            }
        }
    }
    out
}

fn print_import_result(v: &Value, fmt: Format) {
    let (stdout, stderr) = render_import_result(v, fmt);
    print!("{stdout}");
    eprint!("{stderr}");
}

/// WP-62 review (S4/C3): `added` / `skipped` are now the ids themselves
/// (`{added[], skipped[], errors[]}`), not counts — so a caller can see
/// exactly which action id landed where, not just how many. Returns
/// `(stdout, stderr)` separately (errors go to stderr) so tests can assert
/// on each.
fn render_import_result(v: &Value, fmt: Format) -> (String, String) {
    let mut out = String::new();
    let mut err = String::new();
    match fmt {
        Format::Json => {
            let _ = writeln!(out, "{v}");
        }
        Format::Human => {
            let added = v.get("added").and_then(Value::as_array).cloned().unwrap_or_default();
            let skipped = v.get("skipped").and_then(Value::as_array).cloned().unwrap_or_default();
            let errors = v.get("errors").and_then(Value::as_array).cloned().unwrap_or_default();
            let _ = writeln!(out, "ok: added {}, skipped {}", added.len(), skipped.len());
            if !added.is_empty() {
                let ids: Vec<&str> = added.iter().filter_map(Value::as_str).collect();
                let _ = writeln!(out, "  added:   {}", ids.join(", "));
            }
            if !skipped.is_empty() {
                let ids: Vec<&str> = skipped.iter().filter_map(Value::as_str).collect();
                let _ = writeln!(out, "  skipped: {}", ids.join(", "));
            }
            for e in &errors {
                let id = e.get("id").and_then(Value::as_str).unwrap_or("?");
                let error = e.get("error").and_then(Value::as_str).unwrap_or("?");
                let _ = writeln!(err, "  {id}: {error}");
            }
        }
    }
    (out, err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_action_batch_accepts_wrapped_and_bare_arrays() {
        let wrapped =
            parse_action_batch(&json!({ "actions": [{"id": "a"}] }).to_string()).unwrap();
        assert_eq!(wrapped.len(), 1);
        assert_eq!(wrapped[0]["id"], "a");

        let bare = parse_action_batch(&json!([{"id": "b"}]).to_string()).unwrap();
        assert_eq!(bare.len(), 1);
        assert_eq!(bare[0]["id"], "b");
    }

    #[test]
    fn parse_action_batch_rejects_neither_shape() {
        let err = parse_action_batch(&json!({ "not_actions": 1 }).to_string())
            .unwrap_err()
            .to_string();
        assert!(err.contains("neither"), "{err}");
    }

    #[test]
    fn action_id_falls_back_when_missing() {
        assert_eq!(action_id(&json!({"name": "no id"})), "?");
        assert_eq!(action_id(&json!({"id": "explain-file"})), "explain-file");
    }

    #[test]
    fn read_one_action_requires_file_or_doc() {
        let err = read_one_action(None, None).unwrap_err().to_string();
        assert!(err.contains("--file"), "{err}");
        assert!(err.contains("--doc"), "{err}");
    }

    #[test]
    fn parse_one_action_parses_inline_doc() {
        let doc = json!({"id": "x", "name": "X", "run": {"kind": "open", "url": "/x"}, "scope": "personal"})
            .to_string();
        let action = parse_one_action(&doc).unwrap();
        assert_eq!(action["id"], "x");
    }

    #[test]
    fn parse_one_action_rejects_invalid_json() {
        let err = parse_one_action("not json").unwrap_err().to_string();
        assert!(err.contains("parse"), "{err}");
    }

    fn lines(out: &str) -> Vec<Vec<&str>> {
        out.lines().map(|l| l.split_whitespace().collect()).collect()
    }

    #[test]
    fn print_actions_handles_empty_and_populated_shapes() {
        assert_eq!(render_actions(&json!({}), Format::Human), "(no actions)\n");
        assert_eq!(render_actions(&json!({"actions": []}), Format::Human), "(no actions)\n");
        let populated = json!({"actions": [{"id": "a", "source": "personal", "run_kind": "chi", "name": "A"}]});
        assert_eq!(
            lines(&render_actions(&populated, Format::Human)),
            vec![vec!["ID", "SOURCE", "RUN", "NAME"], vec!["a", "personal", "chi", "A"]]
        );
        assert_eq!(
            render_actions(&json!({"actions": []}), Format::Json).trim(),
            json!({"actions": []}).to_string()
        );
    }

    #[test]
    fn print_import_result_reports_ids_not_just_counts() {
        // S4/C3: `added`/`skipped` are the ids themselves, not counts.
        let (stdout, stderr) = render_import_result(
            &json!({"ok": true, "added": ["a", "b"], "skipped": ["c"], "errors": [{"id": "x", "error": "bad"}]}),
            Format::Human,
        );
        assert_eq!(
            lines(&stdout),
            vec![
                vec!["ok:", "added", "2,", "skipped", "1"],
                vec!["added:", "a,", "b"],
                vec!["skipped:", "c"],
            ]
        );
        assert_eq!(stderr, "  x: bad\n");
    }

    #[test]
    fn print_import_result_omits_added_and_skipped_lines_when_empty() {
        let (stdout, stderr) = render_import_result(
            &json!({"ok": true, "added": [], "skipped": [], "errors": []}),
            Format::Human,
        );
        assert_eq!(stdout, "ok: added 0, skipped 0\n");
        assert_eq!(stderr, "");
    }

    #[test]
    fn apply_action_id_injects_when_absent_and_checks_when_present() {
        let mut doc = json!({"name": "X"});
        apply_action_id(&mut doc, "explain-file").unwrap();
        assert_eq!(doc["id"], "explain-file");

        let mut agreeing = json!({"id": "explain-file", "name": "X"});
        apply_action_id(&mut agreeing, "explain-file").unwrap();
        assert_eq!(agreeing["id"], "explain-file");

        let mut mismatched = json!({"id": "other-id", "name": "X"});
        let err = apply_action_id(&mut mismatched, "explain-file")
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not match"), "{err}");
    }

    #[test]
    fn apply_action_id_rejects_a_non_object_document() {
        let mut doc = json!([1, 2, 3]);
        let err = apply_action_id(&mut doc, "x").unwrap_err().to_string();
        assert!(err.contains("JSON object"), "{err}");
    }
}
