//! `iyke keys …` — the Keys noun (WP-62): D-06's `iyke keys list|set`
//! footer lines, plus the `resolve` query the WP-62 hand-off calls
//! "what fires here" (G-ACTIONS §2.3, `resolveKeypress`).
//!
//! Bridge routes:
//!   GET  /iyke/keys?search=<term>                          → `KeysResponse` (WP-21; WP-62 adds `?search=`)
//!   POST /iyke/keys/set { scope, key, command, when?,
//!                         key_scope?, platform? }            → `{ ok, ... }` (422 on a validator refusal)
//!   GET  /iyke/keys/resolve?key=<seq>&platform=<mac|other>   → `{ winner, candidates }`

use anyhow::Result;
use clap::Subcommand;
use serde_json::{json, Value};

use crate::api::Client;
use crate::output::{print_write_result, Format};

#[derive(Subcommand)]
pub enum KeysAction {
    /// List the effective keymap (G-ACTIONS §2.2), optionally filtered by a
    /// case-insensitive substring across command/key/when/label.
    List {
        #[arg(long)]
        search: Option<String>,
    },

    /// Add one keybinding rule (G-ACTIONS §1.5). This adds a new positive
    /// rule — it does not pair a negative rule to unbind a prior one on the
    /// same key, unlike the D-06 Keys tab's "Rebind…" flow. An
    /// `--key-scope os` rule is personal-only (`E_OS_LAYER`); a
    /// `--scope project` write is written **held** until the project's
    /// keybindings are trusted (DEC-65) — both enforced by the same
    /// validator the UI's Keys tab uses, not by this command.
    Set {
        /// `personal` or `project`.
        #[arg(long)]
        scope: String,
        /// An action id; a leading `-` makes a negative rule.
        #[arg(long)]
        command: String,
        /// Registry key grammar: `mod+shift+e`, or a two-stroke chord
        /// `mod+k mod+r`.
        #[arg(long)]
        key: String,
        #[arg(long)]
        when: Option<String>,
        /// `app` (default) or `os`.
        #[arg(long = "key-scope")]
        key_scope: Option<String>,
        /// `mac` or `other`.
        #[arg(long)]
        platform: Option<String>,
    },

    /// "What fires here": the live winner for a key sequence against the
    /// running shell's current focus/context.
    Resolve {
        /// Registry key grammar, e.g. `mod+k`.
        key: String,
        #[arg(long)]
        platform: Option<String>,
    },
}

pub fn run(client: &Client, action: KeysAction, fmt: Format) -> Result<()> {
    match action {
        KeysAction::List { search } => {
            let params: Vec<(&str, String)> = search.map(|s| vec![("search", s)]).unwrap_or_default();
            let v = client.get_with_query("/iyke/keys", &params)?;
            print_keys(&v, fmt);
        }
        KeysAction::Set {
            scope,
            command,
            key,
            when,
            key_scope,
            platform,
        } => {
            let label = format!("keys set {command} -> {key}");
            let body = json!({
                "scope": scope,
                "key": key,
                "command": command,
                "when": when,
                "key_scope": key_scope,
                "platform": platform,
            });
            let v = client.post("/iyke/keys/set", body)?;
            print_write_result(&label, &v, fmt);
        }
        KeysAction::Resolve { key, platform } => {
            let mut params = vec![("key", key)];
            if let Some(p) = platform {
                params.push(("platform", p));
            }
            let v = client.get_with_query("/iyke/keys/resolve", &params)?;
            print_resolve(&v, fmt);
        }
    }
    Ok(())
}

fn print_keys(v: &Value, fmt: Format) {
    match fmt {
        Format::Json => println!("{v}"),
        Format::Human => {
            let Some(entries) = v.get("entries").and_then(Value::as_array) else {
                println!("(no bindings)");
                return;
            };
            if entries.is_empty() {
                println!("(no bindings)");
                return;
            }
            println!("{:<28} {:<14} {:<8} {}", "COMMAND", "KEY", "SOURCE", "WHEN");
            for e in entries {
                let command = e.get("command").and_then(Value::as_str).unwrap_or("?");
                let key_label = e
                    .get("key_label")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .or_else(|| e.get("key").and_then(Value::as_str))
                    .unwrap_or("-");
                let source = e.get("source").and_then(Value::as_str).unwrap_or("?");
                let when = e.get("when").and_then(Value::as_str).unwrap_or("");
                println!("{command:<28} {key_label:<14} {source:<8} {when}");
            }
        }
    }
}

fn print_resolve(v: &Value, fmt: Format) {
    match fmt {
        Format::Json => println!("{v}"),
        Format::Human => {
            match v.get("winner") {
                Some(w) if !w.is_null() => {
                    let command = w.get("command").and_then(Value::as_str).unwrap_or("?");
                    let when = w.get("when").and_then(Value::as_str).unwrap_or("");
                    println!("winner: {command}  ({when})");
                }
                _ => println!("winner: (none)"),
            }
            let candidates = v.get("candidates").and_then(Value::as_array).cloned().unwrap_or_default();
            if candidates.len() > 1 {
                println!("candidates:");
                for c in &candidates {
                    let command = c.get("command").and_then(Value::as_str).unwrap_or("?");
                    let when = c.get("when").and_then(Value::as_str).unwrap_or("");
                    println!("  {command}  ({when})");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_entries() -> Value {
        json!({
            "schema_version": 1,
            "count": 2,
            "entries": [
                {"command": "palette.open", "key": "mod+k", "key_label": "Ctrl+K", "when": "!inputFocus", "source": "default", "label": "Command palette"},
                {"command": "explain-file", "key": "mod+shift+e", "key_label": "Ctrl+Shift+E", "when": "filesFocus", "source": "personal", "label": "Explain this file"}
            ]
        })
    }

    #[test]
    fn print_keys_handles_populated_empty_and_missing_shapes() {
        print_keys(&sample_entries(), Format::Human);
        print_keys(&json!({"entries": []}), Format::Human);
        print_keys(&json!({}), Format::Human);
        print_keys(&sample_entries(), Format::Json);
    }

    #[test]
    fn print_keys_falls_back_to_key_when_key_label_is_empty() {
        print_keys(
            &json!({"entries": [{"command": "x", "key": "mod+k", "key_label": "", "source": "default"}]}),
            Format::Human,
        );
    }

    #[test]
    fn print_resolve_handles_winner_and_no_winner() {
        print_resolve(
            &json!({"winner": {"command": "palette.open", "when": "!inputFocus"}, "candidates": [{"command": "palette.open"}]}),
            Format::Human,
        );
        print_resolve(&json!({"winner": null, "candidates": []}), Format::Human);
        print_resolve(&json!({"winner": null, "candidates": []}), Format::Json);
    }
}
