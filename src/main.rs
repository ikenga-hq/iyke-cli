//! `iyke` — CLI for the Ikenga desktop app's localhost control bridge.
//!
//! The desktop app exposes an HTTP server bound to `127.0.0.1:<random>`
//! with a per-launch bearer token. The token + port are written to a
//! control file under the user's local data dir; this CLI reads that
//! file and proxies subcommands as authenticated HTTP calls.
//!
//! Subcommands roughly mirror what the in-app FE can do via keyboard
//! shortcuts: navigate the focused pane, switch sidebar mode, open new
//! tabs, split/focus/close panes.

mod api;
mod cmd;
mod control;
mod output;

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{json, Value};

use crate::api::Client;
use crate::control::{LoadOutcome, STALE_THRESHOLD_SECS};
use crate::output::{print_state, print_write_result, Format};

#[derive(Parser)]
#[command(
    name = "iyke",
    version,
    about = "Control the Ikenga desktop app from outside the webview.",
    long_about = "iyke talks to the localhost control bridge that the Ikenga desktop app exposes. \
                  Use it to navigate panes, switch sidebar modes, open tabs, and inspect state \
                  from a terminal or script."
)]
struct Cli {
    /// Emit JSON instead of human-readable output.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the current shell state (mode, windows, terminals, focused route, app info).
    State,

    /// List kernel-authoritative terminal records, including logical and PTY ids.
    Terminals,

    /// List all primary and detached windows.
    Windows,

    /// Navigate the focused pane to a route path (e.g. `/finance/receivables`).
    Go {
        /// Route path. Must start with `/`.
        path: String,
    },

    /// Switch the sidebar activity mode.
    ///
    /// The v16 rail exposes exactly four modes — `project`, `chi`, `ngwa`,
    /// `settings`. The bridge still accepts the legacy names (`app`,
    /// `files`, `sessions`, `pkgs`, …) for one compatibility release and
    /// normalizes them shell-side; new automation should use the v16 set.
    Mode {
        /// One of: project, chi, ngwa, settings.
        mode: String,
    },

    /// Open a new tab in the focused pane.
    Open {
        #[command(subcommand)]
        kind: OpenKind,
    },

    /// Split the focused (or specified) pane.
    Split {
        /// Direction.
        direction: SplitDirection,
        /// Optional pane id to split. Defaults to focused.
        #[arg(long)]
        pane_id: Option<String>,
    },

    /// Focus a pane by id or DFS leaf index (1-based, matching ⌃1..⌃6).
    Focus {
        #[command(subcommand)]
        target: FocusTarget,
    },

    /// Close a pane (or the focused pane if id omitted).
    Close {
        /// Optional pane id. Defaults to focused.
        #[arg(long)]
        pane_id: Option<String>,
    },

    /// Resize the main window. Pass either `<W>x<H>` (e.g. `1600x1000`) or
    /// a preset: `maximize`, `unmaximize`, `fullscreen`, `unfullscreen`,
    /// `minimize`.
    Resize {
        /// `<W>x<H>` or a preset name.
        target: String,
    },

    /// Refresh a pane's content (re-mount via React key bump). Defaults to
    /// the focused pane.
    Refresh {
        /// Optional pane id. Defaults to focused.
        #[arg(long)]
        pane_id: Option<String>,
    },

    /// Print an accessibility-tree snapshot of the focused pane (or `--pane`).
    /// Refs (e.g. `e3`) are stable until the next snapshot or navigation.
    Dom {
        /// Substring filter; only entries whose role/name/value match are kept.
        #[arg(long)]
        query: Option<String>,
        /// Include hidden + aria-hidden + zero-size elements.
        #[arg(long)]
        all: bool,
        /// Pane id. Default = focused.
        #[arg(long)]
        pane: Option<String>,
    },

    /// Print recent console + error logs from the running webview.
    Logs {
        /// Filter by level: log | info | warn | error | debug.
        #[arg(long)]
        level: Option<String>,
        /// Only entries with `ts >= since` (epoch ms).
        #[arg(long)]
        since: Option<u128>,
        /// Filter by source pane id (e.g. `shell` or a leaf id).
        #[arg(long)]
        source: Option<String>,
    },

    /// Print recent fetch + XHR network activity (last 100).
    Network {
        #[arg(long)]
        since: Option<u128>,
        #[arg(long)]
        source: Option<String>,
    },

    /// Capture a screenshot of the focused pane or the full window.
    Screenshot {
        /// Capture target.
        #[arg(value_enum, default_value = "window")]
        target: ScreenshotTarget,
        /// Output path. Default: ~/.local/share/ikenga/screenshots/<auto>.png.
        #[arg(long)]
        out: Option<String>,
        /// Pane id when target=pane.
        #[arg(long)]
        pane_id: Option<String>,
    },

    /// Wait until a predicate is satisfied or timeout. Exit non-zero on timeout.
    Wait {
        /// Predicate kind: text | selector | ref | gone-text | gone-selector.
        kind: String,
        /// Predicate value.
        value: String,
        /// Timeout in milliseconds (default 10000, max 60000).
        #[arg(long)]
        timeout_ms: Option<u64>,
        #[arg(long)]
        pane: Option<String>,
    },

    /// Click an element. Specify exactly one of `--ref`, `--selector`, `--text`.
    Click {
        #[arg(long)]
        r#ref: Option<String>,
        #[arg(long)]
        selector: Option<String>,
        #[arg(long)]
        text: Option<String>,
        #[arg(long)]
        pane: Option<String>,
    },

    /// Type text into an input/textarea/contenteditable.
    Type {
        /// Text to type.
        text: String,
        #[arg(long)]
        r#ref: Option<String>,
        #[arg(long)]
        selector: Option<String>,
        /// Replace the existing value instead of appending.
        #[arg(long)]
        replace: bool,
        #[arg(long)]
        pane: Option<String>,
    },

    /// Dispatch a key combo (e.g. `Enter`, `Ctrl+S`, `Meta+K`).
    Key {
        /// Combo string. `+` or `,` separated.
        combo: String,
        #[arg(long)]
        r#ref: Option<String>,
        #[arg(long)]
        selector: Option<String>,
        #[arg(long)]
        pane: Option<String>,
    },

    /// Read captured PTY output for a terminal pane. xterm.js renders to a
    /// canvas that screenshots can't reliably capture on WebKitGTK; this
    /// command reads from a ring buffer the shell tees off each PTY's data
    /// stream. ANSI/VT escapes are stripped by default for readability.
    #[command(name = "terminal-read")]
    TerminalRead {
        /// Leaf id of the terminal pane. Defaults to the focused pane.
        #[arg(long, conflicts_with = "session")]
        pane: Option<String>,
        /// Deprecated compatibility alias. Accepts a logical terminal id or PTY id.
        #[arg(long)]
        session: Option<String>,
        /// Stable logical terminal id.
        #[arg(long)]
        terminal: Option<String>,
        /// Human-assigned terminal label.
        #[arg(long)]
        label: Option<String>,
        /// Return only output after this absolute stream offset.
        #[arg(long)]
        after: Option<u64>,
        /// Read mode: stream (default) or screen (VT-emulated current screen).
        #[arg(long, default_value = "stream")]
        mode: String,
        /// Tail size in bytes. Default returns the entire buffer (per-session
        /// cap is 256 KiB).
        #[arg(long)]
        bytes: Option<usize>,
        /// Return raw bytes (including ANSI/VT escapes). Default strips them.
        #[arg(long)]
        raw: bool,
        /// Require stable terminal/label/session addressing so pane focus cannot change.
        #[arg(long)]
        no_focus: bool,
    },

    /// Write to a terminal pane's PTY. Provide `text` (raw bytes — backslash
    /// escapes like `\n`, `\t`, `\r`, `\x1b`, `\\` are interpreted), one or
    /// more `--key` chords (translated to terminal escapes — `Enter` → `\r`,
    /// `Ctrl+C` → `\x03`, `Up`/`Down`/`Left`/`Right` → CSI arrows, `F1`-`F12`
    /// → SS3/CSI), or both (text first, then keys in order).
    ///
    /// Examples:
    ///   iyke terminal-send "cd ikenga" --key Enter
    ///   iyke terminal-send --pane <leafId> --key Ctrl+C
    ///   iyke terminal-send "echo hi" --key Enter --key Up
    #[command(name = "terminal-send")]
    TerminalSend {
        /// Raw text to write (optional if at least one `--key` is given).
        text: Option<String>,
        /// Key combo to append after `text`. Repeatable.
        #[arg(long = "key")]
        keys: Vec<String>,
        /// Leaf id of the terminal pane. Defaults to the focused pane.
        #[arg(long, conflicts_with = "session")]
        pane: Option<String>,
        /// Deprecated compatibility alias. Accepts a logical terminal id or PTY id.
        #[arg(long)]
        session: Option<String>,
        /// Stable logical terminal id.
        #[arg(long)]
        terminal: Option<String>,
        /// Human-assigned terminal label.
        #[arg(long)]
        label: Option<String>,
        /// Refuse the write if the logical terminal now points at a different PTY.
        #[arg(long)]
        expected_pty_id: Option<String>,
        /// Agent identity recorded in the terminal audit ring.
        #[arg(long)]
        actor: Option<String>,
        /// Token returned by `terminal-lease acquire`.
        #[arg(long)]
        lease_token: Option<String>,
        /// Validate and report the target without writing bytes.
        #[arg(long)]
        dry_run: bool,
        /// Explicitly assert that pane focus must not change. Direct targets always satisfy this.
        #[arg(long)]
        no_focus: bool,
    },

    /// Wait for fresh terminal output to match a regex or become idle.
    #[command(name = "terminal-wait")]
    TerminalWait {
        #[arg(long)]
        terminal: Option<String>,
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        r#match: Option<String>,
        #[arg(long)]
        until_idle_ms: Option<u64>,
        #[arg(long, default_value_t = 0)]
        after: u64,
        #[arg(long, default_value_t = 10_000)]
        timeout_ms: u64,
        #[arg(long)]
        raw: bool,
    },

    /// Spawn a new terminal tab and print its descriptor.
    ///
    /// The tab is an ordinary one — visible in the pane tree, poppable,
    /// and takeable over by a human. Pass `--lease-for` to own it from
    /// birth, closing the window between spawn and a separate lease call
    /// in which anyone else could write to it.
    #[command(name = "terminal-spawn")]
    TerminalSpawn {
        /// Working directory. Defaults to the shell's active-project cwd.
        #[arg(long)]
        cwd: Option<String>,
        /// Tab title.
        #[arg(long)]
        title: Option<String>,
        /// Unique label to assign once the pty exists, so later commands
        /// can address this terminal by role rather than by id.
        #[arg(long)]
        label: Option<String>,
        /// Target pane leaf id. Defaults to the focused pane.
        #[arg(long)]
        pane: Option<String>,
        /// Acquire a writer lease for this agent id and print the token.
        #[arg(long)]
        lease_for: Option<String>,
        /// Lease TTL. Only meaningful with --lease-for.
        #[arg(long, requires = "lease_for")]
        lease_ttl_ms: Option<u64>,
        /// Command + args to run, after `--`. Defaults to the login shell.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        argv: Vec<String>,
    },

