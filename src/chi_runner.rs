//! chi-runner — pipe-preserving engine runner daemon (WP-09).
//!
//! Spawned detached by the Ikenga shell (via its SessionExecutor) so the
//! engine child survives an app restart. Reads its config from a JSON file at
//! startup, runs the engine process with Stdio::piped() (not a PTY), and
//! writes structured output to the chi-cache JSON status file.
//!
//! Invocation (by the shell, detached — no terminal multiplexer, no PTY):
//!   IKENGA_CHI_CONF=<path> chi-runner
//!
//! The config file path is read from IKENGA_CHI_CONF.
//!
//! Status file: every update is written atomically (sibling temp file +
//! rename), so readers — the shell's `GET /iyke/chi/status` and
//! `iyke chi attach` — never observe a partially-written JSON document.
//! Status vocabulary written here: `running`, `done`, `failed`, `timed_out`.
//! Exit codes:
//!   0 = done (success)
//!   1 = done (failed / error from engine)
//!   2 = config or spawn error

use std::{
    env,
    fs,
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{self, Command, Stdio},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

// ── Config ────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct RunnerConf {
    run_id: String,
    engine_id: String,
    prompt: String,
    cwd: String,
    model: Option<String>,
    mode: Option<String>,
    resume_session_id: Option<String>,
    output_path: String,
    /// Seconds before the runner self-terminates. Default 3600.
    timeout_seconds: Option<u64>,
}

// ── Output file (mirrors Tauri RunOutputFile) ─────────────────────────────────

#[derive(Default, Serialize)]
struct OutputFile {
    output: Option<String>,
    error: Option<String>,
    done_at: Option<String>,
    status: Option<String>,
    external_id: Option<String>,
}

fn write_output(path: &str, f: &OutputFile) {
    if let Ok(json) = serde_json::to_string(f) {
        let _ = write_atomic(Path::new(path), json.as_bytes());
    }
}

