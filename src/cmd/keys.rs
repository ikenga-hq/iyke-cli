//! `iyke keys …` — the Keys noun (WP-62): D-06's `iyke keys list|set`
//! footer lines, plus the `resolve` query the WP-62 hand-off calls
//! "what fires here" (G-ACTIONS §2.3, `resolveKeypress`).
//!
//! Bridge routes:
//!   GET  /iyke/keys?search=<term>                          → `KeysResponse` (WP-21; WP-62 adds `?search=`)
//!   POST /iyke/keys/set { scope, key, command, when?,
//!                         key_scope?, platform? }            → `{ ok, ... }` (422 on a validator refusal)
//!   GET  /iyke/keys/resolve?key=<seq>&platform=<mac|other>   → `{ winner, candidates }`

use std::fmt::Write as _;

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
            // WP-62 review (C5): `resolve` is a live frontend round trip
            // (`rpc::request_to`, `actions_routes.rs`), not a plain read off
            // an in-memory mirror — give it more slack than the 5s default so
            // a briefly busy webview doesn't read as "shell not running".
            let v = client.get_with_query_timeout(
                "/iyke/keys/resolve",
                &params,
                std::time::Duration::from_secs(12),
            )?;
            print_resolve(&v, fmt);
        }
    }
    Ok(())
}

fn print_keys(v: &Value, fmt: Format) {
    print!("{}", render_keys(v, fmt));
}

/// WP-62 review ("make the print tests assert on the output"): renders to a
/// `String` instead of printing directly, so tests can assert on exact
/// content rather than just "did not panic". `print_keys` is the thin
/// runtime wrapper.
fn render_keys(v: &Value, fmt: Format) -> String {
    let mut out = String::new();
    match fmt {
        Format::Json => {
            let _ = writeln!(out, "{v}");
        }
        Format::Human => {
            let Some(entries) = v.get("entries").and_then(Value::as_array) else {
                let _ = writeln!(out, "(no bindings)");
                return out;
            };
            if entries.is_empty() {
                let _ = writeln!(out, "(no bindings)");
                return out;
            }
            let _ = writeln!(out, "{:<28} {:<14} {:<8} {}", "COMMAND", "KEY", "SOURCE", "WHEN");
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
                // WP-62 review (S3, DEC-65): a held project rule prints its
                // trust state alongside `status: held` instead of a plain
                // `WHEN`, so `keys list` doesn't read as a live, firing rule.
                let status = e.get("status").and_then(Value::as_str).filter(|s| *s == "held");
                match status {
                    Some(_) => {
                        let trust = e.get("trust").and_then(Value::as_str).unwrap_or("unknown");
                        let _ = writeln!(
                            out,
                            "{command:<28} {key_label:<14} {source:<8} held ({trust})"
                        );
                    }
                    None => {
                        let _ = writeln!(out, "{command:<28} {key_label:<14} {source:<8} {when}");
                    }
                }
            }
        }
    }
    out
}

fn print_resolve(v: &Value, fmt: Format) {
    print!("{}", render_resolve(v, fmt));
}

fn render_resolve(v: &Value, fmt: Format) -> String {
    let mut out = String::new();
    match fmt {
        Format::Json => {
            let _ = writeln!(out, "{v}");
        }
        Format::Human => {
            match v.get("winner") {
                Some(w) if !w.is_null() => {
                    let command = w.get("command").and_then(Value::as_str).unwrap_or("?");
                    let when = w.get("when").and_then(Value::as_str).unwrap_or("");
                    let _ = writeln!(out, "winner: {command}  ({when})");
                }
                _ => {
                    let _ = writeln!(out, "winner: (none)");
                }
            }
            let candidates = v.get("candidates").and_then(Value::as_array).cloned().unwrap_or_default();
            if candidates.len() > 1 {
                let _ = writeln!(out, "candidates:");
                for c in &candidates {
                    let command = c.get("command").and_then(Value::as_str).unwrap_or("?");
                    let when = c.get("when").and_then(Value::as_str).unwrap_or("");
                    let _ = writeln!(out, "  {command}  ({when})");
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

    fn lines(out: &str) -> Vec<Vec<&str>> {
        out.lines().map(|l| l.split_whitespace().collect()).collect()
    }

    #[test]
    fn print_keys_handles_populated_empty_and_missing_shapes() {
        let out = render_keys(&sample_entries(), Format::Human);
        assert_eq!(
            lines(&out),
            vec![
                vec!["COMMAND", "KEY", "SOURCE", "WHEN"],
                vec!["palette.open", "Ctrl+K", "default", "!inputFocus"],
                vec!["explain-file", "Ctrl+Shift+E", "personal", "filesFocus"],
            ]
        );
        assert_eq!(render_keys(&json!({"entries": []}), Format::Human), "(no bindings)\n");
        assert_eq!(render_keys(&json!({}), Format::Human), "(no bindings)\n");
        assert_eq!(
            render_keys(&sample_entries(), Format::Json).trim(),
            sample_entries().to_string()
        );
    }

    #[test]
    fn print_keys_falls_back_to_key_when_key_label_is_empty() {
        let out = render_keys(
            &json!({"entries": [{"command": "x", "key": "mod+k", "key_label": "", "source": "default"}]}),
            Format::Human,
        );
        assert_eq!(lines(&out)[1], vec!["x", "mod+k", "default"]);
    }

    #[test]
    fn print_keys_marks_a_held_row_with_its_trust_state_instead_of_when() {
        // WP-62 review (S3, DEC-65): a held project rule fires nothing, so
        // `keys list` says so instead of printing its (inert) `when`.
        let out = render_keys(
            &json!({"entries": [
                {"command": "delete", "key": "mod+shift+d", "key_label": "Ctrl+Shift+D", "source": "project", "when": "filesFocus", "status": "held", "trust": "untrusted"}
            ]}),
            Format::Human,
        );
        assert_eq!(
            lines(&out)[1],
            vec!["delete", "Ctrl+Shift+D", "project", "held", "(untrusted)"]
        );
    }

    #[test]
    fn print_resolve_handles_winner_and_no_winner() {
        let out = render_resolve(
            &json!({"winner": {"command": "palette.open", "when": "!inputFocus"}, "candidates": [{"command": "palette.open"}, {"command": "terminal.clear"}]}),
            Format::Human,
        );
        assert_eq!(
            lines(&out),
            vec![
                vec!["winner:", "palette.open", "(!inputFocus)"],
                vec!["candidates:"],
                vec!["palette.open", "()"],
                vec!["terminal.clear", "()"],
            ]
        );

        assert_eq!(
            render_resolve(&json!({"winner": null, "candidates": []}), Format::Human),
            "winner: (none)\n"
        );
        assert_eq!(
            render_resolve(&json!({"winner": null, "candidates": []}), Format::Json).trim(),
            json!({"winner": null, "candidates": []}).to_string()
        );
    }
}
