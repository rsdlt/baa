//! The Herdr side: event payloads, plugin context and the few CLI calls baa makes.
//!
//! Every call goes through `HERDR_BIN_PATH`, the binary of the running Herdr.

use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;

/// The metadata token baa owns; users show it with `$baa` in a sidebar row.
pub const TOKEN: &str = "baa";

const STATUS_EVENT: &str = "pane.agent_status_changed";

/// The part of a `pane.agent_status_changed` event baa needs.
#[derive(Debug, PartialEq, Eq, Deserialize)]
pub struct StatusChange {
    pub pane_id: String,
    pub agent_status: String,
}

/// Parses `HERDR_PLUGIN_EVENT_JSON`, returning `None` for any other event.
pub fn parse_status_change(json: &str) -> serde_json::Result<Option<StatusChange>> {
    #[derive(Deserialize)]
    struct Envelope {
        event: String,
        data: serde_json::Value,
    }
    let envelope: Envelope = serde_json::from_str(json)?;
    if envelope.event != STATUS_EVENT {
        return Ok(None);
    }
    serde_json::from_value(envelope.data).map(Some)
}

/// Whether a status change ends an agent's turn.
///
/// Herdr usually reports `done`, but some turns end as a plain `working` → `idle`
/// (the same rule herdr-ntfysh uses). The event carries no previous status,
/// so the caller remembers it.
pub fn turn_finished(previous: Option<&str>, current: &str) -> bool {
    match current {
        "done" => previous != Some("done"),
        "idle" => previous == Some("working"),
        _ => false,
    }
}

/// The focused pane from `HERDR_PLUGIN_CONTEXT_JSON`, which actions and popups receive.
pub fn focused_pane_id(context_json: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Context {
        focused_pane_id: Option<String>,
    }
    serde_json::from_str::<Context>(context_json)
        .ok()?
        .focused_pane_id
}

#[derive(Debug, Deserialize)]
pub struct Pane {
    pub pane_id: String,
    pub agent: Option<String>,
    cwd: Option<String>,
    foreground_cwd: Option<String>,
    #[serde(default)]
    pub tokens: HashMap<String, String>,
}

impl Pane {
    /// Where the pane is working: its foreground process's directory, else the pane's own.
    pub fn cwd(&self) -> Option<&Path> {
        self.foreground_cwd
            .as_deref()
            .or(self.cwd.as_deref())
            .map(Path::new)
    }
}

/// Herdr's CLI answers with `{"id": …, "result": …}`.
#[derive(Deserialize)]
struct Response<T> {
    result: T,
}

pub struct Herdr {
    bin: PathBuf,
    /// Also the `--source` of baa's metadata reports, which Herdr keeps apart from others'.
    plugin_id: String,
}

impl Herdr {
    pub fn new(bin: impl Into<PathBuf>, plugin_id: impl Into<String>) -> Self {
        Self {
            bin: bin.into(),
            plugin_id: plugin_id.into(),
        }
    }

    pub fn from_env() -> Result<Self, String> {
        let bin = env::var_os("HERDR_BIN_PATH").ok_or("HERDR_BIN_PATH is not set")?;
        let plugin_id = env::var("HERDR_PLUGIN_ID").unwrap_or_else(|_| "rsdlt.baa".into());
        Ok(Self::new(bin, plugin_id))
    }

    pub fn pane(&self, pane_id: &str) -> Result<Pane, String> {
        #[derive(Deserialize)]
        struct Reply {
            pane: Pane,
        }
        Ok(self.query::<Reply>(&["pane", "get", pane_id])?.pane)
    }

    pub fn panes(&self) -> Result<Vec<Pane>, String> {
        #[derive(Deserialize)]
        struct Reply {
            panes: Vec<Pane>,
        }
        Ok(self.query::<Reply>(&["pane", "list"])?.panes)
    }

    /// Sets `$baa` on a pane. With a `ttl`, Herdr drops the token when it runs out.
    pub fn set_token(
        &self,
        pane_id: &str,
        value: &str,
        ttl: Option<Duration>,
    ) -> Result<(), String> {
        let token = format!("{TOKEN}={value}");
        let ttl_ms = ttl.map(|ttl| ttl.as_millis().to_string());
        let mut args = vec!["--token", token.as_str()];
        if let Some(ttl_ms) = &ttl_ms {
            args.extend(["--ttl-ms", ttl_ms.as_str()]);
        }
        self.report_metadata(pane_id, &args)
    }

    pub fn clear_token(&self, pane_id: &str) -> Result<(), String> {
        self.report_metadata(pane_id, &["--clear-token", TOKEN])
    }

    /// Opens one of this plugin's `[[panes]]` entrypoints.
    pub fn open_pane(&self, entrypoint: &str) -> Result<(), String> {
        let args = [
            "plugin",
            "pane",
            "open",
            "--plugin",
            &self.plugin_id,
            "--entrypoint",
            entrypoint,
        ];
        self.call(&args).map(drop)
    }

    fn report_metadata(&self, pane_id: &str, args: &[&str]) -> Result<(), String> {
        let mut full = vec![
            "pane",
            "report-metadata",
            pane_id,
            "--source",
            &self.plugin_id,
        ];
        full.extend(args);
        self.call(&full).map(drop)
    }