    /// Kill a terminal's process. Addressed by terminal id, pty id, or label.
    ///
    /// The tab stays by default, showing its exit status — the same thing
    /// that happens when a shell exits on its own, and it keeps the
    /// scrollback readable for a post-mortem. Pass --close-tab to remove it.
    #[command(name = "terminal-kill")]
    TerminalKill {
        terminal: String,
        #[arg(long)]
        close_tab: bool,
    },

    /// Assign or clear a human-readable terminal label.
    #[command(name = "terminal-label")]
    TerminalLabel {
        terminal: String,
        label: Option<String>,
        #[arg(long)]
        clear: bool,
    },

    /// Acquire or release an enforced terminal writer lease.
    #[command(name = "terminal-lease")]
    TerminalLease {
        #[command(subcommand)]
        action: TerminalLeaseAction,
    },

    /// Print the bounded terminal control audit ring.
    #[command(name = "terminal-audit")]
    TerminalAudit,

    /// Register an agent id with the shell.
    ///
    /// Scheduling a timer against an unregistered id is rejected, so this
    /// is the first call in most orchestration scripts.
    Agent {
        #[command(subcommand)]
        action: AgentAction,
    },

    /// Read and acknowledge the agent inbox that due timers fire into.
    Inbox {
        #[command(subcommand)]
        action: InboxAction,
    },

    /// The durable task board, shared with the human.
    ///
    /// Distinct from the per-run coordination todos: tasks outlive a
    /// single agent run and are what the human actually works from.
    Task {
        #[command(subcommand)]
        action: TaskAction,
    },

    /// Activate an existing pane tab without focusing the pane.
    Tab {
        #[command(subcommand)]
        action: TabAction,
    },

    /// Dump the TanStack Query cache: keys, statuses, last update times.
    QueryCache {
        #[arg(long)]
        pane: Option<String>,
    },

    /// Open Chrome DevTools (debug builds only).
    Devtools,

    /// Read the latest published state object for an iframe pane (comp
    /// current frame, etc.). Iframes publish via the bridge's
    /// `publishState(key, value)` API.
    IframeState {
        /// Pane id (from `iyke state` shell.panes.leaves[].id).
        pane: String,
    },

    /// Send a fire-and-forget postMessage to an iframe pane. Used to drive
    /// mini-app actions from the terminal — e.g.
    /// `iyke iframe-send <pane> story-select '{"beatId":"hook"}'`.
    IframeSend {
        /// Pane id.
        pane: String,
        /// Message kind. Up to the iframe to interpret.
        kind: String,
        /// JSON payload. Defaults to null.
        #[arg(default_value = "null")]
        payload: String,
    },

    /// Read and update shared scratchpads through the authenticated bridge.
    Scratchpad {
        #[command(subcommand)]
        action: ScratchpadAction,
    },

    /// pkg-browser: drive native child webviews (e.g. partner portals
    /// like Spotify-for-Artists / Bandcamp). Mirrors the
    /// `@ikenga/mcp-browser` MCP server's tools; useful for scripting,
    /// debugging, and CI flows where MCP isn't appropriate. By default
    /// the CLI acts as the `com.ikenga.mcp-browser` pkg — that pkg's
    /// manifest already declares `capabilities.webview` with wildcard
    /// partitions. Override with `--pkg-id` if you've installed a
    /// different webview-capable pkg you'd like the CLI to drive.
    Browser {
        /// Pkg id whose webview capability the CLI piggybacks on.
        /// Defaults to `com.ikenga.mcp-browser`.
        #[arg(long, global = true, default_value = "com.ikenga.mcp-browser")]
        pkg_id: String,
        #[command(subcommand)]
        action: BrowserAction,
    },

    /// Run, resume, list, and cancel agent runs (chi-first agent surface).
    Chi {
        #[command(subcommand)]
        action: ChiAction,
    },

    /// The Project noun (WP-21b): show and switch the shell's active
    /// project, and list the Explorer sidebar's registered sections.
    Project {
        #[command(subcommand)]
        action: cmd::project::ProjectAction,
    },

    /// The Ngwa noun (WP-21b): the unified equipment catalogue — installed
    /// pkgs + Ọba-placed primitives + engine config — that the `/ngwa/*`
    /// surfaces render. Reads the bridged `ngwa_snapshot` payload.
    Ngwa {
        #[command(subcommand)]
        action: cmd::ngwa::NgwaAction,
    },

    /// The Actions noun (WP-62): list, upsert and import user actions
    /// (G-ACTIONS). Writes round-trip through the running shell into the
    /// same `saveUserAction` the D-06 Editor tab calls.
    Actions {
        #[command(subcommand)]
        action: cmd::actions::ActionsAction,
    },

    /// The Menus noun (WP-62): show one effective menu (G-ACTIONS §1.3).
    Menus {
        #[command(subcommand)]
        action: cmd::menus::MenusAction,
    },

    /// The Keys noun (WP-62): list and add keybindings, and query "what
    /// fires here" for a key sequence.
    Keys {
        #[command(subcommand)]
        action: cmd::keys::KeysAction,
    },
}

#[derive(Subcommand)]
enum AgentAction {
    /// Register (or refresh) an agent. Prints the id the shell recorded —
    /// pass `--id` to choose it yourself, otherwise the shell mints one.
    Register {
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        name: String,
        #[arg(long)]
        model: Option<String>,
    },
}

#[derive(Subcommand)]
enum InboxAction {
    /// List entries for an agent, oldest first.
    ///
    /// `next_since` in the response is the cursor: feed it back as
    /// `--since` to get only what arrived after this batch.
    List {
        #[arg(long)]
        agent_id: String,
        /// Exclusive cursor — only entries created after this timestamp.
        #[arg(long, default_value_t = 0)]
        since: i64,
        #[arg(long)]
        limit: Option<i64>,
        /// Acknowledge everything listed once it has been printed, so a
        /// plain re-run returns only new entries. Acknowledging DELETES:
        /// the entries are gone after this, cursor or not.
        #[arg(long)]
        ack: bool,
    },
    /// Acknowledge entries by id, deleting them from the queue.
    Ack {
        #[arg(long = "id", value_name = "ID", required = true)]
        ids: Vec<String>,
    },
}

#[derive(Subcommand)]
enum TaskAction {
    /// List tasks, most recently touched first.
    List {
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        assigned_to: Option<String>,
        #[arg(long)]
        project_path: Option<String>,
        #[arg(long)]
        limit: Option<i64>,
    },
    /// Create a task. Prints the new id.
    Create {
        title: String,
        #[arg(long)]
        description: Option<String>,
        /// Defaults to `pending` shell-side.
        #[arg(long)]
        status: Option<String>,
        /// Defaults to `medium` shell-side.
        #[arg(long)]
        priority: Option<String>,
        #[arg(long)]
        assigned_to: Option<String>,
        #[arg(long)]
        project_path: Option<String>,
        #[arg(long)]
        due_date: Option<String>,
        /// Recorded on the task event. Defaults to `iyke` shell-side.
        #[arg(long)]
        actor: Option<String>,
    },
    /// Update a task's fields. At least one field is required.
    Update {
        id: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        description: Option<String>,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        priority: Option<String>,
        #[arg(long)]
        assigned_to: Option<String>,
        #[arg(long)]
        progress_pct: Option<i64>,
        #[arg(long)]
        task_result: Option<String>,
        #[arg(long)]
        outcome_notes: Option<String>,
        #[arg(long)]
        actor: Option<String>,
    },
    /// Mark a task completed, stamping completed_at.
    Complete {
        id: String,
        #[arg(long)]
        task_result: Option<String>,
        #[arg(long)]
        outcome_notes: Option<String>,
        #[arg(long)]
        actor: Option<String>,
    },
}

#[derive(Subcommand)]
enum TerminalLeaseAction {
    Acquire {
        terminal: String,
        #[arg(long)]
        agent_id: String,
        #[arg(long, default_value_t = 60_000)]
        ttl_ms: u64,
    },
    Release {
        terminal: String,
        #[arg(long)]
        token: String,
    },
}

#[derive(Subcommand)]
enum TabAction {
    Activate {
        #[arg(long)]
        pane: String,
        #[arg(long)]
        index: Option<usize>,
        #[arg(long)]
        terminal: Option<String>,
    },
}

#[derive(Subcommand)]
enum ScratchpadAction {
    /// Read a scratchpad.
    Get {
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        name: String,
    },
    /// Replace a scratchpad's body.
    Set {
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        name: String,
        #[arg(
            long,
            conflicts_with = "body_file",
            required_unless_present = "body_file"
        )]
        body: Option<String>,
        #[arg(long, value_name = "PATH")]
        body_file: Option<String>,
    },
    /// Append to a scratchpad.
    Append {
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        name: String,
        #[arg(
            long,
            conflicts_with = "body_file",
            required_unless_present = "body_file"
        )]
        body: Option<String>,
        #[arg(long, value_name = "PATH")]
        body_file: Option<String>,
        #[arg(long)]
        no_separator: bool,
    },
    /// List scratchpads in a scope.
    List {
        #[arg(long)]
        scope: Option<String>,
    },
    /// Print the current body and subsequent updates until interrupted.
    Watch {
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        name: String,
        #[arg(long, default_value_t = 30_000)]
        wait_ms: u64,
    },
}

