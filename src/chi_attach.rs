//! `iyke chi attach <run_id>` — follow a chi run's status file.
//!
//! chi-runner is spawned detached by the shell (no terminal session to attach
//! to). It writes a cumulative JSON status file (`output`, `status`, `error`,
//! `done_at`) atomically, and the shell serves it via `GET /iyke/chi/status`.
//! "Attaching" is therefore a poll loop: fetch the run state every
//! [`POLL_INTERVAL`], print only the output suffix that is new since the last
//! poll, and exit once the status is terminal.
//!
//! Ctrl-C (SIGINT's default action) just ends this follower process; the run
//! itself is untouched and keeps going.

use std::{io::Write, time::Duration};

use anyhow::Result;
use serde_json::Value;

/// How often `chi attach` polls the shell for run state.
pub const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// What to print given what has already been printed and the current output.
#[derive(Debug, PartialEq, Eq)]
pub enum OutputDelta<'a> {
    /// Nothing new since the last poll.
    Unchanged,
    /// `current` extends what was printed; print only this suffix.
    Append(&'a str),
    /// `current` no longer starts with what was printed (it shrank or was
    /// rewritten); reprint it in full.
    Reset(&'a str),
}

/// Pure diff between the already-printed output and the current cumulative
/// output. The byte offset tracked is `printed.len()`; the prefix check also
/// catches a same-length-or-longer rewrite, not just a shrink.
pub fn output_delta<'a>(printed: &str, current: &'a str) -> OutputDelta<'a> {
    if current == printed {
        OutputDelta::Unchanged
    } else if let Some(suffix) = current.strip_prefix(printed) {
        OutputDelta::Append(suffix)
    } else {
        OutputDelta::Reset(current)
    }
}

/// Terminal-state classification using the chi status vocabulary: chi-runner
/// writes `running` / `done` / `failed` / `timed_out`, and the shell adds
/// `queued` / `awaiting_auth` / `cancelled` on its side. `None` = keep polling.
#[derive(Debug, PartialEq, Eq)]
pub enum Terminal {
    Success,
    Failure,
}

pub fn classify_status(status: &str) -> Option<Terminal> {
    match status {
        "done" => Some(Terminal::Success),
        "failed" | "cancelled" | "timed_out" => Some(Terminal::Failure),
        _ => None,
    }
}

