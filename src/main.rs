//! baa, "uh-oh or all good": after an agent's turn, check that its project
//! still compiles and show the verdict as `$baa` in Herdr's agent sidebar.

mod detect;
mod herdr;
mod parse;
mod run;
mod state;

use std::env;
use std::error::Error;
use std::io::{self, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use herdr::{Herdr, Pane};
use parse::Verdict;
use run::Outcome;
use state::{Project, State};

type Result<T = ()> = std::result::Result<T, Box<dyn Error>>;

/// Extra life for the `…` token beyond the check's own timeout, so it can't outlive a dead worker.
const CHECKING_GRACE: Duration = Duration::from_secs(10);

const USAGE: &str = "usage: baa <on-event | check | open-details | details | clear>";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let result = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["on-event"] => on_event(),
        ["check"] => check_focused(),
        ["details"] => details(),
        ["open-details"] => open_details(),
        ["clear"] => clear(),
        // Internal: started detached by the commands above.
        ["worker", root] => worker(Path::new(root)),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            match State::from_env() {
                Ok(state) => state.log(&e.to_string()),
                Err(_) => eprintln!("baa: {e}"),
            }
            ExitCode::FAILURE
        }
    }
}

/// Event hook: when an agent's turn ends, queue a check of its project.
/// Herdr waits for hooks, so this only decides and hands off; it never runs a check.
fn on_event() -> Result {
    let state = State::from_env()?;
    let json = env::var("HERDR_PLUGIN_EVENT_JSON")?;
    let change = herdr::parse_status_change(&json)?;
    let previous = state.swap_pane_status(&change.pane_id, &change.agent_status)?;
    if !herdr::turn_finished(previous.as_deref(), &change.agent_status) {
        return Ok(());
    }
    let herdr = Herdr::from_env()?;
    let pane = herdr.pane(&change.pane_id)?;
    queue_check(&state, &herdr, &pane)
}

/// Action: re-check the focused pane's project now.
fn check_focused() -> Result {
    let state = State::from_env()?;
    let herdr = Herdr::from_env()?;
    let pane = herdr.pane(&focused_pane_id()?)?;
    queue_check(&state, &herdr, &pane)
}

/// Action: open the `details` popup. Actions can't open panes themselves.
fn open_details() -> Result {
    Ok(Herdr::from_env()?.open_pane("details")?)
}

/// Popup: page through the last report for the focused pane's project.
fn details() -> Result {
    let state = State::from_env()?;
    let herdr = Herdr::from_env()?;
    let pane = herdr.pane(&focused_pane_id()?)?;
    let root = pane.cwd().and_then(detect::find_root);
    let report = match &root {
        None => "baa: no Rust, TypeScript or .baa.toml project here.".to_string(),
        Some(root) => state
            .project(root)?
            .last_report()?
            .unwrap_or_else(|| format!("baa: no check has run yet for {}", root.display())),
    };
    page(&report)
}

/// Action: remove every `$baa` token and everything baa has saved.
fn clear() -> Result {
    let state = State::from_env()?;
    let herdr = Herdr::from_env()?;
    let panes = herdr.panes()?;
    let tagged = panes
        .iter()
        .filter(|pane| pane.tokens.contains_key(herdr::TOKEN));
    for pane in tagged {
        herdr.clear_token(&pane.pane_id)?;
    }
    state.clear()?;
    Ok(())
}

/// Background worker: run the project's pending checks, publishing each verdict.
fn worker(root: &Path) -> Result {
    let state = State::from_env()?;
    let herdr = Herdr::from_env()?;
    let project = state.project(root)?;
    state::run_pending(&project, || {
        if let Err(e) = check_once(&state, &herdr, &project, root) {
            state.log(&format!("{}: {e}", root.display()));
        }
    })?;
    Ok(())
}

