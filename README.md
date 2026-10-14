# baa

**uh-oh or all good.**

A tiny [Herdr](https://herdr.dev) plugin. When a coding agent finishes its turn, baa
checks that the agent's project still compiles and shows the verdict next to the agent
in Herdr's sidebar:

| Token | Meaning |
|---|---|
| `…` | checking |
| `✓` | all good |
| `✗ 3` | uh-oh: 3 errors |
| `✗` | uh-oh, error count unknown |
| `⏱` | the check timed out |
| *(nothing)* | not a project baa knows; it stays quiet |

<!-- TODO: replace with a recording -->
![baa in the Herdr sidebar](https://raw.githubusercontent.com/rsdlt/baa/main/assets/demo.gif)

## Requirements

- **Herdr 0.8.0 or newer.** 0.7.4 added the metadata tokens and popups baa uses, and
  0.8.0 resolves plugin commands relative to the plugin. Colouring `$baa` (below) needs
  Herdr 0.9.0.
- **Linux or macOS.**
- **Rust 1.89 or newer**, to build baa.

## Install

```sh
herdr plugin install rsdlt/baa
```

This builds baa with `cargo build --release`.

The `baa` binary is also on crates.io as `cargo install herdr-baa`. The plugin itself
always runs the copy it builds, so you only need this to run `baa` by hand.

To hack on baa, link a checkout instead. `link` doesn't run the build step, so build first:

```sh
cargo build --release
herdr plugin link .
```

## Show `$baa` in the sidebar

baa reports a pane metadata token called `baa`. Herdr shows it once you add `$baa` to the
Agent sidebar rows in `~/.config/herdr/config.toml`. For example, to put it after the
agent name in the default layout:

```toml
[ui.sidebar.agents]
rows = [
  ["state_icon", "machine", "workspace", "tab"],
  ["agent", "$baa"],
]
```

Then run `herdr server reload-config`, or choose *reload config* from the global menu.

On Herdr 0.9.0 and newer you can also colour it:

```toml
[ui.sidebar.agents]
rows = [
  ["state_icon", "machine", "workspace", "tab"],
  ["agent", { token = "$baa", rules = [
    { starts_with = "✓", fg = "#98c379" },
    { starts_with = "✗", fg = "#e06c75", bold = true },
    { starts_with = "⏱", fg = "#e5c07b" },
  ] }],
]
```

## What gets checked

baa walks up from the agent's working directory to the nearest project root and uses the
first match:

| Marker | Check | Error count |
|---|---|---|
| `.baa.toml` | your `command` | exit code only, unless you set `error_marker` |
| `Cargo.toml` | `cargo check --message-format=short` | lines with `: error[` / `: error:` |
| `tsconfig.json` | the project's own `node_modules/.bin/tsc --noEmit` | lines with `: error TS` |

baa never installs anything and never runs `package.json` scripts on its own. If a
project has no local `tsc`, baa stays quiet. Checks time out after 60 seconds. Note that
`cargo check` writes to the project's `target/` like any other cargo build.

Every agent working in the same project shows the same verdict. If several agents finish
while a check is running, baa runs one more check when it ends, not one per agent.

### `.baa.toml`

Drop a `.baa.toml` at a project root to use your own check or tune the built-in one. Every
field is optional:

```toml
command = ["just", "check"]   # replaces the built-in check
timeout_secs = 120            # default 60
error_marker = ": error"      # count output lines containing this text
```

Without `command`, the built-in check for that directory runs with your other settings.

> **Note:** override commands run as your user, with your permissions, every time an agent
> finishes a turn. Only put commands in `.baa.toml` that you would run yourself.

## Actions

Run an action with `herdr plugin action invoke <id>`:

| Action id | What it does |
|---|---|
| `rsdlt.baa.check` | Re-check the focused pane's project now. |
| `rsdlt.baa.details` | Open a popup with the last check's full output (`q` closes it). |
| `rsdlt.baa.clear` | Remove every `$baa` token and everything baa has saved. |

"Focused" means the pane Herdr has focus on, so run `check` and `details` from the agent
pane you mean. To bind them to keys, add this to `~/.config/herdr/config.toml`:

```toml
[[keys.command]]
key = "prefix+b"
type = "plugin_action"
command = "rsdlt.baa.check"
description = "baa: check this project"

[[keys.command]]
key = "prefix+shift+b"
type = "plugin_action"
command = "rsdlt.baa.details"
description = "baa: last check output"
```

## How it works

baa subscribes to `pane.agent_status_changed`. A turn counts as finished on `done`, or
on `working` → `idle`. The check runs inside the hook. Herdr runs hooks off its UI
thread, so the UI never waits, and baa leaves no background processes behind. A
per-project file lock means only one check runs per project; a hook that finds the lock
taken just leaves a request for the running check to pick up, and exits. The check runs
with a timeout, and the token goes to every agent pane in that project.

State (last status per pane, last output per project, and a log) lives in
`HERDR_PLUGIN_STATE_DIR`. Anything that goes wrong (no toolchain, a missing binary, a
malformed event) is written to `baa.log` there and to `herdr plugin log list --plugin
rsdlt.baa`. baa never crashes or shows a noisy error.

## License

Apache-2.0, same as Herdr.