    fn query<T: DeserializeOwned>(&self, args: &[&str]) -> Result<T, String> {
        let stdout = self.call(args)?;
        serde_json::from_str::<Response<T>>(&stdout)
            .map(|response| response.result)
            .map_err(|e| format!("herdr {}: unexpected output: {e}", args.join(" ")))
    }

    fn call(&self, args: &[&str]) -> Result<String, String> {
        let output = Command::new(&self.bin)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("herdr {}: {e}", args.join(" ")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("herdr {}: {}", args.join(" "), stderr.trim()));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn parses_status_changes_and_ignores_other_events() {
        let json = r#"{"event":"pane.agent_status_changed","data":{"type":"pane_agent_status_changed",
            "pane_id":"w8:p1","workspace_id":"w8","agent_status":"done","agent":"claude",
            "display_agent":null,"title":null,"state_labels":{},"something_new":1}}"#;
        assert_eq!(
            parse_status_change(json).unwrap(),
            Some(StatusChange {
                pane_id: "w8:p1".into(),
                agent_status: "done".into()
            })
        );

        let other = r#"{"event":"pane.focused","data":{"type":"pane_focused","pane_id":"w8:p1"}}"#;
        assert_eq!(parse_status_change(other).unwrap(), None);

        assert!(parse_status_change("not json").is_err());
        let missing = r#"{"event":"pane.agent_status_changed","data":{"pane_id":"w8:p1"}}"#;
        assert!(parse_status_change(missing).is_err());
    }

    #[test]
    fn finished_turns() {
        let cases = [
            (Some("working"), "done", true),
            (None, "done", true),
            (Some("idle"), "done", true),
            (Some("done"), "done", false),
            (Some("working"), "idle", true),
            (Some("done"), "idle", false),
            (Some("blocked"), "idle", false),
            (None, "idle", false),
            (Some("idle"), "working", false),
            (Some("working"), "blocked", false),
            (Some("working"), "unknown", false),
        ];
        for (previous, current, expected) in cases {
            assert_eq!(
                turn_finished(previous, current),
                expected,
                "{previous:?} -> {current}"
            );
        }
    }

    #[test]
    fn focused_pane_from_context() {
        let context = r#"{"workspace_id":"w8","focused_pane_id":"w8:p1","focused_pane_cwd":"/x"}"#;
        assert_eq!(focused_pane_id(context).as_deref(), Some("w8:p1"));
        assert_eq!(focused_pane_id(r#"{"workspace_id":"w8"}"#), None);
        assert_eq!(focused_pane_id("garbage"), None);
    }

    #[test]
    fn pane_cwd_prefers_the_foreground_process() {
        let pane: Pane = serde_json::from_str(
            r#"{"pane_id":"w8:p1","agent":"claude","cwd":"/shell","foreground_cwd":"/agent","tokens":{"baa":"✓"}}"#,
        )
        .unwrap();
        assert_eq!(pane.cwd(), Some(Path::new("/agent")));
        assert_eq!(pane.tokens.get(TOKEN).map(String::as_str), Some("✓"));

        let pane: Pane = serde_json::from_str(r#"{"pane_id":"w8:p2","cwd":"/shell"}"#).unwrap();
        assert_eq!(pane.cwd(), Some(Path::new("/shell")));
        assert_eq!(pane.agent, None);
    }

    /// A stand-in `herdr` that records its arguments and prints a canned reply.
    fn fake_herdr(reply: &str) -> (tempfile::TempDir, Herdr) {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("herdr");
        let log = dir.path().join("args");
        let script = format!(
            "#!/bin/sh\necho \"$*\" >> '{}'\ncat <<'EOF'\n{reply}\nEOF\n",
            log.display()
        );
        fs::write(&bin, script).unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        (dir, Herdr::new(bin, "rsdlt.baa"))
    }

    fn calls(dir: &tempfile::TempDir) -> String {
        fs::read_to_string(dir.path().join("args")).unwrap()
    }

    #[test]
    fn reads_panes() {
        let reply = r#"{"id":"cli:pane:list","result":{"panes":[{"pane_id":"w1:p1","agent":"claude","foreground_cwd":"/p"},{"pane_id":"w1:p2"}]}}"#;
        let (dir, herdr) = fake_herdr(reply);
        let panes = herdr.panes().unwrap();
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].cwd(), Some(Path::new("/p")));
        assert_eq!(calls(&dir), "pane list\n");
    }

    #[test]
    fn reports_tokens_and_opens_panes() {
        let (dir, herdr) = fake_herdr("{}");
        herdr.set_token("w1:p1", "✗ 3", None).unwrap();
        herdr
            .set_token("w1:p1", "…", Some(Duration::from_secs(70)))
            .unwrap();
        herdr.clear_token("w1:p1").unwrap();
        herdr.open_pane("details").unwrap();
        assert_eq!(
            calls(&dir),
            "pane report-metadata w1:p1 --source rsdlt.baa --token baa=✗ 3\n\
             pane report-metadata w1:p1 --source rsdlt.baa --token baa=… --ttl-ms 70000\n\
             pane report-metadata w1:p1 --source rsdlt.baa --clear-token baa\n\
             plugin pane open --plugin rsdlt.baa --entrypoint details\n"
        );
    }

    #[test]
    fn failures_are_errors_not_panics() {
        let (_dir, herdr) = fake_herdr("not json");
        assert!(herdr.pane("w1:p1").is_err());
        let missing = Herdr::new("/nonexistent/herdr", "rsdlt.baa");
        assert!(missing.panes().is_err());
    }
}