#[derive(Subcommand)]
enum BrowserAction {
    /// Open a child webview pane navigated to `url`. `<pane_id>` is an
    /// opaque handle you choose (e.g. `spotify`); pass it to subsequent
    /// commands. Use `--session <name>` to bind to a named cookie jar
    /// (`iyke browser session create` first), or `--partition <slug>`
    /// for a raw jar.
    ///
    /// For `--engine chrome` in attach mode, `--attach-target` controls
    /// what the engine drives:
    ///   `new`    — open a fresh tab (default; does not disturb open tabs)
    ///   `active` — adopt the first open tab (today's behavior; navigates if --url given)
    ///   `<id>`   — adopt a specific tab by CDP target id or URL substring
    Open {
        pane_id: String,
        url: String,
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        partition: Option<String>,
        /// `<W>x<H>` for size; defaults to 1024x768. Position defaults to (0,0).
        /// Ignored for `--engine chrome` (own OS window).
        #[arg(long, default_value = "1024x768")]
        rect: String,
        /// Engine backing the pane: `webkit` (default, in-shell child webview)
        /// or `chrome` (Managed mode — installed Chrome over CDP, own window).
        #[arg(value_enum, long, default_value = "webkit")]
        engine: Engine,
        /// Playwright mode for `--engine chrome`: `managed` (default — dedicated
        /// profile, own window) or `attach` (drive a running debug Chrome).
        /// `--attach-target` only takes effect in `attach` mode.
        #[arg(long, default_value = "managed")]
        mode: String,
        /// For `--engine chrome` attach mode: which tab to drive.
        /// `new` (default) opens a fresh tab without touching open tabs.
        /// `active` adopts the first open tab (navigates only if url given).
        /// Any other value is treated as a CDP target id or URL substring to match.
        #[arg(long, default_value = "new")]
        attach_target: String,
    },
    /// Close a pane.
    Close { pane_id: String },
    /// List open panes for this pkg.
    List,
    /// Focus a pane (kernel-side currently a no-op; preserved for forward compat).
    Focus { pane_id: String },
    /// Navigate an existing pane.
    Goto { pane_id: String, url: String },
    /// History back.
    Back { pane_id: String },
    /// History forward.
    Forward { pane_id: String },
    /// Reload.
    Reload { pane_id: String },
    /// Accessibility-tree snapshot.
    Snapshot {
        pane_id: String,
        #[arg(long)]
        query: Option<String>,
        #[arg(long)]
        all: bool,
    },
    /// Read one element's text by ref.
    ReadText { pane_id: String, r#ref: String },
    /// Click an element. Exactly one of --ref / --selector / --text.
    Click {
        pane_id: String,
        #[arg(long)]
        r#ref: Option<String>,
        #[arg(long)]
        selector: Option<String>,
        #[arg(long)]
        text: Option<String>,
    },
    /// Fill an input/textarea/contenteditable. Exactly one of --ref / --selector.
    Fill {
        pane_id: String,
        text: String,
        #[arg(long)]
        r#ref: Option<String>,
        #[arg(long)]
        selector: Option<String>,
        #[arg(long)]
        replace: bool,
    },
    /// Pick an option in a <select>.
    Select {
        pane_id: String,
        value: String,
        #[arg(long)]
        r#ref: Option<String>,
        #[arg(long)]
        selector: Option<String>,
    },
    /// Dispatch a key combo.
    PressKey {
        pane_id: String,
        combo: String,
        #[arg(long)]
        r#ref: Option<String>,
        #[arg(long)]
        selector: Option<String>,
    },
    /// Wait until a predicate is satisfied. Kinds: url / text / gone-text /
    /// selector / gone-selector / ref / idle. `idle` ignores `value`.
    WaitFor {
        pane_id: String,
        kind: String,
        #[arg(default_value = "")]
        value: String,
        #[arg(long)]
        timeout_ms: Option<u64>,
    },
    /// Evaluate a JS expression in the pane and return its result.
    Eval { pane_id: String, script: String },
    /// Pause: snapshot/interaction calls return 409 until resumed.
    Pause { pane_id: String },
    /// Resume a paused pane.
    Resume { pane_id: String },
    /// Named-session management.
    Session {
        #[command(subcommand)]
        action: BrowserSessionAction,
    },

    /// List on-disk Chrome profiles (dir, display name, running status).
    /// OS Chrome profiles only — not Ikenga named sessions/partitions.
    /// Data dir: ~/.config/google-chrome (Linux),
    ///            ~/Library/Application Support/Google/Chrome (macOS),
    ///            %LOCALAPPDATA%/Google/Chrome/User Data (Windows).
    #[command(name = "chrome-profiles")]
    ChromeProfiles,

    /// List open targets (tabs/windows) on the running debug Chrome endpoint.
    /// Requires Chrome to be started with `--remote-debugging-port`.
    /// Prints a hint if no CDP endpoint is reachable.
    #[command(name = "chrome-targets")]
    ChromeTargets,

    /// Launch an on-disk Chrome profile in debug mode so it can be attached to.
    /// Errors if that profile is already running (singleton lock present).
    #[command(name = "chrome-launch-profile")]
    ChromeLaunchProfile {
        /// Profile directory name (e.g. `Default`, `Profile 3`).
        /// Use `iyke browser chrome-profiles` to list names.
        dir: String,
        /// CDP debug port to bind. Default: 9222.
        #[arg(long, default_value = "9222")]
        port: u16,
    },
}

#[derive(Subcommand)]
enum BrowserSessionAction {
    /// Create a named session (cookie/storage jar).
    Create {
        name: String,
        #[arg(long)]
        partition: Option<String>,
    },
    /// List named sessions for the active pkg.
    List,
    /// Delete a named session (cookie data on disk is preserved).
    Delete { name: String },
}

#[derive(Subcommand)]
enum ChiAction {
    /// Start a new agent run.
    Run {
        /// Engine id (e.g. claude-code, gemini, codex).
        engine_id: String,
        /// The prompt / task to send.
        #[arg(long)]
        prompt: String,
        /// Working directory. Defaults to the current directory.
        #[arg(long)]
        cwd: Option<String>,
        /// Engine model, if the engine supports it.
        #[arg(long)]
        model: Option<String>,
        /// Permission mode: plan, default, auto, bypassPermissions.
        #[arg(long)]
        mode: Option<String>,
        /// Optional parent run id (subagent chain).
        #[arg(long)]
        parent_id: Option<String>,
        /// Resume from an existing engine session id instead of starting fresh.
        #[arg(long)]
        resume_session_id: Option<String>,
        /// Timeout in seconds. Currently reserved.
        #[arg(long)]
        timeout: Option<u32>,
        /// Launch via tmux so the session survives an Ikenga app restart.
        /// Use `iyke chi attach <run_id>` to reconnect later.
        #[arg(long, default_value_t = false)]
        persistent: bool,
    },
    /// Continue an existing agent run with a new prompt.
    Resume {
        /// Run id returned by `iyke chi run`.
        run_id: String,
        #[arg(long)]
        prompt: String,
    },
    /// Print the current status and output of a run.
    Status {
        /// Run id.
        run_id: String,
    },
    /// List agent runs, optionally filtered by engine.
    List {
        /// Filter by engine id.
        #[arg(long)]
        engine: Option<String>,
        /// Maximum rows.
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
    /// Cancel a running agent run.
    Cancel {
        /// Run id.
        run_id: String,
    },
    /// Attach (or re-attach) to the tmux session backing a persistent chi run.
    /// The chi run must have been started with --persistent (or via Ikenga with
    /// tmux persistence enabled). Spawns `tmux attach-session -t <run_id>`.
    Attach {
        /// Run id returned by `iyke chi run`.
        run_id: String,
    },
}

#[derive(Copy, Clone, ValueEnum)]
enum ScreenshotTarget {
    Window,
    Pane,
}

#[derive(Subcommand)]
enum OpenKind {
    /// Open a route view at `path`.
    Route { path: String },
    /// Open a fresh terminal session.
    Terminal {
        /// Optional command (joined with spaces for the shell). Defaults to login shell.
        #[arg(long)]
        cmd: Option<String>,
    },
    /// Open a chat session by id (or "new" to start one — server-side decides).
    Chat { session_id: String },
    /// Open a file artifact viewer.
    Artifact { path: String },
    /// Open a folder as a Lightroom-style artifact-grid pane (one cell per
    /// `.html` file with iframe thumbnails + pin overlay).
    #[command(name = "artifact-grid")]
    ArtifactGrid { path: String },
    /// Open a mini-app by name (video-engine, canvas-design, image-generator).
    MiniApp { name: String },
}

#[derive(Subcommand)]
enum FocusTarget {
    /// Focus by leaf id.
    Id { pane_id: String },
    /// Focus by 1-based DFS index, like ⌃1..⌃6 in-app.
    Index { index: u8 },
}

#[derive(Copy, Clone, ValueEnum)]
enum SplitDirection {
    Horizontal,
    Vertical,
}

impl SplitDirection {
    fn as_str(self) -> &'static str {
        match self {
            Self::Horizontal => "horizontal",
            Self::Vertical => "vertical",
        }
    }
}

/// Browser engine backing a pane (WP-07 routing). `webkit` is the in-shell
/// child webview; `chrome` is Managed mode — the shell drives the user's
/// installed Chrome over CDP in its own OS window. Decided at `open`; the
/// shell remembers it per pane, so later verbs don't repeat it.
#[derive(Copy, Clone, ValueEnum)]
enum Engine {
    Webkit,
    Chrome,
}

impl Engine {
    fn as_str(self) -> &'static str {
        match self {
            Self::Webkit => "webkit",
            Self::Chrome => "chrome",
        }
    }
}