/// Poll `fetch` until the run is terminal, streaming new output to `out`.
/// Returns the process exit code: 0 for `done`, 1 for failed/cancelled/timed_out.
pub fn follow<F, O, E>(mut fetch: F, out: &mut O, err: &mut E, interval: Duration) -> Result<i32>
where
    F: FnMut() -> Result<Value>,
    O: Write,
    E: Write,
{
    let mut printed = String::new();
    loop {
        let v = fetch()?;
        let current = v.get("output").and_then(Value::as_str).unwrap_or("");

        match output_delta(&printed, current) {
            OutputDelta::Unchanged => {}
            OutputDelta::Append(suffix) => {
                out.write_all(suffix.as_bytes())?;
                out.flush()?;
                printed.push_str(suffix);
            }
            OutputDelta::Reset(full) => {
                writeln!(err, "iyke chi attach: output was reset; reprinting")?;
                if !printed.is_empty() && !printed.ends_with('\n') {
                    writeln!(out)?;
                }
                out.write_all(full.as_bytes())?;
                out.flush()?;
                printed.clear();
                printed.push_str(full);
            }
        }

        let status = v.get("status").and_then(Value::as_str).unwrap_or("");
        if let Some(terminal) = classify_status(status) {
            if !printed.is_empty() && !printed.ends_with('\n') {
                writeln!(out)?;
                out.flush()?;
            }
            return Ok(match terminal {
                Terminal::Success => 0,
                Terminal::Failure => {
                    match v.get("error").and_then(Value::as_str) {
                        Some(e) if !e.is_empty() => {
                            writeln!(err, "iyke chi attach: run {status}: {e}")?
                        }
                        _ => writeln!(err, "iyke chi attach: run {status}")?,
                    }
                    1
                }
            });
        }

        std::thread::sleep(interval);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn delta_unchanged_append_and_reset() {
        assert_eq!(output_delta("", ""), OutputDelta::Unchanged);
        assert_eq!(output_delta("abc", "abc"), OutputDelta::Unchanged);
        assert_eq!(output_delta("", "hello"), OutputDelta::Append("hello"));
        assert_eq!(output_delta("hel", "hello"), OutputDelta::Append("lo"));
        // Shrink → reprint.
        assert_eq!(output_delta("hello", "hel"), OutputDelta::Reset("hel"));
        assert_eq!(output_delta("hello", ""), OutputDelta::Reset(""));
        // Rewrite at the same or greater length → reprint.
        assert_eq!(output_delta("hello", "jello"), OutputDelta::Reset("jello"));
        assert_eq!(
            output_delta("hello", "HELLO world"),
            OutputDelta::Reset("HELLO world")
        );
    }

    #[test]
    fn delta_respects_multibyte_boundaries() {
        assert_eq!(output_delta("né", "né→ok"), OutputDelta::Append("→ok"));
        // "n" + first byte of 'é' would not be a prefix match; must reset, not panic.
        assert_eq!(output_delta("nx", "né"), OutputDelta::Reset("né"));
    }

    #[test]
    fn classify_uses_chi_vocabulary() {
        assert_eq!(classify_status("done"), Some(Terminal::Success));
        for s in ["failed", "cancelled", "timed_out"] {
            assert_eq!(classify_status(s), Some(Terminal::Failure), "{s}");
        }
        for s in ["running", "queued", "awaiting_auth", "", "weird"] {
            assert_eq!(classify_status(s), None, "{s}");
        }
    }

    fn run_script(script: Vec<Value>) -> (i32, String, String, usize) {
        let mut polls = 0usize;
        let mut it = script.into_iter();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = follow(
            || {
                polls += 1;
                Ok(it.next().expect("follow polled past the terminal state"))
            },
            &mut out,
            &mut err,
            Duration::ZERO,
        )
        .unwrap();
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
            polls,
        )
    }

    #[test]
    fn follow_prints_only_new_suffix_and_exits_zero_on_done() {
        let (code, out, err, polls) = run_script(vec![
            json!({"status": "queued"}),
            json!({"status": "running", "output": null}),
            json!({"status": "running", "output": "Hello"}),
            json!({"status": "running", "output": "Hello"}),
            json!({"status": "running", "output": "Hello, wor"}),
            json!({"status": "done", "output": "Hello, world"}),
        ]);
        assert_eq!(code, 0);
        assert_eq!(out, "Hello, world\n");
        assert_eq!(err, "");
        assert_eq!(polls, 6);
    }

    #[test]
    fn follow_reprints_on_reset() {
        let (code, out, err, _) = run_script(vec![
            json!({"status": "running", "output": "draft one"}),
            json!({"status": "running", "output": "final\n"}),
            json!({"status": "done", "output": "final\nmore\n"}),
        ]);
        assert_eq!(code, 0);
        assert_eq!(out, "draft one\nfinal\nmore\n");
        assert!(err.contains("reset"), "{err}");
    }

    #[test]
    fn follow_exits_nonzero_with_error_on_failure_states() {
        let (code, out, err, _) = run_script(vec![
            json!({"status": "running", "output": "partial"}),
            json!({"status": "failed", "output": "partial", "error": "engine exited with code 2"}),
        ]);
        assert_eq!(code, 1);
        assert_eq!(out, "partial\n");
        assert_eq!(
            err,
            "iyke chi attach: run failed: engine exited with code 2\n"
        );

        let (code, _, err, _) = run_script(vec![json!({"status": "cancelled"})]);
        assert_eq!(code, 1);
        assert_eq!(err, "iyke chi attach: run cancelled\n");

        let (code, _, err, _) = run_script(vec![
            json!({"status": "timed_out", "error": "timed out after 60s"}),
        ]);
        assert_eq!(code, 1);
        assert!(err.contains("timed out after 60s"));
    }

    #[test]
    fn follow_propagates_fetch_errors() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let r = follow(
            || Err(anyhow::anyhow!("chi run not found: x")),
            &mut out,
            &mut err,
            Duration::ZERO,
        );
        assert!(r.is_err());
    }
}
