//! The `wb` command line: what a script or an agent can do with the Workbench's terminals. Specification, exit codes
//! and the JSON shapes: docs/design/wb-automation.md. The binary (`src/bin/wb.rs`) only calls `run`.
//!
//! Hardening rules as for the rest of the crate: no `unwrap`/`expect`, no truncating casts, every wait is bounded by
//! what the user asked for, and nothing here widens what the connection is allowed to do (an agent stays an agent).
#![cfg(windows)]
use crate::{
    client::Client,
    proto::{b64_decode, ClientKind, Duration as GrantFor, Request, Response, SessionInfo, Shell, WatchCursor},
    win,
};
use serde_json::{json, Value};
use std::{
    io::Write,
    path::PathBuf,
    time::{Duration, Instant},
};

pub const EXIT_OK: i32 = 0;
pub const EXIT_FAILED: i32 = 1;
pub const EXIT_UNREACHABLE: i32 = 2;
pub const EXIT_NO_SUCH_TERMINAL: i32 = 3;
pub const EXIT_TIMED_OUT: i32 = 4;
pub const EXIT_NOT_PERMITTED: i32 = 5;
pub const EXIT_ENDED: i32 = 6;
pub const EXIT_USAGE: i32 = 64;

const USAGE: &str = "usage: wb [--json] [--agent NAME] COMMAND
  ls | new [label] [--shell cmd|powershell|pwsh] [--cwd DIR] [--extended-history] | attach ID [--fresh]
  read ID [FROM] | send ID TEXT... | end ID
  watch ID... [--from now|start|N] | wait ID [--for TEXT] [--idle SECS] [--exit] [--timeout SECS] [--from N]
  grant ID --to NAME [--for 1m|15m|1h|1d] [--keep] | grants ID | revoke ID --to NAME
  draft ID | accept ID | discard ID | propose ID TEXT... | type ID TEXT...
  log ID start|stop|status    (record a terminal's output to a file, or ask where that file is)
  restore ID | restore --all    (recreate terminals that did not survive a restart)";

/// The command line after the global options are taken out.
struct Args {
    items: Vec<String>,
}
impl Args {
    /// A `--flag` that takes no value; removed when present.
    fn flag(&mut self, name: &str) -> bool {
        match self.items.iter().position(|a| a == name) {
            Some(i) => {
                self.items.remove(i);
                true
            }
            None => false,
        }
    }
    /// A `--flag VALUE`; `Err` when the value is missing.
    fn value(&mut self, name: &str) -> Result<Option<String>, ()> {
        let Some(i) = self.items.iter().position(|a| a == name) else { return Ok(None) };
        if i + 1 >= self.items.len() {
            return Err(());
        }
        let v = self.items.remove(i + 1);
        self.items.remove(i);
        Ok(Some(v))
    }
    fn rest(&self) -> &[String] {
        &self.items
    }
}

struct Ctx {
    json: bool,
    pipe: String,
    data_dir: PathBuf,
    client: Client,
}

/// Run the command line (everything after the program name) and return the process exit code.
pub fn run(raw: Vec<String>) -> i32 {
    let mut args = Args { items: raw };
    let json = args.flag("--json");
    let (Ok(data_dir), Ok(pipe), Ok(agent)) = (args.value("--data-dir"), args.value("--pipe-name"), args.value("--agent")) else { return usage(json, "a flag is missing its value") };
    let data_dir: PathBuf = data_dir.map(Into::into).unwrap_or_else(win::default_data_dir);
    let pipe = pipe.unwrap_or_else(win::default_pipe_name);
    if win::self_elevated() {
        return report(json, "elevated_client", "Workbench runs as your standard user; run wb from a non-elevated shell.", EXIT_UNREACHABLE);
    }
    let Some(command) = args.items.first().cloned() else { return usage(json, "no command") };
    args.items.remove(0);
    let connected = match &agent {
        Some(name) => Client::connect_agent(&pipe, &data_dir, name),
        None => Client::connect(&pipe, &data_dir, ClientKind::Cli),
    };
    let client = match connected {
        Ok(c) => c,
        Err(e) => return report(json, "unreachable", &format!("cannot reach the Workbench daemon ({e}); is wb-daemon running?"), EXIT_UNREACHABLE),
    };
    let mut ctx = Ctx { json, pipe, data_dir, client };
    match command.as_str() {
        "ls" => cmd_ls(&mut ctx),
        "new" => cmd_new(&mut ctx, args),
        "attach" => cmd_attach(ctx, args),
        "read" => cmd_read(&mut ctx, args),
        "send" => cmd_send(&mut ctx, args, true),
        "type" => cmd_send(&mut ctx, args, false),
        "end" => simple(&mut ctx, args, |id| Request::End { session: id }),
        "restore" => cmd_restore(&mut ctx, args),
        "propose" => cmd_propose(&mut ctx, args),
        "watch" => cmd_watch(&mut ctx, args),
        "wait" => cmd_wait(&mut ctx, args),
        "grant" => cmd_grant(&mut ctx, args),
        "grants" => cmd_grants(&mut ctx, args),
        "revoke" => cmd_revoke(&mut ctx, args),
        "draft" => cmd_draft(&mut ctx, args),
        "accept" => simple(&mut ctx, args, |id| Request::AcceptDraft { session: id }),
        "discard" => simple(&mut ctx, args, |id| Request::DiscardDraft { session: id }),
        "log" => cmd_log(&mut ctx, args),
        _ => usage(json, "unknown command"),
    }
}

// ---- output ---------------------------------------------------------------------------------------------------

fn print_json(v: &Value) {
    println!("{v}");
}

/// Print the failure (a line on standard error; a JSON object there with `--json`) and return the exit code.
fn report(json: bool, code: &str, message: &str, exit: i32) -> i32 {
    if json {
        eprintln!("{}", json!({ "error": { "code": code, "message": message } }));
    } else {
        eprintln!("{code}: {message}");
    }
    exit
}

fn usage(json: bool, why: &str) -> i32 {
    if json {
        report(json, "usage", why, EXIT_USAGE)
    } else {
        eprintln!("{why}\n{USAGE}");
        EXIT_USAGE
    }
}

/// The exit code for a service error code.
fn exit_for(code: &str) -> i32 {
    match code {
        "no_such_session" => EXIT_NO_SUCH_TERMINAL,
        "forbidden" | "no_grant" | "gate_failed" | "invalid_phrase" => EXIT_NOT_PERMITTED,
        _ => EXIT_FAILED,
    }
}

fn service_error(ctx: &Ctx, code: &str, message: &str) -> i32 {
    report(ctx.json, code, message, exit_for(code))
}

fn lost_connection(ctx: &Ctx, e: &std::io::Error) -> i32 {
    report(ctx.json, "connection_lost", &format!("the connection to the Workbench daemon failed ({e})"), EXIT_UNREACHABLE)
}

/// One request whose only reply that matters is "ok".
fn call_ok(ctx: &mut Ctx, req: &Request) -> Result<(), i32> {
    match ctx.client.call(req) {
        Ok(Response::Error { code, message }) => Err(service_error(ctx, &code, &message)),
        Ok(_) => Ok(()),
        Err(e) => Err(lost_connection(ctx, &e)),
    }
}

fn session_arg(ctx: &Ctx, args: &Args) -> Result<String, i32> {
    match args.rest().first() {
        Some(id) if !id.starts_with("--") => Ok(id.clone()),
        _ => Err(usage(ctx.json, "a terminal id is needed")),
    }
}

/// The text after the id, joined with spaces (the words of `send`, `propose`, `type`).
fn words(args: &Args) -> String {
    args.rest().get(1..).unwrap_or(&[]).join(" ")
}

/// The end of a terminal's output so far (where "new output" begins), or the exit code to give.
fn output_end(ctx: &mut Ctx, id: &str) -> Result<u64, i32> {
    match ctx.client.call(&Request::Read { session: id.to_owned(), from: u64::MAX }) {
        Ok(Response::Output { next, .. }) => Ok(next),
        Ok(Response::Error { code, message }) => Err(service_error(ctx, &code, &message)),
        Ok(_) => Err(report(ctx.json, "unexpected_reply", "the daemon sent an unexpected reply", EXIT_FAILED)),
        Err(e) => Err(lost_connection(ctx, &e)),
    }
}

// ---- simple commands --------------------------------------------------------------------------------------------

fn session_json(s: &SessionInfo) -> Value {
    serde_json::to_value(s).unwrap_or(Value::Null)
}

fn cmd_ls(ctx: &mut Ctx) -> i32 {
    match ctx.client.call(&Request::List {}) {
        Ok(Response::Sessions { items }) => {
            if ctx.json {
                print_json(&json!({ "sessions": items.iter().map(session_json).collect::<Vec<_>>() }));
            } else {
                for s in items {
                    println!("{}\t{}\t{}\t{}x{}\thost={}\thistory={}MiB\t{}", s.id, s.state, if s.persistent { "persistent" } else { "in-process" }, s.cols, s.rows, s.host_pid, s.history_mib, s.label);
                }
            }
            EXIT_OK
        }
        Ok(Response::Error { code, message }) => service_error(ctx, &code, &message),
        Ok(_) => report(ctx.json, "unexpected_reply", "the daemon sent an unexpected reply", EXIT_FAILED),
        Err(e) => lost_connection(ctx, &e),
    }
}

fn cmd_new(ctx: &mut Ctx, mut args: Args) -> i32 {
    let (Ok(shell), Ok(cwd)) = (args.value("--shell"), args.value("--cwd")) else { return usage(ctx.json, "a flag is missing its value") };
    let extended = args.flag("--extended-history");
    let shell = match shell.as_deref() {
        None | Some("cmd") => Shell::Cmd,
        Some("powershell") => Shell::PowerShell,
        Some("pwsh") => Shell::Pwsh,
        Some(_) => return usage(ctx.json, "unknown shell; use cmd, powershell or pwsh"),
    };
    let label = args.rest().first().filter(|a| !a.starts_with("--")).cloned();
    match ctx.client.call(&Request::NewTerminal { cols: 120, rows: 30, label, shell: Some(shell), extended_history: Some(extended), cwd }) {
        Ok(Response::Created { session }) => {
            if ctx.json {
                print_json(&json!({ "id": session.id, "session": session_json(&session) }));
            } else {
                println!("{}", session.id);
            }
            EXIT_OK
        }
        Ok(Response::Error { code, message }) => service_error(ctx, &code, &message),
        Ok(_) => report(ctx.json, "unexpected_reply", "the daemon sent an unexpected reply", EXIT_FAILED),
        Err(e) => lost_connection(ctx, &e),
    }
}

fn cmd_attach(ctx: Ctx, args: Args) -> i32 {
    let Some(id) = args.rest().first().filter(|a| !a.starts_with("--")).cloned() else { return usage(ctx.json, "a terminal id is needed") };
    let fresh = args.rest().iter().any(|a| a == "--fresh");
    let (pipe, dir) = (ctx.pipe.clone(), ctx.data_dir.clone());
    // `attach` takes over this console and opens its own connections.
    drop(ctx);
    crate::attach::run(&pipe, &dir, &id, fresh)
}

fn cmd_read(ctx: &mut Ctx, args: Args) -> i32 {
    let id = match session_arg(ctx, &args) {
        Ok(id) => id,
        Err(code) => return code,
    };
    let from = args.rest().get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    match ctx.client.call(&Request::Read { session: id, from }) {
        Ok(Response::Output { data, next, skipped }) => {
            let bytes = b64_decode(&data).unwrap_or_default();
            if ctx.json {
                print_json(&json!({ "text": String::from_utf8_lossy(&bytes), "data": data, "next": next, "skipped": skipped }));
            } else {
                print!("{}", String::from_utf8_lossy(&bytes));
                eprintln!("\n[next={next} skipped={skipped}]");
            }
            EXIT_OK
        }
        Ok(Response::Error { code, message }) => service_error(ctx, &code, &message),
        Ok(_) => report(ctx.json, "unexpected_reply", "the daemon sent an unexpected reply", EXIT_FAILED),
        Err(e) => lost_connection(ctx, &e),
    }
}

/// `send` (a person types) and `type` (an agent types, with a grant).
fn cmd_send(ctx: &mut Ctx, args: Args, human: bool) -> i32 {
    let id = match session_arg(ctx, &args) {
        Ok(id) => id,
        Err(code) => return code,
    };
    let text = format!("{}\r", words(&args));
    // Where the output stood before the text went in, so `wait --from` cannot miss what the text causes.
    let from = if ctx.json && human {
        match output_end(ctx, &id) {
            Ok(n) => Some(n),
            Err(code) => return code,
        }
    } else {
        None
    };
    let req = if human { Request::Input { session: id, text } } else { Request::AgentInput { session: id, text } };
    match call_ok(ctx, &req) {
        Ok(()) => {
            if ctx.json {
                print_json(&match from {
                    Some(n) => json!({ "ok": true, "from": n }),
                    None => json!({ "ok": true }),
                });
            } else {
                println!("ok");
            }
            EXIT_OK
        }
        Err(code) => code,
    }
}

fn cmd_propose(ctx: &mut Ctx, args: Args) -> i32 {
    let id = match session_arg(ctx, &args) {
        Ok(id) => id,
        Err(code) => return code,
    };
    let req = Request::Propose { session: id, text: words(&args) };
    simple_result(ctx, &req)
}

/// `log ID start|stop|status`: record a terminal's output to a file, stop recording, or ask where the file is and how
/// far it got. The file is the service's choice (under the service's data folder), never the caller's.
fn cmd_log(ctx: &mut Ctx, args: Args) -> i32 {
    let mut rest = args.rest().iter().filter(|a| !a.starts_with("--"));
    let (Some(id), Some(what)) = (rest.next().cloned(), rest.next().cloned()) else { return usage(ctx.json, "log needs a terminal id and start, stop or status") };
    if rest.next().is_some() {
        return usage(ctx.json, "log takes one of start, stop or status");
    }
    let req = match what.as_str() {
        "start" => Request::LogStart { session: id },
        "stop" => Request::LogStop { session: id },
        "status" => Request::LogStatus { session: id },
        _ => return usage(ctx.json, "log takes one of start, stop or status"),
    };
    match ctx.client.call(&req) {
        Ok(Response::Logging { on, path, bytes, stopped_reason }) => {
            if ctx.json {
                print_json(&json!({ "on": on, "path": path, "bytes": bytes, "stopped_reason": stopped_reason }));
            } else {
                let state = if on { "on" } else { "off" };
                match (&path, &stopped_reason) {
                    (Some(p), Some(r)) => println!("{state}\t{p}\t{bytes} bytes\tstopped: {r}"),
                    (Some(p), None) => println!("{state}\t{p}\t{bytes} bytes"),
                    (None, Some(r)) => println!("{state}\t{bytes} bytes\tstopped: {r}"),
                    (None, None) => println!("{state}\t{bytes} bytes"),
                }
            }
            EXIT_OK
        }
        Ok(Response::Error { code, message }) => service_error(ctx, &code, &message),
        Ok(_) => report(ctx.json, "unexpected_reply", "the daemon sent an unexpected reply", EXIT_FAILED),
        Err(e) => lost_connection(ctx, &e),
    }
}

/// A command with just an id, whose only reply that matters is "ok".
fn simple(ctx: &mut Ctx, args: Args, build: impl FnOnce(String) -> Request) -> i32 {
    let id = match session_arg(ctx, &args) {
        Ok(id) => id,
        Err(code) => return code,
    };
    simple_result(ctx, &build(id))
}

fn simple_result(ctx: &mut Ctx, req: &Request) -> i32 {
    match call_ok(ctx, req) {
        Ok(()) => {
            if ctx.json {
                print_json(&json!({ "ok": true }));
            } else {
                println!("ok");
            }
            EXIT_OK
        }
        Err(code) => code,
    }
}

// ---- restore (people only) -----------------------------------------------------------------------------------------

/// `restore ID` recreates one terminal that did not survive a restart; `restore --all` every such terminal.
fn cmd_restore(ctx: &mut Ctx, mut args: Args) -> i32 {
    let all = args.flag("--all");
    if !all {
        let id = match session_arg(ctx, &args) {
            Ok(id) => id,
            Err(code) => return code,
        };
        return simple_result(ctx, &Request::Restore { session: id, shell: None });
    }
    let lost: Vec<String> = match ctx.client.call(&Request::List {}) {
        Ok(Response::Sessions { items }) => items.into_iter().filter(|s| s.state == "lost").map(|s| s.id).collect(),
        Ok(Response::Error { code, message }) => return service_error(ctx, &code, &message),
        Ok(_) => return report(ctx.json, "unexpected_reply", "the daemon sent an unexpected reply", EXIT_FAILED),
        Err(e) => return lost_connection(ctx, &e),
    };
    let (mut restored, mut failed) = (Vec::new(), Vec::new());
    for id in lost {
        match ctx.client.call(&Request::Restore { session: id.clone(), shell: None }) {
            Ok(Response::Created { .. }) => restored.push(id),
            Ok(Response::Error { code, message }) => failed.push((id, code, message)),
            Ok(_) => failed.push((id, "unexpected_reply".to_owned(), "unexpected reply".to_owned())),
            Err(e) => return lost_connection(ctx, &e),
        }
    }
    if ctx.json {
        print_json(&json!({ "restored": restored, "failed": failed.iter().map(|(id, code, message)| json!({ "id": id, "code": code, "message": message })).collect::<Vec<_>>() }));
    } else {
        for id in &restored {
            println!("restored {id}");
        }
        for (id, code, message) in &failed {
            eprintln!("{id}: {code}: {message}");
        }
    }
    if failed.is_empty() { EXIT_OK } else { EXIT_FAILED }
}

// ---- grants and drafts (people only; the service refuses an agent) -----------------------------------------------

fn parse_grant_for(s: &str) -> Option<GrantFor> {
    match s {
        "1m" => Some(GrantFor::OneMinute),
        "15m" => Some(GrantFor::FifteenMinutes),
        "1h" => Some(GrantFor::OneHour),
        "1d" => Some(GrantFor::OneDay),
        _ => None,
    }
}

fn cmd_grant(ctx: &mut Ctx, mut args: Args) -> i32 {
    let (Ok(agent), Ok(duration)) = (args.value("--to"), args.value("--for")) else { return usage(ctx.json, "a flag is missing its value") };
    let keep = args.flag("--keep");
    let Some(agent) = agent else { return usage(ctx.json, "--to NAME is needed (the agent to give access to)") };
    let Some(duration) = duration.as_deref().map_or(Some(GrantFor::FifteenMinutes), parse_grant_for) else { return usage(ctx.json, "--for takes 1m, 15m, 1h or 1d") };
    let id = match session_arg(ctx, &args) {
        Ok(id) => id,
        Err(code) => return code,
    };
    // The phrase comes from the environment, not the command line, which other programs can read.
    let gate = std::env::var("WB_GATE").unwrap_or_default();
    if !keep && !ctx.json {
        // A grant that is not kept lasts only while a person is attached; this command detaches as soon as it ends.
        eprintln!("note: without --keep the grant works only while a person is attached (the Workbench app is open)");
    }
    simple_result(ctx, &Request::GrantCreate { agent, session: id, duration, keep_after_detach: keep, gate })
}

fn cmd_grants(ctx: &mut Ctx, args: Args) -> i32 {
    let id = match session_arg(ctx, &args) {
        Ok(id) => id,
        Err(code) => return code,
    };
    match ctx.client.call(&Request::Grants { session: id }) {
        Ok(Response::Grants { items }) => {
            if ctx.json {
                print_json(&json!({ "grants": items.iter().map(|g| json!({ "agent": g.agent, "remaining_ms": g.remaining_ms, "keep_after_detach": g.keep_after_detach })).collect::<Vec<_>>() }));
            } else {
                for g in items {
                    println!("{}\t{} s left\t{}", g.agent, g.remaining_ms / 1000, if g.keep_after_detach { "kept after detach" } else { "ends when you detach" });
                }
            }
            EXIT_OK
        }
        Ok(Response::Error { code, message }) => service_error(ctx, &code, &message),
        Ok(_) => report(ctx.json, "unexpected_reply", "the daemon sent an unexpected reply", EXIT_FAILED),
        Err(e) => lost_connection(ctx, &e),
    }
}

fn cmd_revoke(ctx: &mut Ctx, mut args: Args) -> i32 {
    let Ok(agent) = args.value("--to") else { return usage(ctx.json, "--to is missing its value") };
    let Some(agent) = agent else { return usage(ctx.json, "--to NAME is needed (the agent to take access from)") };
    let id = match session_arg(ctx, &args) {
        Ok(id) => id,
        Err(code) => return code,
    };
    simple_result(ctx, &Request::GrantRevoke { agent, session: id })
}

fn cmd_draft(ctx: &mut Ctx, args: Args) -> i32 {
    let id = match session_arg(ctx, &args) {
        Ok(id) => id,
        Err(code) => return code,
    };
    match ctx.client.call(&Request::GetDraft { session: id }) {
        Ok(Response::Draft { draft }) => {
            if ctx.json {
                print_json(&json!({ "draft": draft.map(|d| json!({ "agent": d.agent, "text": d.text, "multiline": d.multiline })) }));
            } else if let Some(d) = draft {
                println!("{}: {}", d.agent, d.text);
            }
            EXIT_OK
        }
        Ok(Response::Error { code, message }) => service_error(ctx, &code, &message),
        Ok(_) => report(ctx.json, "unexpected_reply", "the daemon sent an unexpected reply", EXIT_FAILED),
        Err(e) => lost_connection(ctx, &e),
    }
}

// ---- watch and wait ---------------------------------------------------------------------------------------------

/// One chunk of output for a terminal.
struct Chunk {
    session: String,
    from: u64,
    next: u64,
    skipped: u64,
    data: Vec<u8>,
}

/// Where new output comes from: the service's `Watch` (pushed as it appears) or, for a connection that may not
/// watch (an agent), reads at a short interval.
struct Source {
    polling: bool,
}
impl Source {
    /// Output beyond each cursor, waiting up to `wait` for some; and the sessions that no longer exist.
    fn next(&mut self, ctx: &mut Ctx, cursors: &[WatchCursor], wait: Duration) -> Result<(Vec<Chunk>, Vec<String>), i32> {
        if !self.polling {
            let wait_ms = u64::try_from(wait.as_millis()).unwrap_or(u64::MAX);
            match ctx.client.call(&Request::Watch { cursors: cursors.to_vec(), wait_ms }) {
                Ok(Response::Watch { items, gone }) => {
                    let chunks = items.into_iter().map(|i| Chunk { session: i.session, from: i.from, next: i.next, skipped: i.skipped, data: b64_decode(&i.data).unwrap_or_default() }).collect();
                    return Ok((chunks, gone));
                }
                Ok(Response::Error { code, .. }) if code == "forbidden" => self.polling = true,
                Ok(Response::Error { code, message }) => return Err(service_error(ctx, &code, &message)),
                Ok(_) => return Err(report(ctx.json, "unexpected_reply", "the daemon sent an unexpected reply", EXIT_FAILED)),
                Err(e) => return Err(lost_connection(ctx, &e)),
            }
        }
        // Polling: look now, and if nothing is new look again shortly, until the time is up.
        let deadline = Instant::now() + wait;
        loop {
            let (mut chunks, mut gone) = (Vec::new(), Vec::new());
            for c in cursors {
                match ctx.client.call(&Request::Read { session: c.session.clone(), from: c.from }) {
                    Ok(Response::Output { data, next, skipped }) => {
                        let bytes = b64_decode(&data).unwrap_or_default();
                        if !bytes.is_empty() || skipped > 0 {
                            chunks.push(Chunk { session: c.session.clone(), from: c.from, next, skipped, data: bytes });
                        }
                    }
                    Ok(Response::Error { code, .. }) if code == "no_such_session" => gone.push(c.session.clone()),
                    Ok(Response::Error { code, message }) => return Err(service_error(ctx, &code, &message)),
                    Ok(_) => return Err(report(ctx.json, "unexpected_reply", "the daemon sent an unexpected reply", EXIT_FAILED)),
                    Err(e) => return Err(lost_connection(ctx, &e)),
                }
            }
            if !chunks.is_empty() || !gone.is_empty() || Instant::now() >= deadline {
                return Ok((chunks, gone));
            }
            std::thread::sleep(Duration::from_millis(100).min(deadline.saturating_duration_since(Instant::now())));
        }
    }
}

/// `--from` for watch and wait: `now` (the end of the output so far), `start`, or an offset.
fn start_offset(ctx: &mut Ctx, id: &str, from: Option<&str>, default_now: bool) -> Result<u64, i32> {
    match from {
        Some("start") => Ok(0),
        Some("now") => output_end(ctx, id),
        Some(n) => n.parse().map_err(|_| usage(ctx.json, "--from takes now, start or a number")),
        None if default_now => output_end(ctx, id),
        None => Ok(0),
    }
}

fn cmd_watch(ctx: &mut Ctx, mut args: Args) -> i32 {
    let Ok(from) = args.value("--from") else { return usage(ctx.json, "--from is missing its value") };
    let ids: Vec<String> = args.rest().iter().filter(|a| !a.starts_with("--")).cloned().collect();
    if ids.is_empty() || ids.len() > crate::proto::MAX_WATCH_CURSORS {
        return usage(ctx.json, "give between 1 and 64 terminal ids");
    }
    if ids.len() > 1 && !ctx.json {
        return usage(ctx.json, "watching several terminals needs --json (one line per chunk)");
    }
    let mut cursors = Vec::new();
    for id in &ids {
        // A terminal that is not there at all is "no such terminal" (3); one that ends while watched is 6.
        match terminal_state(ctx, id) {
            Ok(Some(_)) => {}
            Ok(None) => return report(ctx.json, "no_such_session", &format!("no terminal {id}"), EXIT_NO_SUCH_TERMINAL),
            Err(code) => return code,
        }
        match start_offset(ctx, id, from.as_deref(), true) {
            Ok(n) => cursors.push(WatchCursor { session: id.clone(), from: n }),
            Err(code) => return code,
        }
    }
    let mut source = Source { polling: false };
    let mut stdout = std::io::stdout();
    loop {
        let (chunks, gone) = match source.next(ctx, &cursors, Duration::from_millis(1000)) {
            Ok(r) => r,
            Err(code) => return code,
        };
        for chunk in chunks {
            if let Some(cur) = cursors.iter_mut().find(|c| c.session == chunk.session && c.from == chunk.from) {
                cur.from = chunk.next;
            }
            if ctx.json {
                print_json(&json!({ "session": chunk.session, "from": chunk.from, "next": chunk.next, "skipped": chunk.skipped, "text": String::from_utf8_lossy(&chunk.data), "data": crate::proto::b64_encode(&chunk.data) }));
            } else if stdout.write_all(&chunk.data).and_then(|()| stdout.flush()).is_err() {
                return EXIT_OK; // the reader went away (a closed pipe)
            }
        }
        if let Some(missing) = gone.first() {
            return report(ctx.json, "terminal_ended", &format!("terminal {missing} ended"), EXIT_ENDED);
        }
    }
}

/// The state the service lists for a terminal, or `None` when there is no such terminal.
fn terminal_state(ctx: &mut Ctx, id: &str) -> Result<Option<String>, i32> {
    match ctx.client.call(&Request::List {}) {
        Ok(Response::Sessions { items }) => Ok(items.into_iter().find(|s| s.id == id).map(|s| s.state)),
        Ok(Response::Error { code, message }) => Err(service_error(ctx, &code, &message)),
        Ok(_) => Err(report(ctx.json, "unexpected_reply", "the daemon sent an unexpected reply", EXIT_FAILED)),
        Err(e) => Err(lost_connection(ctx, &e)),
    }
}

/// Is `needle` in `haystack`? (Literal bytes.)
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

fn print_wait_result(json: bool, reason: &str, next: u64, waited_ms: u64) {
    if json {
        print_json(&json!({ "reason": reason, "next": next, "waited_ms": waited_ms }));
    } else {
        println!("{reason}");
    }
}

fn cmd_wait(ctx: &mut Ctx, mut args: Args) -> i32 {
    let (Ok(needle), Ok(idle), Ok(timeout), Ok(from)) = (args.value("--for"), args.value("--idle"), args.value("--timeout"), args.value("--from")) else { return usage(ctx.json, "a flag is missing its value") };
    let until_exit = args.flag("--exit");
    let secs = |v: &Option<String>| -> Result<Option<Duration>, ()> {
        match v {
            None => Ok(None),
            Some(s) => s.parse::<f64>().ok().filter(|n| n.is_finite() && *n >= 0.0 && *n < 86_400.0 * 30.0).map(|n| Some(Duration::from_secs_f64(n))).ok_or(()),
        }
    };
    let (Ok(idle), Ok(timeout)) = (secs(&idle), secs(&timeout)) else { return usage(ctx.json, "--idle and --timeout take a number of seconds") };
    if needle.is_none() && idle.is_none() && !until_exit {
        return usage(ctx.json, "say what to wait for: --for TEXT, --idle SECS or --exit");
    }
    let id = match session_arg(ctx, &args) {
        Ok(id) => id,
        Err(code) => return code,
    };
    match terminal_state(ctx, &id) {
        Ok(Some(_)) => {}
        Ok(None) if until_exit => {
            print_wait_result(ctx.json, "ended", 0, 0);
            return EXIT_OK;
        }
        Ok(None) => return report(ctx.json, "no_such_session", &format!("no terminal {id}"), EXIT_NO_SUCH_TERMINAL),
        Err(code) => return code,
    }
    let start = match start_offset(ctx, &id, from.as_deref(), true) {
        Ok(n) => n,
        Err(code) => return code,
    };
    let needle = needle.map(String::into_bytes);
    let started = Instant::now();
    let mut last_output = started;
    let mut cursor = start;
    // Enough of the end of what was seen to find a needle split across two chunks.
    let mut tail: Vec<u8> = Vec::new();
    let keep = needle.as_ref().map_or(0, Vec::len).saturating_sub(1);
    let mut source = Source { polling: false };
    let done = |ctx: &Ctx, reason: &str, next: u64| -> i32 {
        print_wait_result(ctx.json, reason, next, u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
        EXIT_OK
    };
    loop {
        // Never sleep past the earliest thing that could end the wait.
        let mut wait = Duration::from_millis(1000);
        if let Some(t) = timeout {
            wait = wait.min(t.saturating_sub(started.elapsed()));
        }
        if let Some(i) = idle {
            wait = wait.min(i.saturating_sub(last_output.elapsed()));
        }
        let (chunks, gone) = match source.next(ctx, &[WatchCursor { session: id.clone(), from: cursor }], wait) {
            Ok(r) => r,
            Err(code) => return code,
        };
        for chunk in chunks {
            cursor = chunk.next;
            last_output = Instant::now();
            if let Some(n) = &needle {
                tail.extend_from_slice(&chunk.data);
                if contains(&tail, n) {
                    return done(ctx, "text", cursor);
                }
                let cut = tail.len().saturating_sub(keep);
                tail.drain(..cut);
            }
        }
        if !gone.is_empty() {
            return if until_exit {
                done(ctx, "ended", cursor)
            } else {
                report(ctx.json, "terminal_ended", &format!("terminal {id} ended"), EXIT_ENDED)
            };
        }
        if idle.is_some_and(|i| last_output.elapsed() >= i) {
            return done(ctx, "idle", cursor);
        }
        if until_exit {
            match terminal_state(ctx, &id) {
                Ok(Some(state)) if state != "exited" && state != "lost" => {}
                Ok(_) => return done(ctx, "ended", cursor),
                Err(code) => return code,
            }
        }
        if timeout.is_some_and(|t| started.elapsed() >= t) {
            return report(ctx.json, "timed_out", "gave up waiting", EXIT_TIMED_OUT);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Args {
        Args { items: items.iter().map(|s| (*s).to_owned()).collect() }
    }

    #[test]
    fn flags_and_values_are_taken_out_wherever_they_are() {
        let mut a = args(&["send", "--json", "ab12", "--agent", "claude", "hello"]);
        assert!(a.flag("--json"));
        assert!(!a.flag("--json"));
        assert_eq!(a.value("--agent"), Ok(Some("claude".to_owned())));
        assert_eq!(a.value("--agent"), Ok(None));
        assert_eq!(a.rest(), ["send", "ab12", "hello"]);
        let mut b = args(&["--agent"]);
        assert_eq!(b.value("--agent"), Err(()), "a value is missing");
    }

    #[test]
    fn service_errors_map_to_distinct_exit_codes() {
        assert_eq!(exit_for("no_such_session"), EXIT_NO_SUCH_TERMINAL);
        for code in ["forbidden", "no_grant", "gate_failed", "invalid_phrase"] {
            assert_eq!(exit_for(code), EXIT_NOT_PERMITTED, "{code}");
        }
        assert_eq!(exit_for("host_error"), EXIT_FAILED);
        let all = [EXIT_OK, EXIT_FAILED, EXIT_UNREACHABLE, EXIT_NO_SUCH_TERMINAL, EXIT_TIMED_OUT, EXIT_NOT_PERMITTED, EXIT_ENDED, EXIT_USAGE];
        let mut sorted = all.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), all.len(), "every outcome has its own code");
    }

    #[test]
    fn grant_durations_parse_exactly() {
        assert_eq!(parse_grant_for("15m"), Some(GrantFor::FifteenMinutes));
        assert_eq!(parse_grant_for("1d"), Some(GrantFor::OneDay));
        assert_eq!(parse_grant_for("2h"), None);
    }

    #[test]
    fn a_needle_is_found_across_chunk_boundaries_literally() {
        assert!(contains(b"abcDONE-1def", b"DONE-1"));
        assert!(!contains(b"abc", b""));
        assert!(!contains(b"DONE", b"DONE-1"));
    }
}
