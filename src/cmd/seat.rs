//! `iyke seat …` and `iyke terminal-send --seat` — the Chi seats surface
//! (WP-70; G-SEATS §7, `plans/shell-ux-rearchitecture/drafts/seats-schema.md`
//! in the workspace meta-repo).
//!
//! A **seat** is a named, per-project slot (`seat:<project>/<name>`) that
//! points at one session — a Chi run or an agent terminal. `--seat` is its
//! own addressing mode (G-91), not an alias of `--label`: `--label` finds a
//! live PTY by a global, in-memory name; `--seat` finds the seat in its
//! project and routes to whatever its session is — including a vacant seat,
//! which resumes its last session and then sends (DEC-69a).
//!
//! Bridge routes (shell `BRIDGE_API` 5, `src-tauri/src/iyke/seat_routes.rs`):
//!   GET  /iyke/seats/list?project=<id>              → SeatView[]
//!   POST /iyke/seats/create   {project?, name, engine?, session?, resume?}
//!   POST /iyke/seats/resume   {seat, session?, prompt?}
//!   POST /iyke/seats/fill     {seat, prompt}
//!   POST /iyke/seats/clear    {seat}
//!   POST /iyke/seats/release  {seat}
//!   POST /iyke/seats/send     {seat, text, lease_token?}
//! Every POST carries the §5 actor: `client` (`--as`), `hold` (`--hold`),
//! `takeover` (`--takeover`). A refusal is `{code, message, details?}` at
//! the code's HTTP status; [`seat_error`] turns it into a readable error.
//!
//! **Client id.** `--as` defaults to `"iyke"`: this CLI has no persisted
//! agent identity of its own (`iyke agent register` returns an id but keeps
//! none), so the fallback the contract names applies.

use std::fmt::Write as _;

use anyhow::{anyhow, Error, Result};
use clap::{Args, Subcommand};
use serde_json::{json, Map, Value};

use crate::api::{Client, HttpStatusError};
use crate::cmd::require_bridge_api;
use crate::output::Format;

/// The §5 client id when `--as` isn't given (see the module doc).
pub const DEFAULT_CLIENT: &str = "iyke";

/// The bridge API level that serves `/iyke/seats/*`.
pub const SEATS_BRIDGE_API: u32 = 5;

/// §5 flags common to every seat verb. Global within `iyke seat`, so
/// `iyke seat --as orch ls` and `iyke seat ls --as orch` both work.
#[derive(Args, Clone, Debug, Default, PartialEq)]
pub struct SeatActorArgs {
    /// Take over a seat another client holds. Explicit only — a takeover
    /// never happens by timeout. The displaced client is told on its next
    /// call.
    #[arg(long, global = true)]
    pub takeover: bool,
    /// Acquire (or renew) this client's hold on the seat, 10 minutes by
    /// default. While held, other clients are refused unless they pass
    /// `--takeover`.
    #[arg(long, global = true)]
    pub hold: bool,
    /// The client id recorded as the seat's holder (§5). Defaults to `iyke`.
    #[arg(long = "as", value_name = "CLIENT", global = true)]
    pub client: Option<String>,
}

impl SeatActorArgs {
    pub fn client_id(&self) -> String {
        self.client
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .unwrap_or(DEFAULT_CLIENT)
            .to_string()
    }
}

#[derive(Subcommand, Debug, PartialEq)]
pub enum SeatAction {
    /// List the seats of a project (default: the shell's active project):
    /// status, session, who holds it and since when, and engine caveats
    /// ("not resumable after restart", "can't resume sessions").
    #[command(visible_alias = "list")]
    Ls {
        /// Project id. Defaults to the shell's active project.
        #[arg(long)]
        project: Option<String>,
    },