/// `1600x1000` → `(label, json)` for explicit size; preset keyword →
/// `(label, { "preset": <kw> })`. Anything else is a hard error so users
/// see a typo immediately rather than getting a server-side 400.
fn parse_resize_target(target: &str) -> Result<(String, serde_json::Value)> {
    const PRESETS: &[&str] = &[
        "maximize",
        "unmaximize",
        "fullscreen",
        "unfullscreen",
        "minimize",
    ];
    if PRESETS.contains(&target) {
        return Ok((format!("resize {target}"), json!({ "preset": target })));
    }
    if let Some((w, h)) = target.split_once('x') {
        let w: u32 = w
            .parse()
            .map_err(|_| anyhow!("invalid width in {target:?}: expected integer"))?;
        let h: u32 = h
            .parse()
            .map_err(|_| anyhow!("invalid height in {target:?}: expected integer"))?;
        return Ok((
            format!("resize {w}x{h}"),
            json!({ "width": w, "height": h }),
        ));
    }
    Err(anyhow!(
        "could not parse resize target {target:?}: expected `<W>x<H>` or one of {PRESETS:?}"
    ))
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let fmt = Format::from_flag(cli.json);

    let cf = match control::load()? {
        LoadOutcome::Ok(cf) => cf,
        LoadOutcome::Missing => {
            return Err(anyhow!(
                "PA desktop app does not appear to be running (no control.json found)."
            ));
        }
        LoadOutcome::StaleRemoved => {
            return Err(anyhow!(
                "PA desktop app does not appear to be running (cleared a stale control.json from a previous launch)."
            ));
        }
        LoadOutcome::StaleYoung { age_secs } => {
            return Err(anyhow!(
                "control.json exists but the recorded PID is dead and the file is only {age_secs}s old \
                 (threshold {STALE_THRESHOLD_SECS}s). The app may be launching or in a startup race; \
                 retry shortly, or delete the file by hand if you're sure it's stale."
            ));
        }
    };

    let client = Client::new(&cf);

    match cli.command {
        Command::State => {
            let v = client.get_state()?;
            print_state(&v, fmt);
        }
        Command::Terminals => {
            let v = client.get_with_query("/iyke/terminal/list", &[])?;
            output::print_terminals(&v, fmt);
        }
        Command::Windows => {
            let v = client.get_with_query("/iyke/windows", &[])?;
            output::print_windows(&v, fmt);
        }
        Command::Go { path } => {
            if !path.starts_with('/') {
                return Err(anyhow!("path must start with '/' (got {path:?})"));
            }
            let v = client.post("/iyke/go", json!({ "path": path }))?;
            print_write_result(&format!("go {path}"), &v, fmt);
        }
        Command::Mode { mode } => {
            let v = client.post("/iyke/mode", json!({ "mode": mode }))?;
            print_write_result(&format!("mode {mode}"), &v, fmt);
        }
        Command::Open { kind } => {
            let (label, body) = match kind {
                OpenKind::Route { path } => (
                    format!("open route {path}"),
                    json!({ "kind": "route", "path": path }),
                ),
                OpenKind::Terminal { cmd } => (
                    format!(
                        "open terminal{}",
                        cmd.as_deref()
                            .map(|c| format!(" ({c})"))
                            .unwrap_or_default()
                    ),
                    json!({ "kind": "terminal", "cmd": cmd }),
                ),
                OpenKind::Chat { session_id } => (
                    format!("open chat {session_id}"),
                    json!({ "kind": "chat", "session_id": session_id }),
                ),
                OpenKind::Artifact { path } => (
                    format!("open artifact {path}"),
                    json!({ "kind": "artifact", "path": path }),
                ),
                OpenKind::ArtifactGrid { path } => (
                    format!("open artifact-grid {path}"),
                    json!({ "kind": "artifact-grid", "path": path }),
                ),
                OpenKind::MiniApp { name } => (
                    format!("open mini-app {name}"),
                    json!({ "kind": "mini-app", "name": name }),
                ),
            };
            let v = client.post("/iyke/open", body)?;
            print_write_result(&label, &v, fmt);
        }
        Command::Split { direction, pane_id } => {
            let body = match pane_id {
                Some(id) => json!({ "direction": direction.as_str(), "pane_id": id }),
                None => json!({ "direction": direction.as_str() }),
            };
            let v = client.post("/iyke/split", body)?;
            print_write_result(&format!("split {}", direction.as_str()), &v, fmt);
        }
        Command::Focus { target } => {
            let (label, body) = match target {
                FocusTarget::Id { pane_id } => {
                    (format!("focus {pane_id}"), json!({ "pane_id": pane_id }))
                }
                FocusTarget::Index { index } => {
                    (format!("focus index {index}"), json!({ "index": index }))
                }
            };
            let v = client.post("/iyke/focus", body)?;
            print_write_result(&label, &v, fmt);
        }
        Command::Resize { target } => {
            let (label, body) = parse_resize_target(&target)?;
            let v = client.post("/iyke/resize", body)?;
            print_write_result(&label, &v, fmt);
        }
        Command::Refresh { pane_id } => {
            let body = match pane_id.as_ref() {
                Some(id) => json!({ "pane_id": id }),
                None => json!({}),
            };
            let v = client.post("/iyke/refresh", body)?;
            print_write_result(
                &format!(
                    "refresh{}",
                    pane_id.map(|id| format!(" {id}")).unwrap_or_default()
                ),
                &v,
                fmt,
            );
        }
        Command::Close { pane_id } => {
            let body = match pane_id {
                Some(ref id) => json!({ "pane_id": id }),
                None => json!({}),
            };
            let v = client.post("/iyke/close", body)?;
            print_write_result(
                &format!(
                    "close{}",
                    pane_id.map(|id| format!(" {id}")).unwrap_or_default()
                ),
                &v,
                fmt,
            );
        }
        Command::Dom { query, all, pane } => {
            let mut q = Vec::new();
            if let Some(s) = &query {
                q.push(("query", s.clone()));
            }
            if all {
                q.push(("all", "true".into()));
            }
            if let Some(p) = &pane {
                q.push(("pane", p.clone()));
            }
            let v = client.get_with_query("/iyke/dom", &q)?;
            output::print_dom(&v, fmt);
        }
        Command::Logs {
            level,
            since,
            source,
        } => {
            let mut q = Vec::new();
            if let Some(s) = &level {
                q.push(("level", s.clone()));
            }
            if let Some(s) = since {
                q.push(("since", s.to_string()));
            }
            if let Some(s) = &source {
                q.push(("source", s.clone()));
            }
            let v = client.get_with_query("/iyke/logs", &q)?;
            output::print_logs(&v, fmt);
        }
        Command::Network { since, source } => {
            let mut q = Vec::new();
            if let Some(s) = since {
                q.push(("since", s.to_string()));
            }
            if let Some(s) = &source {
                q.push(("source", s.clone()));
            }
            let v = client.get_with_query("/iyke/network", &q)?;
            output::print_network(&v, fmt);
        }
        Command::Screenshot {
            target,
            out,
            pane_id,
        } => {
            let path = match target {
                ScreenshotTarget::Window => "/iyke/screenshot/window",
                ScreenshotTarget::Pane => "/iyke/screenshot/pane",
            };
            let mut body = serde_json::Map::new();
            if let Some(p) = out {
                body.insert("out_path".into(), json!(p));
            }
            if matches!(target, ScreenshotTarget::Pane) {
                let id = pane_id.ok_or_else(|| anyhow!("--pane-id required when target=pane"))?;
                body.insert("pane_id".into(), json!(id));
            }
            let v = client.post(path, serde_json::Value::Object(body))?;
            output::print_screenshot(&v, fmt);
        }
        Command::Wait {
            kind,
            value,
            timeout_ms,
            pane,
        } => {
            let body = json!({
                "kind": kind,
                "value": value,
                "timeout_ms": timeout_ms,
                "pane": pane,
            });
            let v = client.post("/iyke/wait", body)?;
            let satisfied = output::print_wait(&v, fmt);
            if !satisfied {
                std::process::exit(2);
            }
        }
        Command::Click {
            r#ref,
            selector,
            text,
            pane,
        } => {
            require_one(&r#ref, &selector, &text)?;
            let body = json!({
                "ref": r#ref,
                "selector": selector,
                "text": text,
                "pane": pane,
            });
            let v = client.post("/iyke/click", body)?;
            print_write_result("click", &v, fmt);
        }
        Command::Type {
            text,
            r#ref,
            selector,
            replace,
            pane,
        } => {
            require_one(&r#ref, &selector, &None)?;
            let body = json!({
                "ref": r#ref,
                "selector": selector,
                "text": text,
                "replace": replace,
                "pane": pane,
            });
            let v = client.post("/iyke/type", body)?;
            print_write_result("type", &v, fmt);
        }
        Command::Key {
            combo,
            r#ref,
            selector,
            pane,
        } => {
            let body = json!({
                "combo": combo,
                "ref": r#ref,
                "selector": selector,
                "pane": pane,
            });
            let v = client.post("/iyke/key", body)?;
            print_write_result(&format!("key {combo}"), &v, fmt);
        }
        Command::TerminalRead {
            pane,
            session,
            terminal,
            label,
            after,
            mode,
            bytes,
            raw,
            no_focus,
        } => {
            require_at_most_one_target(&pane, &session, &terminal, &label)?;
            if no_focus && terminal.is_none() && label.is_none() && session.is_none() {
                return Err(anyhow!(
                    "--no-focus requires --terminal, --label, or --session"
                ));
            }
            let mut q: Vec<(&str, String)> = Vec::new();
            if let Some(p) = &pane {
                q.push(("pane", p.clone()));
            }
            if let Some(session) = &session {
                q.push(("session", session.clone()));
            }
            if let Some(terminal) = &terminal {
                q.push(("terminal", terminal.clone()));
            }
            if let Some(label) = &label {
                q.push(("label", label.clone()));
            }
            if let Some(after) = after {
                q.push(("after", after.to_string()));
            }
            q.push(("mode", mode));
            if let Some(b) = bytes {
                q.push(("bytes", b.to_string()));
            }
            if raw {
                q.push(("raw", "true".into()));
            }
            let v = client.get_with_query("/iyke/terminal/read", &q)?;
            match fmt {
                Format::Json => {
                    println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default())
                }
                Format::Human => {
                    if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
                        eprintln!("terminal-read: {err}");
                        std::process::exit(1);
                    }
                    if let Some(text) = v.get("text").and_then(|t| t.as_str()) {
                        print!("{text}");
                        if !text.ends_with('\n') {
                            println!();
                        }
                    }
                }
            }
        }
        Command::TerminalSend {
            text,
            keys,
            pane,
            session,
            terminal,
            label,
            expected_pty_id,
            actor,
            lease_token,
            dry_run,
            no_focus,
        } => {
            require_at_most_one_target(&pane, &session, &terminal, &label)?;
            if no_focus && terminal.is_none() && label.is_none() && session.is_none() {
                return Err(anyhow!(
                    "--no-focus requires --terminal, --label, or --session"
                ));
            }
            if text.is_none() && keys.is_empty() {
                return Err(anyhow!(
                    "terminal-send: must provide text and/or at least one --key"
                ));
            }
            let data = text.as_deref().map(interpret_backslash_escapes);
            let body = json!({
                "pane": pane,
                "session": session,
                "terminal": terminal,
                "label": label,
                "expected_pty_id": expected_pty_id,
                "actor": actor,
                "lease_token": lease_token,
                "dry_run": dry_run,
                "data": data,
                "keys": keys,
            });
            let v = client.post("/iyke/terminal/send", body)?;
            // If the server returns ok:false (pane had no writable terminal),
            // treat it as a hard error so the caller gets a non-zero exit and
            // the silent-success bug from issue #78 is surfaced immediately.
            if v.get("ok").and_then(|o| o.as_bool()) == Some(false) {
                let msg = v
                    .get("error")
                    .and_then(|e| e.as_str())
                    .unwrap_or("pane has no writable terminal");
                return Err(anyhow!("terminal-send: {msg}"));
            }
            // Build human-readable summary. Include resolved terminal/pty ids
            // and delivered byte count when the server returns them (it does for
            // direct-target writes that go through controlled_write).
            let delivered_bytes = v.get("byte_count").and_then(|b| b.as_u64())
                .or_else(|| data.as_ref().map(|d| d.len() as u64));
            let terminal_tag = v.get("terminal_id")
                .and_then(|t| t.as_str())
                .map(|id| format!(" [terminal {}]", &id[..id.len().min(8)]))
                .unwrap_or_default();
            let bytes_tag = delivered_bytes
                .map(|b| format!(" {}b", b))
                .unwrap_or_default();
            let summary = match (!keys.is_empty(), delivered_bytes.is_some() || data.is_some()) {
                (true, true) => format!("terminal-send{terminal_tag}{bytes_tag} + {} key(s)", keys.len()),
                (true, false) => format!("terminal-send{terminal_tag} {} key(s)", keys.len()),
                (false, _) => format!("terminal-send{terminal_tag}{bytes_tag}"),
            };
            print_write_result(&summary, &v, fmt);
        }
        Command::TerminalWait {
            terminal,
            label,
            session,
            r#match,
            until_idle_ms,
            after,
            timeout_ms,
            raw,
        } => {
            let target = exactly_one_terminal_target(terminal, label, session)?;
            if r#match.is_none() == until_idle_ms.is_none() {
                return Err(anyhow!("set exactly one of --match or --until-idle-ms"));
            }
            let v = client.post_with_timeout(
                "/iyke/terminal/wait",
                json!({
                    "terminal": target,
                    "match": r#match,
                    "until_idle_ms": until_idle_ms,
                    "after": after,
                    "timeout_ms": timeout_ms,
                    "raw": raw,
                }),
                std::time::Duration::from_millis(timeout_ms.saturating_add(5_000)),
            )?;
            let satisfied = output::print_terminal_wait(&v, fmt);
            if !satisfied {
                std::process::exit(2);
            }
        }
        Command::TerminalSpawn {
            cwd,
            title,
            label,
            pane,
            lease_for,
            lease_ttl_ms,
            argv,
        } => {
            // The shell waits up to 10s for the frontend to mint an id and
            // another 10s for the pty to reach the registry, so the default
            // 65s post ceiling is already generous — but be explicit rather
            // than inheriting a number tuned for `wait`.
            let v = client.post_with_timeout(
                "/iyke/terminal/spawn",
                json!({
                    "cwd": cwd,
                    "argv": if argv.is_empty() { None } else { Some(argv) },
                    "title": title,
                    "label": label,
                    "pane": pane,
                    "lease_for": lease_for,
                    "lease_ttl_ms": lease_ttl_ms,
                }),
                std::time::Duration::from_secs(30),
            )?;
            output::print_terminal_spawn(&v, fmt);
        }
        Command::TerminalKill {
            terminal,
            close_tab,
        } => {
            let v = client.post(
                "/iyke/terminal/kill",
                json!({ "terminal": terminal, "close_tab": close_tab }),
            )?;
            print_write_result("terminal-kill", &v, fmt);
        }
        Command::Agent { action } => match action {
            AgentAction::Register { id, name, model } => {
                let v = client.post(
                    "/iyke/agent/register",
                    json!({ "id": id, "name": name, "model": model }),
                )?;
                print_write_result("agent register", &v, fmt);
            }
        },
        Command::Inbox { action } => match action {
            InboxAction::List {
                agent_id,
                since,
                limit,
                ack,
            } => {
                let mut params = vec![("agent_id", agent_id.clone()), ("since", since.to_string())];
                if let Some(n) = limit {
                    params.push(("limit", n.to_string()));
                }
                let v = client.get_with_query("/iyke/agent/inbox", &params)?;
                output::print_inbox(&v, fmt);
                // Ack AFTER printing: acking deletes, so a failure to render
                // must not be able to lose entries that were never shown.
                if ack {
                    let ids: Vec<String> = v
                        .get("entries")
                        .and_then(Value::as_array)
                        .map(|entries| {
                            entries
                                .iter()
                                .filter_map(|e| e.get("id").and_then(Value::as_str))
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default();
                    if !ids.is_empty() {
                        let acked = client.post("/iyke/agent/inbox/ack", json!({ "ids": ids }))?;
                        print_write_result("inbox ack", &acked, fmt);
                    }
                }
            }
            InboxAction::Ack { ids } => {
                let v = client.post("/iyke/agent/inbox/ack", json!({ "ids": ids }))?;
                print_write_result("inbox ack", &v, fmt);
            }
        },
        Command::Task { action } => run_task(&client, action, fmt)?,
        Command::TerminalLabel {
            terminal,
            label,
            clear,
        } => {
            if clear == label.is_some() {
                return Err(anyhow!("provide a label or --clear"));
            }
            let v = client.post(
                "/iyke/terminal/label",
                json!({ "terminal": terminal, "label": if clear { None } else { label } }),
            )?;
            print_write_result("terminal-label", &v, fmt);
        }
        Command::TerminalLease { action } => {
            let (path, body, description) = match action {
                TerminalLeaseAction::Acquire {
                    terminal,
                    agent_id,
                    ttl_ms,
                } => (
                    "/iyke/terminal/lease/acquire",
                    json!({ "terminal": terminal, "agent_id": agent_id, "ttl_ms": ttl_ms }),
                    "terminal-lease acquire",
                ),
                TerminalLeaseAction::Release { terminal, token } => (
                    "/iyke/terminal/lease/release",
                    json!({ "terminal": terminal, "token": token }),
                    "terminal-lease release",
                ),
            };
            let v = client.post(path, body)?;
            match fmt {
                Format::Json => println!("{}", v),
                Format::Human => {
                    println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default())
                }
            }
            let _ = description;
        }
        Command::TerminalAudit => {
            let v = client.get_with_query("/iyke/terminal/audit", &[])?;
            match fmt {
                Format::Json => println!("{}", v),
                Format::Human => {
                    println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default())
                }
            }
        }
        Command::Tab { action } => match action {
            TabAction::Activate {
                pane,
                index,
                terminal,
            } => {
                if index.is_none() == terminal.is_none() {
                    return Err(anyhow!("set exactly one of --index or --terminal"));
                }
                let v = client.post(
                    "/iyke/tab/activate",
                    json!({ "pane": pane, "index": index, "terminal": terminal }),
                )?;
                print_write_result("tab activate", &v, fmt);
            }
        },
        Command::QueryCache { pane } => {
            let mut q = Vec::new();
            if let Some(p) = &pane {
                q.push(("pane", p.clone()));
            }
            let v = client.get_with_query("/iyke/query-cache", &q)?;
            output::print_query_cache(&v, fmt);
        }
        Command::Devtools => {
            let v = client.post("/iyke/devtools", json!({}))?;
            print_write_result("devtools", &v, fmt);
        }
        Command::IframeState { pane } => {
            let v = client.get_with_query("/iyke/iframe-state", &[("pane", pane.clone())])?;
            output::print_iframe_state(&v, fmt);
        }
        Command::IframeSend {
            pane,
            kind,
            payload,
        } => {
            let parsed: serde_json::Value =
                serde_json::from_str(&payload).map_err(|e| anyhow!("invalid payload JSON: {e}"))?;
            let v = client.post(
                "/iyke/iframe-message",
                json!({ "pane": pane, "kind": kind, "payload": parsed }),
            )?;
            print_write_result(&format!("iframe-send {pane} {kind}"), &v, fmt);
        }
        Command::Scratchpad { action } => {
            run_scratchpad(&client, action, fmt)?;
        }
        Command::Browser { pkg_id, action } => {
            run_browser(&client, &pkg_id, action, fmt)?;
        }
        Command::Project { action } => cmd::project::run(&client, action, fmt)?,
        Command::Ngwa { action } => cmd::ngwa::run(&client, action, fmt)?,
        Command::Actions { action } => cmd::actions::run(&client, action, fmt)?,
        Command::Menus { action } => cmd::menus::run(&client, action, fmt)?,
        Command::Keys { action } => cmd::keys::run(&client, action, fmt)?,
        Command::Chi { action } => {
            run_chi(&client, action, fmt)?;
        }
    }

    Ok(())
}