fn queue_check(state: &State, herdr: &Herdr, pane: &Pane) -> Result {
    match pane.cwd().and_then(detect::find_root) {
        Some(root) => {
            state.project(&root)?.request()?;
            spawn_worker(&root)
        }
        // Not a project baa knows (any more): don't leave an old verdict behind.
        None if pane.tokens.contains_key(herdr::TOKEN) => Ok(herdr.clear_token(&pane.pane_id)?),
        None => Ok(()),
    }
}

/// Starts `baa worker <root>` in the background and returns at once. The worker gets
/// no stdio: Herdr waits for a hook's output pipes to close, so inheriting them would
/// hold the hook open for the whole check.
fn spawn_worker(root: &Path) -> Result {
    Command::new(env::current_exe()?)
        .arg("worker")
        .arg(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    Ok(())
}

fn check_once(state: &State, herdr: &Herdr, project: &Project, root: &Path) -> Result {
    let Some(check) = detect::detect(root)? else {
        publish(state, herdr, root, None, None);
        return Ok(());
    };
    let ttl = check.timeout + CHECKING_GRACE;
    publish(state, herdr, root, Some(parse::CHECKING), Some(ttl));

    let started = Instant::now();
    let (verdict, output) = match run::run(&check.command, root, check.timeout)? {
        Outcome::Exited { success, output } => {
            (parse::verdict(&check.counter, success, &output), output)
        }
        Outcome::TimedOut { output } => (Verdict::TimedOut, output),
        Outcome::NotFound => {
            state.log(&format!(
                "{}: {} is not installed",
                root.display(),
                check.command[0]
            ));
            publish(state, herdr, root, None, None);
            return Ok(());
        }
    };
    let token = verdict.token();
    let elapsed = started.elapsed().as_secs_f64();
    let command = check.command.join(" ");
    let report = format!(
        "{token}  {}\n$ {command}  ({elapsed:.1}s)\n\n{}",
        root.display(),
        output.trim()
    );
    project.save_report(&report)?;
    state.log(&format!("{}: {token}", root.display()));
    // Listed again: agents may have started in this project while the check ran.
    publish(state, herdr, root, Some(&token), None);
    Ok(())
}

/// Sets (or, with `None`, clears) `$baa` on every agent pane working in `root`.
/// One failing pane (it may have just closed) doesn't stop the others.
fn publish(state: &State, herdr: &Herdr, root: &Path, value: Option<&str>, ttl: Option<Duration>) {
    let panes = match herdr.panes() {
        Ok(panes) => panes,
        Err(e) => return state.log(&e),
    };
    let agents_here = panes.iter().filter(|pane| {
        pane.agent.is_some() && pane.cwd().and_then(detect::find_root).as_deref() == Some(root)
    });
    for pane in agents_here {
        let result = match value {
            Some(value) => herdr.set_token(&pane.pane_id, value, ttl),
            None => herdr.clear_token(&pane.pane_id),
        };
        if let Err(e) = result {
            state.log(&e);
        }
    }
}

/// Actions get `HERDR_PANE_ID`; popups only get the plugin context.
fn focused_pane_id() -> Result<String> {
    let context = env::var("HERDR_PLUGIN_CONTEXT_JSON").unwrap_or_default();
    herdr::focused_pane_id(&context)
        .or_else(|| env::var("HERDR_PANE_ID").ok())
        .ok_or_else(|| "no focused pane".into())
}

/// Shows `text` in `less`, or prints it and waits for Enter when `less` is missing.
/// The popup closes as soon as this returns.
fn page(text: &str) -> Result {
    if let Ok(mut less) = Command::new("less").stdin(Stdio::piped()).spawn() {
        if let Some(mut stdin) = less.stdin.take() {
            // `less` may quit before reading everything; that's fine.
            let _ = stdin.write_all(text.as_bytes());
        }
        less.wait()?;
        return Ok(());
    }
    println!("{text}\n\n[press Enter to close]");
    io::stdin().read_line(&mut String::new())?;
    Ok(())
}
