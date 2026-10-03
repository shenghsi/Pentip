---
name: pentipctl
description: Use when reading, controlling, splitting, or waiting for terminals in the Pentip Agent Panel. Outside a controllable Pentip terminal, continue without Pentip control commands.
---

# Pentip control

On macOS or Linux, find the release-matched control executable:

```sh
find "$HOME/Library/Application Support/Zed" "$HOME/.local/share/zed" -maxdepth 1 -name 'agent-control-*-executable.json' -exec cat {} \; 2>/dev/null
```

The release-matched control socket is beside the marker and has the same `agent-control-<channel>` stem with a `.sock` suffix. If no matching marker or socket exists, continue the task without Pentip control. Pentip control is not available on Windows.

Use the `executable` value from the marker as `<pentipctl>`. Do not assume that `pentipctl` is on `PATH`.

Run `"<pentipctl>" terminal current --json`. A successful result permits terminal commands. If the connection fails, the protocol is incompatible, or Pentip reports that the caller is not recognized, continue without Pentip control. Do not use `TERM_PROGRAM`, `ZED_TERM`, or another environment variable to decide whether Pentip control is available.

Use `"<pentipctl>" terminal --help` before terminal control. Control applies only to the terminals of the Agent Panel in the caller's workspace.

Use `terminal split` when the user names a direction, asks for a split or pane, or needs to see the old and new terminals at the same time. A split creates a new pane in the Agent Panel with its own shell and a draggable divider, like a tmux pane. Use exactly one of `--current` or `--terminal <terminal-id>`, and always give `--direction left`, `right`, `up`, or `down`. The target terminal must be visible in a pane.

Use `terminal open` for a plain terminal request without a direction, and for an ambiguous request. It creates a new shell as an entry of the Agent Panel that no pane shows. Use `--focus` only when the user asks to switch to the new terminal; the new terminal then replaces the entry of the active pane. Never substitute `terminal open --focus` when the user asks to see both terminals at the same time.

Preserve the caller's working directory unless the user gives another directory. Do not use `--focus` unless the user asks for focus.

To start another coding agent, split a pane and then run the agent command in the new terminal with `terminal run`. The `agent` field of a terminal is the agent CLI that Pentip started in it (`codex`, `claude`, `pi`, or `agy`), or `null` for a terminal that Pentip started as a shell.

Use `terminal current --json` or `terminal list --json` to get a `<terminal-id>` before targeting a terminal with `read`, `send-text`, `send-key`, `run`, `wait-output`, or `split --terminal`. `terminal list` shows every other terminal in the caller's Agent Panel; add `--all` to include the caller's own terminal too.

Use `terminal read <terminal-id>` to see what a terminal has produced. The default `--source recent` returns the last physical lines of output, including scrollback. Use `--source visible` for only what the terminal renders right now -- prefer this over `recent` when a repainting program (a shell prompt, a progress bar, a TUI) would otherwise show stacked fragments instead of the real content. Use `--source detection` to judge whether an agent looks idle or busy; it always returns a snapshot sized to the terminal's current row count and ignores `--lines`. After a first read, pass `--since <cursor>` with the cursor from that read's `--json` output to fetch only the output appended since, instead of rereading the same tail.

Use `terminal wait-output <terminal-id> --match "<text>"` or `--regex "<pattern>"` to block until matching output appears, instead of polling `terminal read` in a loop. The default timeout is 30 seconds; raise `--timeout` for a command expected to take longer.

Use `terminal send-text <terminal-id> "<text>"` to type text without pressing a key, `terminal send-key <terminal-id> <key>...` to send named keys (`enter`, `escape`, `ctrl-c`, `alt-left`, arrow keys, `f1`-`f12`), and `terminal run <terminal-id> "<command>"` to type a full command and press enter in one call. None of these three wait for a response; follow up with `terminal read` or `terminal wait-output` to see the result.