    /// Create a seat. With `--session` (an open session) or `--resume` (a
    /// past one) the session moves into the new seat — out of any other
    /// seat it sat in. A resumed past session resumes on the next send.
    ///
    /// Examples (the locked create form's lines):
    ///   iyke seat create docs --engine claude-code
    ///   iyke seat create docs --session <terminal or run id>
    ///   iyke seat create docs --engine claude-code --resume <run id>
    Create {
        /// Seat name: 1–32 of `a-z`, `0-9`, `-`, starting and ending with a
        /// letter or digit. Unique per project.
        name: String,
        /// Chi engine id (`claude-code`, `codex`, `antigravity-cli`,
        /// `openrouter`, …). Required unless `--session`/`--resume` names
        /// a session whose engine the shell can tell.
        #[arg(long)]
        engine: Option<String>,
        /// An open session to seat: a terminal id or a Chi run id.
        #[arg(long, conflicts_with = "resume")]
        session: Option<String>,
        /// A past session to seat: a terminal id or a Chi run id.
        #[arg(long)]
        resume: Option<String>,
        /// Project id. Defaults to the shell's active project.
        #[arg(long)]
        project: Option<String>,
    },

    /// Resume a seat. `--session` moves that session into the seat;
    /// `--prompt` resumes the seat's own (vacant) session with the text as
    /// its first turn — and never falls back to a fresh session. With
    /// neither, the shell answers `needs_prompt` (an interactive resume
    /// needs the Ikenga window).
    Resume {
        /// `<name>`, `@<name>`, `<project>/<name>` or `seat:<project>/<name>`.
        seat: String,
        /// A terminal id or Chi run id to move into the seat.
        #[arg(long, conflicts_with = "prompt")]
        session: Option<String>,
        /// The resumed session's first turn.
        #[arg(long)]
        prompt: Option<String>,
    },

    /// Fill a seat with a new session on its engine, started with `--prompt`.
    /// The seat's previous session, if any, keeps running, unseated.
    Fill {
        seat: String,
        #[arg(long)]
        prompt: String,
    },

    /// Clear a seat: it forgets its session (which keeps running, unseated).
    /// The seat's scratchpad is kept.
    Clear { seat: String },

    /// Release this client's hold on a seat (someone else's needs
    /// `--takeover`).
    Release { seat: String },
}

pub fn run(client: &Client, actor: &SeatActorArgs, action: SeatAction, fmt: Format) -> Result<()> {
    require_bridge_api(client, SEATS_BRIDGE_API, "iyke seat")?;
    match action {
        SeatAction::Ls { project } => {
            let params: Vec<(&str, String)> = project
                .filter(|p| !p.trim().is_empty())
                .map(|p| vec![("project", p)])
                .unwrap_or_default();
            let v = client
                .get_with_query("/iyke/seats/list", &params)
                .map_err(seat_error)?;
            print!("{}", render_seats(&v, fmt, now_ms()));
        }
        SeatAction::Create {
            name,
            engine,
            session,
            resume,
            project,
        } => {
            let body = create_body(&name, engine, session, resume, project, actor);
            let v = client
                .post("/iyke/seats/create", body)
                .map_err(seat_error)?;
            print!("{}", render_view_result("created", &v, fmt, now_ms()));
        }
        SeatAction::Resume {
            seat,
            session,
            prompt,
        } => {
            let body = with_actor(
                json!({ "seat": seat, "session": session, "prompt": prompt }),
                actor,
            );
            let v = client
                .post("/iyke/seats/resume", body)
                .map_err(seat_error)?;
            let out = if v.get("outcome").is_some() {
                render_resume_result(&v, fmt)
            } else {
                render_view_result("seated the session in", &v, fmt, now_ms())
            };
            print!("{out}");
        }
        SeatAction::Fill { seat, prompt } => {
            let body = with_actor(json!({ "seat": seat, "prompt": prompt }), actor);
            let v = client.post("/iyke/seats/fill", body).map_err(seat_error)?;
            print!("{}", render_fill_result(&v, fmt));
        }
        SeatAction::Clear { seat } => {
            let body = with_actor(json!({ "seat": seat }), actor);
            let v = client.post("/iyke/seats/clear", body).map_err(seat_error)?;
            print!("{}", render_view_result("cleared", &v, fmt, now_ms()));
        }
        SeatAction::Release { seat } => {
            let body = with_actor(json!({ "seat": seat }), actor);
            let v = client
                .post("/iyke/seats/release", body)
                .map_err(seat_error)?;
            print!("{}", render_view_result("released", &v, fmt, now_ms()));
        }
    }
    Ok(())
}

