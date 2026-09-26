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

use std::fs;
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use clap::Subcommand;
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
        /// `personal` or `project`.
        #[arg(long)]
        scope: String,
        /// Path to a JSON file holding one UserAction document.
        #[arg(long, conflicts_with = "doc")]
        file: Option<PathBuf>,
        /// The UserAction document as an inline JSON string.
        #[arg(long, conflicts_with = "file")]
        doc: Option<String>,
    },

    /// Upsert a batch of user actions from a JSON file: either
    /// `{"actions": [...]}` (the same envelope `actions list` prints) or a
    /// bare `[...]` array of UserAction documents. One invalid entry is
    /// reported and skipped; it doesn't sink the rest of the batch.
    Import {
        /// `personal` or `project`.
        #[arg(long)]
        scope: String,
        #[arg(long)]
        file: PathBuf,
    },
}

pub fn run(client: &Client, action: ActionsAction, fmt: Format) -> Result<()> {
    match action {
        ActionsAction::List { scope } => {
            let params: Vec<(&str, String)> = scope.map(|s| vec![("scope", s)]).unwrap_or_default();
            let v = client.get_with_query("/iyke/actions", &params)?;
            print_actions(&v, fmt);
        }
        ActionsAction::Set { scope, file, doc } => {
            let action = read_one_action(file, doc)?;
            let label = format!("actions set {}", action_id(&action));
            let v = client.post("/iyke/actions/set", json!({ "scope": scope, "action": action }))?;
            print_write_result(&label, &v, fmt);
        }
        ActionsAction::Import { scope, file } => {
            let actions = read_action_batch(&file)?;
            let v = client.post(
                "/iyke/actions/import",
                json!({ "scope": scope, "actions": actions }),
            )?;
            print_import_result(&v, fmt);
        }
    }
    Ok(())
}

fn action_id(action: &Value) -> &str {
    action.get("id").and_then(Value::as_str).unwrap_or("?")
}

fn read_one_action(file: Option<PathBuf>, doc: Option<String>) -> Result<Value> {
    let text = match (file, doc) {
        (Some(path), None) => {
            fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?
        }
        (None, Some(inline)) => inline,
        (None, None) => {
            return Err(anyhow!(
                "actions set needs one of --file <path> or --doc '<json>'"
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
    match fmt {
        Format::Json => println!("{v}"),
        Format::Human => {
            let actions = v.get("actions").and_then(Value::as_array);
            let Some(actions) = actions else {
                println!("(no actions)");
                return;
            };
            if actions.is_empty() {
                println!("(no actions)");
                return;
            }
            println!("{:<28} {:<10} {:<8} {}", "ID", "SOURCE", "RUN", "NAME");
            for a in actions {
                let id = a.get("id").and_then(Value::as_str).unwrap_or("?");
                let source = a.get("source").and_then(Value::as_str).unwrap_or("?");
                let run_kind = a.get("run_kind").and_then(Value::as_str).unwrap_or("-");
                let name = a.get("name").and_then(Value::as_str).unwrap_or("-");
                println!("{id:<28} {source:<10} {run_kind:<8} {name}");
            }
        }
    }
}

fn print_import_result(v: &Value, fmt: Format) {
    match fmt {
        Format::Json => println!("{v}"),
        Format::Human => {
            let added = v.get("added").and_then(Value::as_u64).unwrap_or(0);
            let skipped = v.get("skipped").and_then(Value::as_u64).unwrap_or(0);
            println!("ok: added {added}, skipped {skipped}");
            if let Some(errors) = v.get("errors").and_then(Value::as_array) {
                for e in errors {
                    let id = e.get("id").and_then(Value::as_str).unwrap_or("?");
                    let error = e.get("error").and_then(Value::as_str).unwrap_or("?");
                    eprintln!("  {id}: {error}");
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

    #[test]
    fn print_actions_and_import_result_do_not_panic_on_empty_or_malformed() {
        print_actions(&json!({}), Format::Human);
        print_actions(&json!({"actions": []}), Format::Human);
        print_actions(
            &json!({"actions": [{"id": "a", "source": "personal", "run_kind": "chi", "name": "A"}]}),
            Format::Human,
        );
        print_actions(&json!({"actions": []}), Format::Json);
        print_import_result(&json!({"added": 2, "skipped": 1, "errors": [{"id": "x", "error": "bad"}]}), Format::Human);
        print_import_result(&json!({"added": 0, "skipped": 0}), Format::Human);
    }
}