/// Write `bytes` to `target` atomically: write a sibling temp file in the same
/// directory (`<target>.tmp.<pid>`), then `rename` it over the target. On
/// POSIX `rename(2)` within one filesystem is atomic; on Windows
/// `std::fs::rename` uses `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`, which
/// replaces an existing target. Readers therefore see either the previous
/// complete document or the new one, never a torn write. The temp file is
/// removed if any step fails.
fn write_atomic(target: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut tmp_name = target.as_os_str().to_owned();
    tmp_name.push(format!(".tmp.{}", process::id()));
    let tmp = PathBuf::from(tmp_name);

    let result = fs::write(&tmp, bytes).and_then(|()| fs::rename(&tmp, target));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn now_iso() -> String {
    // Minimal ISO-8601 without chrono dep.
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (y, mo, d, h, mi, s) = epoch_to_ymd(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

fn epoch_to_ymd(secs: u64) -> (u64, u64, u64, u64, u64, u64) {
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let days = secs / 86400;
    // Rough Gregorian — good enough for a log timestamp.
    let y400 = days / 146097;
    let r = days % 146097;
    let y100 = r / 36524;
    let r2 = r % 36524;
    let y4 = r2 / 1461;
    let r3 = r2 % 1461;
    let y1 = r3 / 365;
    let doy = r3 % 365;
    let year = y400 * 400 + y100 * 100 + y4 * 4 + y1 + 1970;
    let (mo, d) = doy_to_md(doy, is_leap(year));
    (year, mo, d, h, m, s)
}

fn is_leap(y: u64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn doy_to_md(doy: u64, leap: bool) -> (u64, u64) {
    let months: &[u64] = if leap {
        &[31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        &[31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut rem = doy;
    for (i, &days) in months.iter().enumerate() {
        if rem < days {
            return (i as u64 + 1, rem + 1);
        }
        rem -= days;
    }
    (12, 31)
}

// ── Engine runner ─────────────────────────────────────────────────────────────

/// The daemon's Chi mode id -> claude's `--permission-mode`. Must match the
/// daemon's in-process mapping (`acp_mode.rs` `as_claude_flag`): the daemon's
/// `auto` is claude's `acceptEdits`. Claude also has its own, broader `auto`
/// mode, so passing `auto` through unchanged gave detached runs a wider mode
/// than the daemon (and its audit) recorded. Unknown ids fail closed to
/// `default`.
fn claude_permission_mode(mode: Option<&str>) -> &'static str {
    match mode.unwrap_or("default") {
        "bypassPermissions" => "bypassPermissions",
        "auto" | "acceptEdits" => "acceptEdits",
        "plan" => "plan",
        _ => "default",
    }
}

fn build_command(conf: &RunnerConf) -> Result<Command, String> {
    match conf.engine_id.as_str() {
        "claude-code" => {
            let perm_mode = claude_permission_mode(conf.mode.as_deref());
            let mut cmd = Command::new("claude");
            cmd.arg("--permission-prompt-tool")
                .arg("stdio")
                .arg("--permission-mode")
                .arg(perm_mode)
                .arg("--print")
                .arg("--input-format")
                .arg("stream-json")
                .arg("--output-format")
                .arg("stream-json")
                .arg("--verbose")
                .current_dir(&conf.cwd)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if let Some(id) = &conf.resume_session_id {
                cmd.arg("--resume").arg(id);
            }
            if let Some(m) = &conf.model {
                cmd.arg("--model").arg(m);
            }
            Ok(cmd)
        }
        "antigravity-cli" => {
            let mut cmd = Command::new("agy");
            cmd.arg("-p")
                .arg(&conf.prompt)
                .arg("--output-format")
                .arg("stream-json")
                .current_dir(&conf.cwd)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if let Some(id) = &conf.resume_session_id {
                cmd.arg("--conversation").arg(id);
            }
            if let Some(m) = &conf.model {
                cmd.arg("--model").arg(m);
            }
            if let Some(mo) = &conf.mode {
                cmd.arg("--mode").arg(mo);
            }
            Ok(cmd)
        }
        // Codex: `codex exec --json` (new) or `codex exec resume <id> --json`.
        // `-` means "read prompt from stdin". `--skip-git-repo-check` avoids
        // refusing to run outside a git repo (a common setup in ci / agent dirs).
        "codex" => {
            let mut cmd = Command::new("codex");
            if let Some(id) = &conf.resume_session_id {
                cmd.args(["exec", "resume", id.as_str(), "--json"]);
            } else {
                cmd.args(["exec", "--json"]);
            }
            cmd.args(["--skip-git-repo-check", "--cd", &conf.cwd, "-"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if let Some(m) = &conf.model {
                cmd.arg("--model").arg(m);
            }
            Ok(cmd)
        }
        other => Err(format!("engine not supported by chi-runner: {other}")),
    }
}

/// Write the Claude stream-json prompt envelope to stdin.
fn claude_prompt_envelope(prompt: &str) -> String {
    // Mirrors the envelope format used in the Tauri claude_one_off_task.
    serde_json::json!({
        "type": "user",
        "message": { "role": "user", "content": prompt }
    })
    .to_string()
        + "\n"
}

fn run(conf: RunnerConf) -> i32 {
    let timeout = Duration::from_secs(conf.timeout_seconds.unwrap_or(3600));
    let started = Instant::now();
    let output_path = conf.output_path.clone();

    let mut state = OutputFile {
        status: Some("running".into()),
        ..Default::default()
    };
    write_output(&output_path, &state);

    // Build + spawn.
    let mut cmd = match build_command(&conf) {
        Ok(c) => c,
        Err(e) => {
            state.status = Some("failed".into());
            state.error = Some(e);
            state.done_at = Some(now_iso());
            write_output(&output_path, &state);
            return 2;
        }
    };

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            state.status = Some("failed".into());
            state.error = Some(format!("spawn failed: {e}"));
            state.done_at = Some(now_iso());
            write_output(&output_path, &state);
            return 2;
        }
    };

    // Send prompt to stdin for Claude (closed after write so Claude knows
    // no more input is coming for this turn).
    if conf.engine_id == "claude-code" {
        if let Some(mut stdin) = child.stdin.take() {
            let envelope = claude_prompt_envelope(&conf.prompt);
            let _ = stdin.write_all(envelope.as_bytes());
            let _ = stdin.flush();
            // Drop stdin → EOF on the child side.
        }
    } else if conf.engine_id == "codex" {
        // Codex reads the plain prompt text from stdin; no JSON envelope.
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(conf.prompt.as_bytes());
            let _ = stdin.flush();
            // Drop stdin → EOF so codex knows the prompt is complete.
        }
    } else {
        // antigravity-cli gets the prompt via --prompt flag; close stdin.
        drop(child.stdin.take());
    }

    // Read stdout.
    let stdout = match child.stdout.take() {
        Some(s) => s,
        None => {
            state.status = Some("failed".into());
            state.error = Some("stdout pipe missing".into());
            state.done_at = Some(now_iso());
            write_output(&output_path, &state);
            return 2;
        }
    };

    let mut output = String::new();
    let mut external_id: Option<String> = None;
    let mut saw_done = false;

    let reader = BufReader::new(stdout);
    for line in reader.lines() {
        // Timeout check.
        if started.elapsed() > timeout {
            state.status = Some("timed_out".into());
            state.error = Some(format!("timed out after {}s", timeout.as_secs()));
            state.output = Some(output.clone());
            state.done_at = Some(now_iso());
            write_output(&output_path, &state);
            let _ = child.kill();
            return 1;
        }

        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };

        // Parse stream-json events.
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) {
            // Claude stream-json events.
            if let Some(t) = val.get("type").and_then(|v| v.as_str()) {
                match t {
                    "system" => {
                        if let Some(id) = val.get("session_id").and_then(|v| v.as_str()) {
                            if external_id.is_none() {
                                external_id = Some(id.to_string());
                                state.external_id = Some(id.to_string());
                            }
                        }
                    }
                    "assistant" => {
                        if let Some(content) = val
                            .get("message")
                            .and_then(|m| m.get("content"))
                            .and_then(|c| c.as_array())
                        {
                            for item in content {
                                if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                                    if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                                        output.push_str(text);
                                    }
                                }
                            }
                        }
                    }
                    "result" => {
                        saw_done = true;
                    }
                    // Codex JSONL events (keyed by `type` as well).
                    "thread.started" => {
                        if let Some(id) = val.get("thread_id").and_then(|v| v.as_str()) {
                            if external_id.is_none() {
                                external_id = Some(id.to_string());
                                state.external_id = Some(id.to_string());
                            }
                        }
                    }
                    "turn.completed" => {
                        saw_done = true;
                    }
                    "turn.failed" => {
                        saw_done = true;
                        if let Some(msg) = val
                            .get("error")
                            .and_then(|e| e.get("message"))
                            .and_then(|v| v.as_str())
                        {
                            output.push_str(&format!("[error] {msg}\n"));
                        }
                    }
                    // Codex item events: extract agent_message text.
                    "item.completed" | "item.updated" => {
                        if let Some(item) = val.get("item").and_then(|v| v.as_object()) {
                            let itype = item
                                .get("item_type")
                                .or_else(|| item.get("type"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            if itype == "agent_message" {
                                if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                                    if !text.is_empty() {
                                        output.push_str(text);
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            // Antigravity stream-json events.
            if let Some(ev) = val.get("event").and_then(|v| v.as_str()) {
                match ev {
                    "init" => {
                        if let Some(id) = val.get("conversation_id").and_then(|v| v.as_str()) {
                            if external_id.is_none() {
                                external_id = Some(id.to_string());
                                state.external_id = Some(id.to_string());
                            }
                        }
                    }
                    "step_update" => {
                        if let Some(su) = val.get("step_update") {
                            if su.get("step_type").and_then(|t| t.as_str()) == Some("agent_response") {
                                if let Some(delta) = su.get("text_delta").and_then(|d| d.as_str()) {
                                    output.push_str(delta);
                                }
                            }
                        }
                    }
                    "result" => {
                        saw_done = true;
                    }
                    _ => {}
                }
            }
        }

        // Flush partial output periodically.
        state.output = Some(output.clone());
        write_output(&output_path, &state);
    }

    let exit_status = child.wait().ok();
    let exit_code = exit_status.and_then(|s| s.code()).unwrap_or(-1);

    state.output = Some(output);
    state.done_at = Some(now_iso());

    if saw_done && exit_code == 0 {
        state.status = Some("done".into());
        write_output(&output_path, &state);
        0
    } else {
        state.status = Some("failed".into());
        state.error = Some(format!("engine exited with code {exit_code}"));
        write_output(&output_path, &state);
        1
    }
}

fn main() {
    let conf_path = match env::var("IKENGA_CHI_CONF") {
        Ok(p) => p,
        Err(_) => {
            eprintln!("chi-runner: IKENGA_CHI_CONF not set");
            process::exit(2);
        }
    };

    let conf_str = match fs::read_to_string(&conf_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("chi-runner: cannot read conf {conf_path}: {e}");
            process::exit(2);
        }
    };

    let conf: RunnerConf = match serde_json::from_str(&conf_str) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("chi-runner: invalid conf JSON: {e}");
            process::exit(2);
        }
    };

    eprintln!("chi-runner: starting run_id={} engine={}", conf.run_id, conf.engine_id);

    let code = run(conf);
    process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("chi-runner-test-{name}-{}", process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn tmp_siblings(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp."))
            .collect()
    }

    #[test]
    fn daemon_auto_maps_to_claude_accept_edits_not_claude_auto() {
        assert_eq!(claude_permission_mode(Some("auto")), "acceptEdits");
        assert_eq!(claude_permission_mode(Some("acceptEdits")), "acceptEdits");
        assert_eq!(claude_permission_mode(Some("plan")), "plan");
        assert_eq!(
            claude_permission_mode(Some("bypassPermissions")),
            "bypassPermissions"
        );
        assert_eq!(claude_permission_mode(None), "default");
        assert_eq!(claude_permission_mode(Some("Auto")), "default");
        assert_eq!(claude_permission_mode(Some("manual")), "default");
    }

    #[test]
    fn build_command_passes_accept_edits_for_daemon_auto() {
        let conf = RunnerConf {
            run_id: "r".into(),
            engine_id: "claude-code".into(),
            prompt: "p".into(),
            cwd: ".".into(),
            model: None,
            mode: Some("auto".into()),
            resume_session_id: None,
            output_path: "o".into(),
            timeout_seconds: None,
        };
        let cmd = build_command(&conf).unwrap();
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let i = args.iter().position(|a| a == "--permission-mode").unwrap();
        assert_eq!(args[i + 1], "acceptEdits");
    }

    #[test]
    fn write_atomic_replaces_target_and_removes_temp() {
        let dir = scratch_dir("replace");
        let target = dir.join("run.json");
        write_atomic(&target, b"{\"a\":1}").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "{\"a\":1}");
        write_atomic(&target, b"{\"a\":2}").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "{\"a\":2}");
        assert!(tmp_siblings(&dir).is_empty(), "temp file left behind");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_atomic_cleans_up_temp_on_rename_failure() {
        let dir = scratch_dir("fail");
        // A non-empty directory at the target path makes rename fail on every
        // platform, after the temp file has already been written.
        let target = dir.join("run.json");
        fs::create_dir_all(target.join("occupied")).unwrap();
        assert!(write_atomic(&target, b"{}").is_err());
        assert!(
            tmp_siblings(&dir).is_empty(),
            "temp file left behind on error"
        );
        assert!(target.is_dir(), "target must be untouched on error");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_output_never_exposes_a_partial_document() {
        let dir = scratch_dir("torn");
        let target = dir.join("run.json");
        let path = target.to_string_lossy().into_owned();

        // Large, growing payloads make a torn non-atomic write overwhelmingly
        // likely to be observed by the concurrent reader.
        let mut state = OutputFile {
            status: Some("running".into()),
            ..Default::default()
        };
        write_output(&path, &state);

        let stop = Arc::new(AtomicBool::new(false));
        let reader = {
            let stop = Arc::clone(&stop);
            let target = target.clone();
            std::thread::spawn(move || {
                let mut reads = 0u32;
                while !stop.load(Ordering::Relaxed) {
                    let s = fs::read_to_string(&target).expect("target must always exist");
                    let v: serde_json::Value = serde_json::from_str(&s).unwrap_or_else(|e| {
                        panic!("observed partial document ({e}): {} bytes", s.len())
                    });
                    assert!(v.get("status").is_some());
                    reads += 1;
                }
                reads
            })
        };

        let mut output = String::new();
        for i in 0..150 {
            output.push_str(&"x".repeat(4096));
            output.push_str(&i.to_string());
            state.output = Some(output.clone());
            write_output(&path, &state);
        }
        state.status = Some("done".into());
        write_output(&path, &state);

        stop.store(true, Ordering::Relaxed);
        let reads = reader.join().expect("reader observed a partial document");
        assert!(reads > 0);
        assert!(tmp_siblings(&dir).is_empty(), "temp file left behind");
        let final_v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&target).unwrap()).unwrap();
        assert_eq!(final_v["status"], "done");
        fs::remove_dir_all(&dir).unwrap();
    }
}