/// `iyke terminal-send --seat <seat> "<text>"` → `POST /iyke/seats/send`.
/// The shell routes by the seat's session: an occupied terminal gets the
/// text plus Enter (under that PTY's lease — pass `--lease-token`), a busy
/// run queues it, an idle run resumes with it, and a vacant seat resumes its
/// last session (or starts a fresh one, and says so) with it.
pub fn send(
    client: &Client,
    seat: &str,
    text: &str,
    lease_token: Option<String>,
    actor: &SeatActorArgs,
    fmt: Format,
) -> Result<()> {
    require_bridge_api(client, SEATS_BRIDGE_API, "terminal-send --seat")?;
    let body = send_body(seat, text, lease_token, actor);
    let v = client.post("/iyke/seats/send", body).map_err(seat_error)?;
    print!("{}", render_send_result(&v, fmt));
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════
// Request bodies
// ═══════════════════════════════════════════════════════════════════════

/// Merge the §5 actor fields into a body and drop `null` fields, so an
/// absent flag is absent on the wire rather than `null`.
pub fn with_actor(body: Value, actor: &SeatActorArgs) -> Value {
    let mut out: Map<String, Value> = match body {
        Value::Object(m) => m.into_iter().filter(|(_, v)| !v.is_null()).collect(),
        _ => Map::new(),
    };
    out.insert("client".into(), json!(actor.client_id()));
    out.insert("hold".into(), json!(actor.hold));
    out.insert("takeover".into(), json!(actor.takeover));
    Value::Object(out)
}

pub fn create_body(
    name: &str,
    engine: Option<String>,
    session: Option<String>,
    resume: Option<String>,
    project: Option<String>,
    actor: &SeatActorArgs,
) -> Value {
    with_actor(
        json!({
            "name": name,
            "engine": engine,
            "session": session,
            "resume": resume,
            "project": project,
        }),
        actor,
    )
}

pub fn send_body(
    seat: &str,
    text: &str,
    lease_token: Option<String>,
    actor: &SeatActorArgs,
) -> Value {
    with_actor(
        json!({ "seat": seat, "text": text, "lease_token": lease_token }),
        actor,
    )
}

// ═══════════════════════════════════════════════════════════════════════
// Errors
// ═══════════════════════════════════════════════════════════════════════

/// Turn a seat route's `{code, message, details?}` refusal into a readable
/// error (§5.2: a hold refusal says who holds it since when, and how to
/// claim it). Anything else — a transport failure, a non-JSON body — passes
/// through untouched.
pub fn seat_error(e: Error) -> Error {
    let parsed = e.downcast_ref::<HttpStatusError>().and_then(|http| {
        serde_json::from_str::<Value>(&http.body)
            .ok()
            .map(|body| (http.status, body))
    });
    let Some((status, body)) = parsed else {
        return e;
    };
    let Some(code) = body.get("code").and_then(Value::as_str) else {
        return e;
    };
    let message = body.get("message").and_then(Value::as_str).unwrap_or("");
    anyhow!(
        "{}",
        render_seat_error(code, message, body.get("details"), status)
    )
}

pub fn render_seat_error(
    code: &str,
    message: &str,
    details: Option<&Value>,
    status: u16,
) -> String {
    let int = |key: &str| details.and_then(|d| d.get(key)).and_then(Value::as_i64);
    match code {
        "seat_held" => {
            let mut msg = message.to_string();
            if let Some(since) = int("since") {
                msg = msg.replace(&since.to_string(), &fmt_time(since));
            }
            format!("seat_held: {msg}; --takeover to claim it")
        }
        "seat_taken_over" => {
            let mut msg = message.to_string();
            if let Some(at) = int("at") {
                msg = msg.replace(&at.to_string(), &fmt_time(at));
            }
            format!("seat_taken_over: {msg} — your hold is gone; --takeover to take the seat back")
        }
        "needs_prompt" => format!("needs_prompt: {message}"),
        "not_resumable" => format!(
            "not_resumable: {message} — `iyke seat fill <seat> --prompt …` starts a new session"
        ),
        _ => format!("{code} (HTTP {status}): {message}"),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Rendering
// ═══════════════════════════════════════════════════════════════════════

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Unix ms → `YYYY-MM-DD HH:MMZ` (UTC). No date crate: Howard Hinnant's
/// civil-from-days.
pub fn fmt_time(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let secs = ms.rem_euclid(86_400_000) / 1000;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}Z",
        secs / 3600,
        (secs % 3600) / 60
    )
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

/// `@name` for a view (the address's name part), for human lines.
fn at_name(view: &Value) -> String {
    let name = s(view, "name");
    if name.is_empty() {
        s(view, "address").to_string()
    } else {
        format!("@{name}")
    }
}

/// The session column: `run <id>`, `terminal <id> (agent live)`, or `—`.
pub fn session_label(view: &Value) -> String {
    let Some(session) = view.get("session").filter(|v| !v.is_null()) else {
        return "—".to_string();
    };
    match s(session, "kind") {
        "run" => format!("run {}", s(session, "run_id")),
        "terminal" => {
            let mut out = format!("terminal {}", s(session, "terminal_id"));
            if let Some(agent) = view.get("agent").and_then(Value::as_str) {
                let _ = write!(out, " (agent {agent})");
            }
            out
        }
        other => other.to_string(),
    }
}

/// The notes under a seat's line, in the order D-09 shows them: the hold
/// ("held by X since T"), the engine caveat, then resume / queue / mount.
pub fn seat_notes(view: &Value, now: i64) -> Vec<String> {
    let mut notes = Vec::new();
    if let Some(hold) = view.get("hold").filter(|h| !h.is_null()) {
        let expires = hold.get("expires_at").and_then(Value::as_i64).unwrap_or(0);
        if expires > now {
            let since = hold.get("since").and_then(Value::as_i64).unwrap_or(0);
            notes.push(format!(
                "held by {} since {}",
                s(hold, "client"),
                fmt_time(since)
            ));
        }
    }
    match s(view, "engine_resume") {
        // §6.2: carried at all times, not only once vacant.
        "process-local" => notes.push("not resumable after restart".to_string()),
        "none" => notes.push("can't resume sessions".to_string()),
        _ => {}
    }
    if s(view, "status") == "vacant" {
        let resume = view.get("resume");
        let resumable = resume
            .and_then(|r| r.get("resumable"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if resumable {
            notes.push("resumes its session on the next send".to_string());
        } else {
            let reason = resume.map(|r| s(r, "reason")).unwrap_or("");
            match reason {
                "" | "no_session" => {
                    notes.push("empty — the next send starts a session".to_string())
                }
                reason => notes.push(format!(
                    "can't resume ({reason}) — the next send starts a new session"
                )),
            }
        }
    }
    if let Some(queued) = view.get("queued").filter(|q| !q.is_null()) {
        let since = queued.get("since").and_then(Value::as_i64).unwrap_or(0);
        notes.push(format!(
            "a text is queued since {} — sends when its run finishes",
            fmt_time(since)
        ));
    }
    if let Some(mount) = view.get("mount").filter(|m| !m.is_null()) {
        let window = s(mount, "window_label");
        if !window.is_empty() && window != "main" {
            notes.push(format!("popped out ({window})"));
        }
    }
    notes
}

/// One seat: `address  status  engine  session`, then its notes indented.
fn render_seat_lines(out: &mut String, view: &Value, now: i64) {
    let _ = writeln!(
        out,
        "{:<32} {:<7} {:<16} {}",
        s(view, "address"),
        s(view, "status"),
        s(view, "engine_id"),
        session_label(view)
    );
    for note in seat_notes(view, now) {
        let _ = writeln!(out, "    {note}");
    }
}

/// `iyke seat ls`.
pub fn render_seats(v: &Value, fmt: Format, now: i64) -> String {
    let mut out = String::new();
    match fmt {
        Format::Json => {
            let _ = writeln!(out, "{v}");
        }
        Format::Human => {
            let seats = v.as_array().cloned().unwrap_or_default();
            if seats.is_empty() {
                let _ = writeln!(out, "(no seats)");
            }
            for seat in &seats {
                render_seat_lines(&mut out, seat, now);
            }
        }
    }
    out
}

/// A write that answers with one `SeatView` (create, move, clear, release).
pub fn render_view_result(verb: &str, v: &Value, fmt: Format, now: i64) -> String {
    let mut out = String::new();
    match fmt {
        Format::Json => {
            let _ = writeln!(out, "{v}");
        }
        Format::Human => {
            let _ = writeln!(out, "ok: {verb} {}", s(v, "address"));
            render_seat_lines(&mut out, v, now);
        }
    }
    out
}

/// `SeatResumeResult` (`seat resume --prompt`).
pub fn render_resume_result(v: &Value, fmt: Format) -> String {
    match fmt {
        Format::Json => format!("{v}\n"),
        Format::Human => {
            let seat = v.get("seat").cloned().unwrap_or(Value::Null);
            format!("ok: resumed {} — run {}\n", at_name(&seat), s(v, "run_id"))
        }
    }
}

/// `SeatFillResult` (`seat fill`).
pub fn render_fill_result(v: &Value, fmt: Format) -> String {
    match fmt {
        Format::Json => format!("{v}\n"),
        Format::Human => {
            let seat = v.get("seat").cloned().unwrap_or(Value::Null);
            let mut out = format!("ok: filled {} — run {}\n", at_name(&seat), s(v, "run_id"));
            if let Some(prev) = v.get("previous").filter(|p| !p.is_null()) {
                let prev_view = json!({ "session": prev });
                let _ = writeln!(
                    out,
                    "    its previous session ({}) keeps running, unseated",
                    session_label(&prev_view)
                );
            }
            out
        }
    }
}

/// `/iyke/seats/send`'s answer, in the §6.3 toast wording (a CLI says run
/// ids where the UI says session numbers).
pub fn render_send_result(v: &Value, fmt: Format) -> String {
    if let Format::Json = fmt {
        return format!("{v}\n");
    }
    let seat = v.get("seat").cloned().unwrap_or(Value::Null);
    let name = at_name(&seat);
    let engine = s(&seat, "engine_id");
    let run = s(v, "run_id");
    let line = match s(v, "route") {
        "pty" => format!("sent to {name} (terminal {})", s(v, "terminal_id")),
        "chi-resume" if v.get("queued").and_then(Value::as_bool) == Some(true) => {
            format!("Queued for {name} — sends when its run finishes (run {run})")
        }
        "chi-resume" => format!("sent to {name} (run {run})"),
        "vacant" => match (s(v, "outcome"), s(v, "reason")) {
            ("resumed", _) => format!("{name} was vacant — resumed run {run}, then sent"),
            ("started-fresh", "process_local") => format!(
                "{name} was vacant — its {engine} session can't resume after restart; started a new one (run {run})"
            ),
            ("started-fresh", "no_session") | ("started-fresh", "") => {
                format!("{name} was vacant — filled with run {run}, then sent")
            }
            ("started-fresh", _) => format!(
                "{name} was vacant — its {engine} session can't be resumed; started a new one (run {run})"
            ),
            _ => format!("sent to {name} (run {run})"),
        },
        other => format!("sent to {name} ({other})"),
    };
    format!("{line}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actor(client: Option<&str>, hold: bool, takeover: bool) -> SeatActorArgs {
        SeatActorArgs {
            takeover,
            hold,
            client: client.map(str::to_string),
        }
    }

    fn view(status: &str, engine_resume: &str) -> Value {
        json!({
            "id": "5f0c", "project_id": "royalti-co", "name": "lead",
            "engine_id": "claude-code", "session": null, "created_at": 0,
            "last_active_at": 0, "hold": null, "address": "seat:royalti-co/lead",
            "agent_id": "5f0c", "status": status, "agent": null,
            "resume": { "resumable": false, "reason": "no_session" },
            "engine_resume": engine_resume, "mount": null, "queued": null,
            "pad": { "count": 0, "latest": null }, "inbox_count": 0
        })
    }

    #[test]
    fn as_defaults_to_iyke() {
        assert_eq!(SeatActorArgs::default().client_id(), "iyke");
        assert_eq!(actor(Some("  "), false, false).client_id(), "iyke");
        assert_eq!(
            actor(Some("orchestrator"), false, false).client_id(),
            "orchestrator"
        );
    }

    #[test]
    fn every_body_carries_the_actor_and_drops_absent_flags() {
        let body = create_body(
            "docs",
            Some("claude-code".into()),
            None,
            Some("run-9".into()),
            None,
            &actor(Some("orch"), true, false),
        );
        assert_eq!(
            body,
            json!({
                "name": "docs", "engine": "claude-code", "resume": "run-9",
                "client": "orch", "hold": true, "takeover": false
            })
        );
        let send = send_body("lead", "go", None, &actor(None, false, true));
        assert_eq!(
            send,
            json!({ "seat": "lead", "text": "go", "client": "iyke", "hold": false, "takeover": true })
        );
        let send = send_body("lead", "go", Some("tok".into()), &SeatActorArgs::default());
        assert_eq!(send["lease_token"], "tok");
    }

    #[test]
    fn fmt_time_is_utc_minutes() {
        assert_eq!(fmt_time(0), "1970-01-01 00:00Z");
        assert_eq!(fmt_time(1_790_517_780_000), "2026-09-27 14:03Z");
        assert_eq!(fmt_time(951_782_400_000), "2000-02-29 00:00Z");
        assert_eq!(fmt_time(-60_000), "1969-12-31 23:59Z");
    }

    #[test]
    fn ls_shows_status_session_hold_and_the_engine_caveat() {
        let now = 1_790_517_800_000;
        let mut lead = view("live", "durable");
        lead["session"] = json!({ "kind": "terminal", "terminal_id": "t-1",
                                  "external_id": null, "cwd": "/w" });
        lead["agent"] = json!("live");
        lead["hold"] = json!({ "client": "orchestrator", "since": 1_790_517_780_000_i64,
                               "expires_at": now + 60_000 });
        let mut nightly = view("run", "process-local");
        nightly["address"] = json!("seat:royalti-co/nightly");
        nightly["engine_id"] = json!("openrouter");
        nightly["session"] = json!({ "kind": "run", "run_id": "r-9",
                                     "external_id": null, "cwd": null });
        let mut scribe = view("vacant", "none");
        scribe["address"] = json!("seat:royalti-co/scribe");
        scribe["engine_id"] = json!("opencode");

        let out = render_seats(&json!([lead, nightly, scribe]), Format::Human, now);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].starts_with("seat:royalti-co/lead"));
        assert!(lines[0].contains("live"));
        assert!(lines[0].contains("terminal t-1 (agent live)"));
        assert_eq!(
            lines[1].trim(),
            "held by orchestrator since 2026-09-27 14:03Z"
        );
        assert!(lines[2].contains("run r-9"));
        assert_eq!(lines[3].trim(), "not resumable after restart");
        assert!(lines[4].starts_with("seat:royalti-co/scribe"));
        assert!(lines[4].contains("vacant") && lines[4].contains('—'));
        assert_eq!(lines[5].trim(), "can't resume sessions");
        assert_eq!(lines[6].trim(), "empty — the next send starts a session");
        assert_eq!(lines.len(), 7);
    }

    #[test]
    fn ls_hides_an_expired_hold_and_shows_queue_and_popout() {
        let now = 2_000;
        let mut v = view("run", "durable");
        v["hold"] = json!({ "client": "orch", "since": 0, "expires_at": 1_000 });
        v["queued"] = json!({ "since": 0 });
        v["mount"] = json!({ "window_label": "detached-2", "pane_ids": ["p1"] });
        let notes = seat_notes(&v, now);
        assert_eq!(
            notes,
            vec![
                "a text is queued since 1970-01-01 00:00Z — sends when its run finishes"
                    .to_string(),
                "popped out (detached-2)".to_string(),
            ]
        );
        let mut resumable = view("vacant", "durable");
        resumable["resume"] = json!({ "resumable": true });
        assert_eq!(
            seat_notes(&resumable, now),
            vec!["resumes its session on the next send"]
        );
        let mut gone = view("vacant", "durable");
        gone["resume"] = json!({ "resumable": false, "reason": "run_missing" });
        assert_eq!(
            seat_notes(&gone, now),
            vec!["can't resume (run_missing) — the next send starts a new session"]
        );
    }

    #[test]
    fn ls_empty_and_json() {
        assert_eq!(render_seats(&json!([]), Format::Human, 0), "(no seats)\n");
        assert_eq!(render_seats(&json!([]), Format::Json, 0), "[]\n");
    }

    #[test]
    fn send_results_use_the_toast_wording() {
        let seat = view("idle", "durable");
        let r = |extra: Value| {
            let mut v = json!({ "seat": seat.clone(), "run_id": "r-2" });
            v.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            render_send_result(&v, Format::Human)
        };
        assert_eq!(
            r(json!({ "route": "vacant", "outcome": "resumed" })),
            "@lead was vacant — resumed run r-2, then sent\n"
        );
        assert_eq!(
            r(json!({ "route": "vacant", "outcome": "started-fresh", "reason": "process_local" })),
            "@lead was vacant — its claude-code session can't resume after restart; started a new one (run r-2)\n"
        );
        assert_eq!(
            r(json!({ "route": "vacant", "outcome": "started-fresh", "reason": "no_resume_id" })),
            "@lead was vacant — its claude-code session can't be resumed; started a new one (run r-2)\n"
        );
        assert_eq!(
            r(json!({ "route": "vacant", "outcome": "started-fresh", "reason": "no_session" })),
            "@lead was vacant — filled with run r-2, then sent\n"
        );
        assert_eq!(
            r(json!({ "route": "chi-resume", "queued": true })),
            "Queued for @lead — sends when its run finishes (run r-2)\n"
        );
        assert_eq!(
            r(json!({ "route": "chi-resume" })),
            "sent to @lead (run r-2)\n"
        );
        assert_eq!(
            r(json!({ "route": "pty", "run_id": null, "terminal_id": "t-1" })),
            "sent to @lead (terminal t-1)\n"
        );
    }

    #[test]
    fn hold_refusals_say_who_since_when_and_how_to_claim() {
        let details = json!({ "client": "orch", "since": 1_790_517_780_000_i64, "expires_at": 0 });
        let msg = render_seat_error(
            "seat_held",
            "lead is held by orch since 1790517780000",
            Some(&details),
            409,
        );
        assert_eq!(
            msg,
            "seat_held: lead is held by orch since 2026-09-27 14:03Z; --takeover to claim it"
        );
        let details = json!({ "by": "orch", "at": 0 });
        let msg = render_seat_error(
            "seat_taken_over",
            "orch took over lead at 0",
            Some(&details),
            409,
        );
        assert!(msg.starts_with("seat_taken_over: orch took over lead at 1970-01-01 00:00Z"));
        assert!(msg.contains("--takeover"));
        assert_eq!(
            render_seat_error("seat_not_found", "that seat no longer exists", None, 404),
            "seat_not_found (HTTP 404): that seat no longer exists"
        );
    }

    #[test]
    fn seat_error_reads_the_structured_body_and_passes_others_through() {
        let structured = anyhow::Error::new(HttpStatusError {
            path: "/iyke/seats/fill".into(),
            status: 409,
            body: json!({ "code": "needs_prompt", "message": "fill needs a prompt" }).to_string(),
        });
        assert_eq!(
            seat_error(structured).to_string(),
            "needs_prompt: fill needs a prompt"
        );

        let plain = anyhow::Error::new(HttpStatusError {
            path: "/iyke/seats/list".into(),
            status: 404,
            body: "Not Found".into(),
        });
        assert_eq!(
            seat_error(plain).to_string(),
            "/iyke/seats/list returned HTTP 404: Not Found"
        );
        let other = anyhow!("could not reach iyke server");
        assert_eq!(seat_error(other).to_string(), "could not reach iyke server");
    }
}