fn run_scratchpad(client: &Client, action: ScratchpadAction, fmt: Format) -> Result<()> {
    match action {
        ScratchpadAction::Get { scope, name } => {
            let mut q = vec![("name", name)];
            if let Some(scope) = scope {
                q.push(("scope", scope));
            }
            let value = client.get_with_query("/iyke/scratchpad/read", &q)?;
            print_scratchpad(&value, fmt);
        }
        ScratchpadAction::Set {
            scope,
            name,
            body,
            body_file,
        } => {
            let body = load_body(body, body_file)?;
            let value = client.post(
                "/iyke/scratchpad/write",
                json!({ "scope": scope, "name": name, "body": body }),
            )?;
            print_write_result("scratchpad set", &value, fmt);
        }
        ScratchpadAction::Append {
            scope,
            name,
            body,
            body_file,
            no_separator,
        } => {
            let body = load_body(body, body_file)?;
            let value = client.post(
                "/iyke/scratchpad/append",
                json!({
                    "scope": scope,
                    "name": name,
                    "body": body,
                    "with_separator": !no_separator,
                }),
            )?;
            print_write_result("scratchpad append", &value, fmt);
        }
        ScratchpadAction::List { scope } => {
            let q = scope
                .map(|scope| vec![("scope", scope)])
                .unwrap_or_default();
            let value = client.get_with_query("/iyke/scratchpad/list", &q)?;
            print_scratchpad_list(&value, fmt);
        }
        ScratchpadAction::Watch {
            scope,
            name,
            wait_ms,
        } => {
            if wait_ms == 0 {
                return Err(anyhow!("--wait-ms must be greater than zero"));
            }
            let mut since = -1_i64;
            loop {
                let mut q = vec![("name", name.clone()), ("since", since.to_string())];
                if let Some(scope) = &scope {
                    q.push(("scope", scope.clone()));
                }
                q.push(("wait_ms", wait_ms.to_string()));
                let value = client.get_with_query_timeout(
                    "/iyke/scratchpad/watch",
                    &q,
                    std::time::Duration::from_millis(wait_ms.saturating_add(5_000)),
                )?;
                if value.get("updated").and_then(|v| v.as_bool()) == Some(true) {
                    since = value
                        .get("updated_at")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(since);
                    if value.get("deleted").and_then(|v| v.as_bool()) == Some(true) {
                        match fmt {
                            Format::Json => println!("{}", value),
                            Format::Human => println!("(scratchpad deleted)"),
                        }
                    } else {
                        print_scratchpad(&value, fmt);
                    }
                }
            }
        }
    }
    Ok(())
}

fn load_body(body: Option<String>, body_file: Option<String>) -> Result<String> {
    match (body, body_file) {
        (Some(body), None) => Ok(body),
        (None, Some(path)) => std::fs::read_to_string(&path)
            .map_err(|e| anyhow!("read scratchpad body file {path:?}: {e}")),
        _ => Err(anyhow!("must provide exactly one of --body or --body-file")),
    }
}

fn print_scratchpad(value: &serde_json::Value, fmt: Format) {
    match fmt {
        Format::Json => println!("{}", value),
        Format::Human => {
            if let Some(body) = value.get("body").and_then(|v| v.as_str()) {
                print!("{body}");
                if !body.ends_with('\n') {
                    println!();
                }
            }
        }
    }
}

fn print_scratchpad_list(value: &serde_json::Value, fmt: Format) {
    match fmt {
        Format::Json => println!("{}", value),
        Format::Human => {
            let items = value
                .get("scratchpads")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            if items.is_empty() {
                println!("(no scratchpads)");
            } else {
                for item in items {
                    let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                    let updated_at = item.get("updated_at").and_then(|v| v.as_i64()).unwrap_or(0);
                    let preview = item.get("preview").and_then(|v| v.as_str()).unwrap_or("");
                    println!("{updated_at}  {name}  {}", preview.replace('\n', " "));
                }
            }
        }
    }
}

/// Interpret a handful of common backslash escapes in CLI input so users
/// don't have to figure out their shell's quoting rules to send a newline.
/// Unknown escapes pass through as the literal backslash + char.
fn interpret_backslash_escapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let Some(&next) = chars.peek() else {
            out.push('\\');
            break;
        };
        chars.next();
        match next {
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            '0' => out.push('\0'),
            '\\' => out.push('\\'),
            '"' => out.push('"'),
            '\'' => out.push('\''),
            'e' => out.push('\x1b'),
            'x' => {
                let h1 = chars.next();
                let h2 = chars.next();
                match (h1, h2) {
                    (Some(a), Some(b)) => {
                        let hex: String = [a, b].iter().collect();
                        match u8::from_str_radix(&hex, 16) {
                            Ok(byte) => out.push(byte as char),
                            Err(_) => {
                                out.push('\\');
                                out.push('x');
                                out.push(a);
                                out.push(b);
                            }
                        }
                    }
                    _ => {
                        out.push('\\');
                        out.push('x');
                        if let Some(a) = h1 {
                            out.push(a);
                        }
                    }
                }
            }
            other => {
                out.push('\\');
                out.push(other);
            }
        }
    }
    out
}

fn parse_rect(s: &str) -> Result<serde_json::Value> {
    let (w, h) = s
        .split_once('x')
        .ok_or_else(|| anyhow!("rect must be <W>x<H> (got {s:?})"))?;
    let w: u32 = w
        .parse()
        .map_err(|_| anyhow!("rect width not an integer: {s:?}"))?;
    let h: u32 = h
        .parse()
        .map_err(|_| anyhow!("rect height not an integer: {s:?}"))?;
    Ok(json!({ "x": 0, "y": 0, "w": w, "h": h }))
}

fn run_task(client: &Client, action: TaskAction, fmt: Format) -> Result<()> {
    match action {
        TaskAction::List {
            status,
            assigned_to,
            project_path,
            limit,
        } => {
            let mut params: Vec<(&str, String)> = Vec::new();
            if let Some(v) = status {
                params.push(("status", v));
            }
            if let Some(v) = assigned_to {
                params.push(("assigned_to", v));
            }
            if let Some(v) = project_path {
                params.push(("project_path", v));
            }
            if let Some(v) = limit {
                params.push(("limit", v.to_string()));
            }
            let v = client.get_with_query("/iyke/task/list", &params)?;
            output::print_tasks(&v, fmt);
        }
        TaskAction::Create {
            title,
            description,
            status,
            priority,
            assigned_to,
            project_path,
            due_date,
            actor,
        } => {
            let v = client.post(
                "/iyke/task/create",
                json!({
                    "title": title,
                    "description": description,
                    "status": status,
                    "priority": priority,
                    "assigned_to": assigned_to,
                    "project_path": project_path,
                    "due_date": due_date,
                    "actor": actor,
                }),
            )?;
            print_write_result("task create", &v, fmt);
        }
        TaskAction::Update {
            id,
            title,
            description,
            status,
            priority,
            assigned_to,
            progress_pct,
            task_result,
            outcome_notes,
            actor,
        } => {
            // The shell rejects a no-op update, but catching it here names
            // the flags the caller actually has rather than echoing a
            // generic "no fields to update" from the far side of the wire.
            if title.is_none()
                && description.is_none()
                && status.is_none()
                && priority.is_none()
                && assigned_to.is_none()
                && progress_pct.is_none()
                && task_result.is_none()
                && outcome_notes.is_none()
            {
                return Err(anyhow!(
                    "nothing to update — pass at least one of: --title, --description, \
                     --status, --priority, --assigned-to, --progress-pct, --task-result, \
                     --outcome-notes"
                ));
            }
            let v = client.post(
                "/iyke/task/update",
                json!({
                    "id": id,
                    "title": title,
                    "description": description,
                    "status": status,
                    "priority": priority,
                    "assigned_to": assigned_to,
                    "progress_pct": progress_pct,
                    "task_result": task_result,
                    "outcome_notes": outcome_notes,
                    "actor": actor,
                }),
            )?;
            print_write_result("task update", &v, fmt);
        }
        TaskAction::Complete {
            id,
            task_result,
            outcome_notes,
            actor,
        } => {
            let v = client.post(
                "/iyke/task/complete",
                json!({
                    "id": id,
                    "task_result": task_result,
                    "outcome_notes": outcome_notes,
                    "actor": actor,
                }),
            )?;
            print_write_result("task complete", &v, fmt);
        }
    }
    Ok(())
}

fn run_browser(client: &Client, pkg_id: &str, action: BrowserAction, fmt: Format) -> Result<()> {
    match action {
        BrowserAction::ChromeProfiles => {
            let v = client.get_with_query("/iyke/browser/profiles", &[])?;
            match fmt {
                Format::Json => {
                    println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default())
                }
                Format::Human => {
                    if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
                        eprintln!("browser profiles: {err}");
                        std::process::exit(1);
                    }
                    let profiles = v
                        .get("profiles")
                        .and_then(|p| p.as_array())
                        .map(|a| a.as_slice())
                        .unwrap_or(&[]);
                    if profiles.is_empty() {
                        println!("No Chrome profiles found.");
                    } else {
                        println!("{:<20} {:<30} {}", "DIR", "NAME", "RUNNING");
                        println!("{}", "-".repeat(60));
                        for p in profiles {
                            let dir = p.get("dir").and_then(|d| d.as_str()).unwrap_or("-");
                            let name = p.get("name").and_then(|n| n.as_str()).unwrap_or("-");
                            let running =
                                p.get("running").and_then(|r| r.as_bool()).unwrap_or(false);
                            println!(
                                "{:<20} {:<30} {}",
                                dir,
                                name,
                                if running { "yes" } else { "no" }
                            );
                        }
                    }
                }
            }
        }
        BrowserAction::ChromeTargets => {
            let v = client.get_with_query("/iyke/browser/targets", &[])?;
            match fmt {
                Format::Json => {
                    println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default())
                }
                Format::Human => {
                    if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
                        eprintln!("browser targets: {err}");
                        std::process::exit(1);
                    }
                    let endpoint = v.get("endpoint").and_then(|e| e.as_str());
                    if endpoint.is_none() {
                        println!("No CDP endpoint reachable.");
                        println!("Start Chrome with:  --remote-debugging-port=9222 --remote-allow-origins=http://127.0.0.1:9222");
                        println!("Or use: iyke browser chrome-launch-profile <dir>");
                        return Ok(());
                    }
                    println!("Endpoint: {}", endpoint.unwrap_or("-"));
                    let targets = v
                        .get("targets")
                        .and_then(|t| t.as_array())
                        .map(|a| a.as_slice())
                        .unwrap_or(&[]);
                    if targets.is_empty() {
                        println!("No open tabs/windows.");
                    } else {
                        println!("{:<40} {:<10} {}", "TARGET ID", "KIND", "TITLE / URL");
                        println!("{}", "-".repeat(90));
                        for t in targets {
                            let id = t.get("targetId").and_then(|i| i.as_str()).unwrap_or("-");
                            let kind = t.get("kind").and_then(|k| k.as_str()).unwrap_or("-");
                            let title = t.get("title").and_then(|t| t.as_str()).unwrap_or("");
                            let url = t.get("url").and_then(|u| u.as_str()).unwrap_or("-");
                            let label = if title.is_empty() {
                                url.to_string()
                            } else {
                                format!("{title}  ({url})")
                            };
                            println!("{:<40} {:<10} {}", id, kind, label);
                        }
                    }
                }
            }
        }
        BrowserAction::ChromeLaunchProfile { dir, port } => {
            let v = client.post(
                "/iyke/browser/launch_profile",
                json!({ "dir": dir, "port": port }),
            )?;
            match fmt {
                Format::Json => {
                    println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default())
                }
                Format::Human => {
                    if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
                        eprintln!("browser launch-profile: {err}");
                        std::process::exit(1);
                    }
                    let endpoint = v
                        .get("endpoint")
                        .and_then(|e| e.as_str())
                        .unwrap_or("unknown");
                    println!("Launched Chrome profile '{dir}' — CDP endpoint: {endpoint}");
                }
            }
        }
        BrowserAction::Open {
            pane_id,
            url,
            session,
            partition,
            rect,
            engine,
            mode,
            attach_target,
        } => {
            if session.is_some() && partition.is_some() {
                return Err(anyhow!("pass at most one of --session / --partition"));
            }
            let resolved_partition: Option<String> = if let Some(name) = &session {
                let v = client.post(
                    "/iyke/browser/session/resolve",
                    json!({ "pkg_id": pkg_id, "name": name }),
                )?;
                Some(
                    v.get("partition")
                        .and_then(|p| p.as_str())
                        .ok_or_else(|| anyhow!("session resolve returned no partition"))?
                        .to_string(),
                )
            } else {
                partition
            };
            let body = json!({
                "pkg_id": pkg_id,
                "pane_id": pane_id,
                "url": url,
                "partition": resolved_partition,
                "rect": parse_rect(&rect)?,
                "engine": engine.as_str(),
                "mode": mode,
                "attach_target": attach_target,
            });
            let v = client.post("/iyke/browser/open", body)?;
            print_write_result(&format!("browser open {pane_id} {url}"), &v, fmt);
        }
        BrowserAction::Close { pane_id } => {
            let v = client.post(
                "/iyke/browser/close",
                json!({ "pkg_id": pkg_id, "pane_id": pane_id }),
            )?;
            print_write_result(&format!("browser close {pane_id}"), &v, fmt);
        }
        BrowserAction::List => {
            let v =
                client.get_with_query("/iyke/browser/list", &[("pkg_id", pkg_id.to_string())])?;
            print_write_result("browser list", &v, fmt);
        }
        BrowserAction::Focus { pane_id } => {
            let v = client.post(
                "/iyke/browser/focus",
                json!({ "pkg_id": pkg_id, "pane_id": pane_id }),
            )?;
            print_write_result(&format!("browser focus {pane_id}"), &v, fmt);
        }
        BrowserAction::Goto { pane_id, url } => {
            let v = client.post(
                "/iyke/browser/goto",
                json!({ "pkg_id": pkg_id, "pane_id": pane_id, "url": url }),
            )?;
            print_write_result(&format!("browser goto {pane_id} {url}"), &v, fmt);
        }
        BrowserAction::Back { pane_id } => {
            let v = client.post(
                "/iyke/browser/back",
                json!({ "pkg_id": pkg_id, "pane_id": pane_id }),
            )?;
            print_write_result(&format!("browser back {pane_id}"), &v, fmt);
        }
        BrowserAction::Forward { pane_id } => {
            let v = client.post(
                "/iyke/browser/forward",
                json!({ "pkg_id": pkg_id, "pane_id": pane_id }),
            )?;
            print_write_result(&format!("browser forward {pane_id}"), &v, fmt);
        }
        BrowserAction::Reload { pane_id } => {
            let v = client.post(
                "/iyke/browser/reload",
                json!({ "pkg_id": pkg_id, "pane_id": pane_id }),
            )?;
            print_write_result(&format!("browser reload {pane_id}"), &v, fmt);
        }
        BrowserAction::Snapshot {
            pane_id,
            query,
            all,
        } => {
            let v = client.post(
                "/iyke/browser/snapshot",
                json!({ "pkg_id": pkg_id, "pane_id": pane_id, "query": query, "all": all }),
            )?;
            print_write_result(&format!("browser snapshot {pane_id}"), &v, fmt);
        }
        BrowserAction::ReadText { pane_id, r#ref } => {
            let v = client.post(
                "/iyke/browser/read-text",
                json!({ "pkg_id": pkg_id, "pane_id": pane_id, "ref": r#ref }),
            )?;
            print_write_result(
                &format!("browser read-text {pane_id} {ref_}", ref_ = r#ref),
                &v,
                fmt,
            );
        }
        BrowserAction::Click {
            pane_id,
            r#ref,
            selector,
            text,
        } => {
            require_one(&r#ref, &selector, &text)?;
            let v = client.post(
                "/iyke/browser/click",
                json!({
                    "pkg_id": pkg_id, "pane_id": pane_id,
                    "ref": r#ref, "selector": selector, "text": text,
                }),
            )?;
            print_write_result(&format!("browser click {pane_id}"), &v, fmt);
        }
        BrowserAction::Fill {
            pane_id,
            text,
            r#ref,
            selector,
            replace,
        } => {
            require_one(&r#ref, &selector, &None)?;
            let v = client.post(
                "/iyke/browser/fill",
                json!({
                    "pkg_id": pkg_id, "pane_id": pane_id, "text": text,
                    "ref": r#ref, "selector": selector, "replace": replace,
                }),
            )?;
            print_write_result(&format!("browser fill {pane_id}"), &v, fmt);
        }
        BrowserAction::Select {
            pane_id,
            value,
            r#ref,
            selector,
        } => {
            require_one(&r#ref, &selector, &None)?;
            let v = client.post(
                "/iyke/browser/select",
                json!({
                    "pkg_id": pkg_id, "pane_id": pane_id, "value": value,
                    "ref": r#ref, "selector": selector,
                }),
            )?;
            print_write_result(&format!("browser select {pane_id} {value}"), &v, fmt);
        }
        BrowserAction::PressKey {
            pane_id,
            combo,
            r#ref,
            selector,
        } => {
            let v = client.post(
                "/iyke/browser/press-key",
                json!({
                    "pkg_id": pkg_id, "pane_id": pane_id, "combo": combo,
                    "ref": r#ref, "selector": selector,
                }),
            )?;
            print_write_result(&format!("browser press-key {pane_id} {combo}"), &v, fmt);
        }
        BrowserAction::WaitFor {
            pane_id,
            kind,
            value,
            timeout_ms,
        } => {
            let value_field: serde_json::Value = if value.is_empty() {
                serde_json::Value::Null
            } else {
                json!(value)
            };
            let v = client.post(
                "/iyke/browser/wait-for",
                json!({
                    "pkg_id": pkg_id, "pane_id": pane_id, "kind": kind,
                    "value": value_field, "timeout_ms": timeout_ms,
                }),
            )?;
            let satisfied = v
                .get("satisfied")
                .and_then(|s| s.as_bool())
                .unwrap_or(false);
            print_write_result(
                &format!("browser wait-for {pane_id} {kind}={value}"),
                &v,
                fmt,
            );
            if !satisfied {
                std::process::exit(2);
            }
        }
        BrowserAction::Eval { pane_id, script } => {
            let v = client.post(
                "/iyke/browser/eval",
                json!({ "pkg_id": pkg_id, "pane_id": pane_id, "script": script }),
            )?;
            print_write_result(&format!("browser eval {pane_id}"), &v, fmt);
        }
        BrowserAction::Pause { pane_id } => {
            let v = client.post(
                "/iyke/browser/pause",
                json!({ "pkg_id": pkg_id, "pane_id": pane_id }),
            )?;
            print_write_result(&format!("browser pause {pane_id}"), &v, fmt);
        }
        BrowserAction::Resume { pane_id } => {
            let v = client.post(
                "/iyke/browser/resume",
                json!({ "pkg_id": pkg_id, "pane_id": pane_id }),
            )?;
            print_write_result(&format!("browser resume {pane_id}"), &v, fmt);
        }
        BrowserAction::Session { action } => match action {
            BrowserSessionAction::Create { name, partition } => {
                let v = client.post(
                    "/iyke/browser/session/create",
                    json!({ "pkg_id": pkg_id, "name": name, "partition": partition }),
                )?;
                print_write_result(&format!("browser session create {name}"), &v, fmt);
            }
            BrowserSessionAction::List => {
                let v = client.get_with_query(
                    "/iyke/browser/session/list",
                    &[("pkg_id", pkg_id.to_string())],
                )?;
                print_write_result("browser session list", &v, fmt);
            }
            BrowserSessionAction::Delete { name } => {
                let v = client.post(
                    "/iyke/browser/session/delete",
                    json!({ "pkg_id": pkg_id, "name": name }),
                )?;
                print_write_result(&format!("browser session delete {name}"), &v, fmt);
            }
        },
    }
    Ok(())
}

fn run_chi(client: &Client, action: ChiAction, fmt: Format) -> Result<()> {
    match action {
        ChiAction::Run {
            engine_id,
            prompt,
            cwd,
            model,
            mode,
            parent_id,
            resume_session_id,
            timeout,
            persistent,
        } => {
            let body = json!({
                "engineId": engine_id,
                "prompt": prompt,
                "cwd": cwd,
                "model": model,
                "mode": mode,
                "parentId": parent_id,
                "resumeSessionId": resume_session_id,
                "timeoutSeconds": timeout,
                "persistent": persistent,
            });
            let v = client.post_with_timeout(
                "/iyke/chi/run",
                body,
                std::time::Duration::from_secs(120),
            )?;
            output::print_chi_result(&v, fmt);
        }
        ChiAction::Resume { run_id, prompt } => {
            let v = client.post(
                "/iyke/chi/resume",
                json!({"runId": run_id, "prompt": prompt}),
            )?;
            output::print_chi_result(&v, fmt);
        }
        ChiAction::Status { run_id } => {
            let v = client.get_with_query("/iyke/chi/status", &[("runId", run_id)])?;
            output::print_chi_result(&v, fmt);
        }
        ChiAction::List { engine, limit } => {
            let mut params = vec![("limit", limit.to_string())];
            if let Some(e) = engine {
                params.push(("engineId", e));
            }
            let v = client.get_with_query("/iyke/chi/list", &params)?;
            output::print_chi_list(&v, fmt);
        }
        ChiAction::Cancel { run_id } => {
            let v = client.post("/iyke/chi/cancel", json!({"runId": run_id}))?;
            output::print_chi_result(&v, fmt);
        }
        ChiAction::Attach { run_id } => {
            // Check that the run has a terminal_session_id (i.e. was started
            // with tmux persistence). If the field is absent or null we bail
            // early with a clear message rather than spawning a shell.
            let status = client.get_with_query("/iyke/chi/status", &[("runId", run_id.clone())])?;
            let ts_id = status
                .get("terminal_session_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let session = ts_id.unwrap_or_else(|| run_id.clone());

            // Verify the tmux session exists before attaching.
            let check = std::process::Command::new("tmux")
                .args(["has-session", "-t", &session])
                .status();
            match check {
                Ok(s) if s.success() => {}
                _ => {
                    eprintln!("iyke chi attach: tmux session '{session}' not found.");
                    eprintln!("The run may have already finished, or was not started with persistence.");
                    std::process::exit(1);
                }
            }

            // Replace this process with tmux attach.
            #[cfg(unix)]
            {
                use std::ffi::CString;
                let tmux = CString::new("tmux").unwrap();
                let attach = CString::new("attach-session").unwrap();
                let flag_t = CString::new("-t").unwrap();
                let sess = CString::new(session.as_str()).unwrap();
                let args = [tmux.as_ptr(), attach.as_ptr(), flag_t.as_ptr(), sess.as_ptr(), std::ptr::null()];
                unsafe { libc::execvp(tmux.as_ptr(), args.as_ptr()); }
                eprintln!("iyke chi attach: execvp tmux failed");
                std::process::exit(1);
            }
            #[cfg(not(unix))]
            {
                let _ = std::process::Command::new("tmux")
                    .args(["attach-session", "-t", &session])
                    .status();
            }
        }
    }
    Ok(())
}

fn require_at_most_one_target(
    pane: &Option<String>,
    session: &Option<String>,
    terminal: &Option<String>,
    label: &Option<String>,
) -> Result<()> {
    let count = pane.is_some() as u8
        + session.is_some() as u8
        + terminal.is_some() as u8
        + label.is_some() as u8;
    if count > 1 {
        return Err(anyhow!(
            "must supply at most one of: --pane, --session, --terminal, --label"
        ));
    }
    Ok(())
}

fn exactly_one_terminal_target(
    terminal: Option<String>,
    label: Option<String>,
    session: Option<String>,
) -> Result<String> {
    let targets = [terminal, label, session]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    if targets.len() != 1 {
        return Err(anyhow!(
            "must supply exactly one of: --terminal, --label, --session"
        ));
    }
    Ok(targets.into_iter().next().expect("one target"))
}

fn require_one(
    r#ref: &Option<String>,
    selector: &Option<String>,
    text: &Option<String>,
) -> Result<()> {
    let count = r#ref.is_some() as u8 + selector.is_some() as u8 + text.is_some() as u8;
    if count != 1 {
        return Err(anyhow!(
            "must supply exactly one of: --ref, --selector, --text"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_as_str_matches_wire_strings() {
        // The wire strings the shell's engine router (WP-07) matches on.
        assert_eq!(Engine::Webkit.as_str(), "webkit");
        assert_eq!(Engine::Chrome.as_str(), "chrome");
    }

    #[test]
    fn parses_scratchpad_set() {
        let cli = Cli::try_parse_from([
            "iyke",
            "scratchpad",
            "set",
            "--scope",
            "project:royalti-co",
            "--name",
            "handoff",
            "--body",
            "ready",
        ])
        .unwrap();
        match cli.command {
            Command::Scratchpad {
                action:
                    ScratchpadAction::Set {
                        scope,
                        name,
                        body,
                        body_file,
                    },
            } => {
                assert_eq!(scope.as_deref(), Some("project:royalti-co"));
                assert_eq!(name, "handoff");
                assert_eq!(body.as_deref(), Some("ready"));
                assert!(body_file.is_none());
            }
            _ => panic!("expected scratchpad set"),
        }
    }

    #[test]
    fn parses_terminal_session_addressing() {
        let read = Cli::try_parse_from([
            "iyke",
            "terminal-read",
            "--session",
            "session-123",
            "--bytes",
            "4096",
        ])
        .unwrap();
        match read.command {
            Command::TerminalRead {
                pane,
                session,
                bytes,
                raw,
                ..
            } => {
                assert!(pane.is_none());
                assert_eq!(session.as_deref(), Some("session-123"));
                assert_eq!(bytes, Some(4096));
                assert!(!raw);
            }
            _ => panic!("expected terminal-read"),
        }

        let send = Cli::try_parse_from([
            "iyke",
            "terminal-send",
            "hello",
            "--session",
            "session-123",
            "--key",
            "Enter",
        ])
        .unwrap();
        match send.command {
            Command::TerminalSend {
                text,
                keys,
                pane,
                session,
                ..
            } => {
                assert_eq!(text.as_deref(), Some("hello"));
                assert_eq!(keys, vec!["Enter"]);
                assert!(pane.is_none());
                assert_eq!(session.as_deref(), Some("session-123"));
            }
            _ => panic!("expected terminal-send"),
        }
    }

    #[test]
    fn parses_terminal_control_plane_commands() {
        let read = Cli::try_parse_from([
            "iyke",
            "terminal-read",
            "--terminal",
            "terminal-1",
            "--after",
            "42",
            "--mode",
            "screen",
        ])
        .unwrap();
        assert!(matches!(
            read.command,
            Command::TerminalRead {
                terminal: Some(ref id),
                after: Some(42),
                ref mode,
                ..
            } if id == "terminal-1" && mode == "screen"
        ));

        let wait = Cli::try_parse_from([
            "iyke",
            "terminal-wait",
            "--label",
            "reviewer",
            "--match",
            "READY",
        ])
        .unwrap();
        assert!(matches!(
            wait.command,
            Command::TerminalWait {
                label: Some(ref label),
                r#match: Some(ref pattern),
                ..
            } if label == "reviewer" && pattern == "READY"
        ));

        assert!(Cli::try_parse_from([
            "iyke",
            "terminal-lease",
            "acquire",
            "terminal-1",
            "--agent-id",
            "orchestrator",
        ])
        .is_ok());
        assert!(Cli::try_parse_from([
            "iyke",
            "tab",
            "activate",
            "--pane",
            "leaf-1",
            "--terminal",
            "terminal-1",
        ])
        .is_ok());
    }

    #[test]
    fn terminal_addressing_rejects_pane_and_session_together() {
        assert!(Cli::try_parse_from([
            "iyke",
            "terminal-read",
            "--pane",
            "leaf-1",
            "--session",
            "session-123"
        ])
        .is_err());
    }

    #[test]
    fn scratchpad_set_requires_exactly_one_body_source() {
        assert!(Cli::try_parse_from(["iyke", "scratchpad", "set", "--name", "handoff"]).is_err());
        assert!(Cli::try_parse_from([
            "iyke",
            "scratchpad",
            "set",
            "--name",
            "handoff",
            "--body",
            "ready",
            "--body-file",
            "body.md"
        ])
        .is_err());
    }

    #[test]
    fn parses_terminal_spawn_argv_after_separator() {
        // Everything after `--` is the command, hyphens and all — otherwise
        // `bun run dev --port 3000` would be eaten as iyke's own flags.
        let cli = Cli::try_parse_from([
            "iyke",
            "terminal-spawn",
            "--cwd",
            "/tmp/work",
            "--label",
            "builder",
            "--",
            "bun",
            "run",
            "dev",
            "--port",
            "3000",
        ])
        .unwrap();
        match cli.command {
            Command::TerminalSpawn {
                cwd, label, argv, ..
            } => {
                assert_eq!(cwd.as_deref(), Some("/tmp/work"));
                assert_eq!(label.as_deref(), Some("builder"));
                assert_eq!(argv, ["bun", "run", "dev", "--port", "3000"]);
            }
            _ => panic!("expected terminal-spawn"),
        }
    }

    #[test]
    fn terminal_spawn_lease_ttl_requires_lease_for() {
        // A TTL without an agent to lease for silently does nothing, which
        // reads as "I owned it for 5s" when in fact nothing was ever leased.
        assert!(Cli::try_parse_from(["iyke", "terminal-spawn", "--lease-ttl-ms", "5000"]).is_err());
        assert!(Cli::try_parse_from([
            "iyke",
            "terminal-spawn",
            "--lease-for",
            "orchestrator",
            "--lease-ttl-ms",
            "5000",
        ])
        .is_ok());
    }

    #[test]
    fn parses_terminal_kill_and_close_tab() {
        let cli =
            Cli::try_parse_from(["iyke", "terminal-kill", "reviewer", "--close-tab"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::TerminalKill {
                ref terminal,
                close_tab: true,
            } if terminal == "reviewer"
        ));
        // Tab retention is the default: killing leaves the exited tab so the
        // scrollback survives for a post-mortem.
        let keep = Cli::try_parse_from(["iyke", "terminal-kill", "reviewer"]).unwrap();
        assert!(matches!(
            keep.command,
            Command::TerminalKill {
                close_tab: false,
                ..
            }
        ));
    }

    #[test]
    fn parses_inbox_commands() {
        let list = Cli::try_parse_from([
            "iyke",
            "inbox",
            "list",
            "--agent-id",
            "orchestrator",
            "--ack",
        ])
        .unwrap();
        match list.command {
            Command::Inbox {
                action:
                    InboxAction::List {
                        agent_id,
                        ack,
                        since,
                        ..
                    },
            } => {
                assert_eq!(agent_id, "orchestrator");
                assert!(ack);
                assert_eq!(since, 0, "a fresh poll starts from the beginning");
            }
            _ => panic!("expected inbox list"),
        }

        let ack = Cli::try_parse_from(["iyke", "inbox", "ack", "--id", "a", "--id", "b"]).unwrap();
        match ack.command {
            Command::Inbox {
                action: InboxAction::Ack { ids },
            } => assert_eq!(ids, ["a", "b"]),
            _ => panic!("expected inbox ack"),
        }

        // Acking nothing is a no-op the shell would accept, but it is far
        // more likely to be a scripting mistake than an intent.
        assert!(Cli::try_parse_from(["iyke", "inbox", "ack"]).is_err());
    }

    #[test]
    fn parses_task_commands() {
        let create = Cli::try_parse_from([
            "iyke",
            "task",
            "create",
            "Ship the control plane",
            "--priority",
            "high",
        ])
        .unwrap();
        match create.command {
            Command::Task {
                action:
                    TaskAction::Create {
                        title, priority, ..
                    },
            } => {
                assert_eq!(title, "Ship the control plane");
                assert_eq!(priority.as_deref(), Some("high"));
            }
            _ => panic!("expected task create"),
        }

        let complete = Cli::try_parse_from([
            "iyke",
            "task",
            "complete",
            "task-1",
            "--task-result",
            "merged",
        ])
        .unwrap();
        assert!(matches!(
            complete.command,
            Command::Task {
                action: TaskAction::Complete { ref id, ref task_result, .. },
            } if id == "task-1" && task_result.as_deref() == Some("merged")
        ));
    }

    #[test]
    fn parses_chi_run() {
        let cli = Cli::try_parse_from([
            "iyke",
            "chi",
            "run",
            "claude-code",
            "--prompt",
            "hello",
            "--cwd",
            "/tmp/work",
            "--mode",
            "auto",
        ])
        .unwrap();
        match cli.command {
            Command::Chi {
                action:
                    ChiAction::Run {
                        engine_id,
                        prompt,
                        cwd,
                        mode,
                        ..
                    },
            } => {
                assert_eq!(engine_id, "claude-code");
                assert_eq!(prompt, "hello");
                assert_eq!(cwd.as_deref(), Some("/tmp/work"));
                assert_eq!(mode.as_deref(), Some("auto"));
            }
            _ => panic!("expected chi run"),
        }
    }

    #[test]
    fn parses_chi_resume_status_cancel() {
        let resume = Cli::try_parse_from([
            "iyke",
            "chi",
            "resume",
            "run-123",
            "--prompt",
            "continue",
        ])
        .unwrap();
        match resume.command {
            Command::Chi {
                action: ChiAction::Resume { run_id, prompt },
            } => {
                assert_eq!(run_id, "run-123");
                assert_eq!(prompt, "continue");
            }
            _ => panic!("expected chi resume"),
        }

        let status = Cli::try_parse_from(["iyke", "chi", "status", "run-123"]).unwrap();
        assert!(matches!(
            status.command,
            Command::Chi {
                action: ChiAction::Status { ref run_id },
            } if run_id == "run-123"
        ));

        let cancel = Cli::try_parse_from(["iyke", "chi", "cancel", "run-123"]).unwrap();
        assert!(matches!(
            cancel.command,
            Command::Chi {
                action: ChiAction::Cancel { ref run_id },
            } if run_id == "run-123"
        ));
    }

    #[test]
    fn parses_chi_list() {
        let cli = Cli::try_parse_from([
            "iyke",
            "chi",
            "list",
            "--engine",
            "claude-code",
            "--limit",
            "10",
        ])
        .unwrap();
        match cli.command {
            Command::Chi {
                action: ChiAction::List { engine, limit },
            } => {
                assert_eq!(engine.as_deref(), Some("claude-code"));
                assert_eq!(limit, 10);
            }
            _ => panic!("expected chi list"),
        }
    }

    #[test]
    fn chi_list_output_formatting() {
        let value = serde_json::json!([
            {
                "runId": "run-1",
                "engineId": "claude-code",
                "status": "running",
                "brief": "hello\nworld"
            },
            {
                "runId": "run-2",
                "engineId": "claude-code",
                "status": "done",
                "brief": null
            }
        ]);
        // Should not panic; JSON mode round-trips.
        let mut buf = Vec::new();
        {
            use std::io::Write;
            // Human formatting is side-effect-only; we just exercise it.
            output::print_chi_list(&value, output::Format::Human);
            output::print_chi_list(&value, output::Format::Json);
            write!(buf, "{}" , value).unwrap();
        }
        assert!(!buf.is_empty());
    }

    #[test]
    fn parses_chi_run_persistent() {
        let cli = Cli::try_parse_from([
            "iyke",
            "chi",
            "run",
            "claude-code",
            "--prompt",
            "hello",
            "--persistent",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Chi {
                action: ChiAction::Run {
                    ref engine_id,
                    persistent: true,
                    ..
                },
            } if engine_id == "claude-code"
        ));
    }

    #[test]
    fn parses_chi_attach() {
        let cli = Cli::try_parse_from(["iyke", "chi", "attach", "run-abc"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Chi {
                action: ChiAction::Attach { ref run_id },
            } if run_id == "run-abc"
        ));
    }

    #[test]
    fn parses_project_noun() {
        use crate::cmd::project::ProjectAction;

        let show = Cli::try_parse_from(["iyke", "project", "show"]).unwrap();
        assert!(matches!(
            show.command,
            Command::Project {
                action: ProjectAction::Show
            }
        ));

        let sections = Cli::try_parse_from(["iyke", "project", "sections"]).unwrap();
        assert!(matches!(
            sections.command,
            Command::Project {
                action: ProjectAction::Sections
            }
        ));

        let switch = Cli::try_parse_from(["iyke", "project", "switch", "C:/repo"]).unwrap();
        match switch.command {
            Command::Project {
                action: ProjectAction::Switch { path },
            } => assert_eq!(path, "C:/repo"),
            _ => panic!("expected project switch"),
        }
    }

    #[test]
    fn parses_ngwa_noun() {
        use crate::cmd::ngwa::NgwaAction;

        for (argv, want) in [
            (vec!["iyke", "ngwa", "installed"], "installed"),
            (vec!["iyke", "ngwa", "store"], "store"),
            (vec!["iyke", "ngwa", "scopes"], "scopes"),
            (vec!["iyke", "ngwa", "health"], "health"),
        ] {
            let cli = Cli::try_parse_from(argv).unwrap();
            let ok = match (cli.command, want) {
                (
                    Command::Ngwa {
                        action: NgwaAction::Installed,
                    },
                    "installed",
                ) => true,
                (Command::Ngwa { action: NgwaAction::Store }, "store") => true,
                (Command::Ngwa { action: NgwaAction::Scopes }, "scopes") => true,
                (
                    Command::Ngwa {
                        action: NgwaAction::Health,
                    },
                    "health",
                ) => true,
                _ => false,
            };
            assert!(ok, "parse failed for {want}");
        }

        let item = Cli::try_parse_from(["iyke", "ngwa", "item", "com.ikenga.iyke"]).unwrap();
        match item.command {
            Command::Ngwa {
                action: NgwaAction::Item { id },
            } => assert_eq!(id, "com.ikenga.iyke"),
            _ => panic!("expected ngwa item"),
        }
    }
}
