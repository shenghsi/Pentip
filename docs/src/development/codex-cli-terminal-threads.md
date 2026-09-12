# Codex CLI Terminal Threads in Pentip

Status: Implemented

Date: 2026-09-12

## Goal {#codex-cli-terminal-threads-goal}

Pentip can connect external agents through the Agent Client Protocol (ACP).
Some users want to use the native agent CLI instead. They want the CLI to run
in an Agent Panel terminal and appear in the Threads Sidebar.

The first supported direct launch is Codex CLI.

This design keeps the change close to Zed upstream. It uses the existing
Terminal Thread code in `agent_ui`. It does not port the separate Flint
`agent_threads` crate.

## User Flow {#codex-cli-terminal-threads-user-flow}

1. Open the new-thread menu in the Agent Panel.
2. Select **Codex CLI**.
3. Pentip opens a terminal in the current project folder.
4. Pentip sends `codex` to the terminal shell.
5. The terminal appears as **Codex** in the Agent Panel and Threads Sidebar.

The normal **Terminal** option stays available. It opens a terminal without a
fixed agent command.

## Product Decisions {#codex-cli-terminal-threads-product-decisions}

- **Codex CLI** is a separate item in the new-thread menu.
- The item does not replace the existing **Terminal** item.
- The item does not replace ACP external agents.
- The command is fixed to `codex` for this first implementation.
- Pentip does not add `--dangerously-bypass-approvals-and-sandbox` or other
  Codex CLI options.
- Codex uses its own authentication, models, tools, skills, and configuration.
- Pentip supplies the terminal, project folder, title, and sidebar entry.

## Implementation {#codex-cli-terminal-threads-implementation}

The implementation is in `crates/agent_ui/src/agent_panel.rs` and
`crates/agent_ui/src/agent_ui.rs`.

### Action and Menu {#codex-cli-terminal-threads-action-menu}

The `agent::NewCodexTerminalThread` action starts the feature. Pentip registers
the action on the workspace and on the Agent Panel element. This makes the
action work when the Agent Panel or another workspace surface dispatches it.

The Agent Panel new-thread menu contains a **Codex CLI** item. The item uses the
OpenAI icon and dispatches `NewCodexTerminalThread`. The item is available only
when the project supports terminals. This is the same gate that controls the
normal **Terminal** item.

### Terminal Launch {#codex-cli-terminal-threads-launch}

`AgentPanel::new_codex_terminal` performs these steps:

1. Check that the project supports terminals.
2. Record `AgentPanelEntryKind::Terminal` as the last created entry type.
3. Select the normal terminal working folder for the current project.
4. Call the shared agent CLI terminal launcher with the title `Codex` and a
   `codex` command that publishes its thread ID in the terminal title.

The shared launcher creates a normal Agent Panel shell terminal. It does not
start Codex as an ACP process. It gives the terminal a custom `Codex` title and
selects and focuses the terminal.

### Startup Command Order {#codex-cli-terminal-threads-startup-order}

Pentip starts the configured terminal shell. It sends the configured Terminal
Thread init command first, if one exists. It then sends `codex` with a local
terminal-title override that publishes the Codex thread ID. This keeps the
normal terminal environment and supports local, remote, and WSL projects.

For example, if `agent.terminal_init_command` is `prepare-codex`, Pentip writes
this input to the shell:

```text
prepare-codex<CR>codex -c 'tui.terminal_title=["thread-id"]'<CR>
```

Pentip waits for the terminal startup handshake before it writes this input.
The write does not count as keyboard input from the user. Each command ends
with a carriage return. This behavior also works with PowerShell.

If the shell cannot find `codex`, the shell shows its normal command-not-found
error. Pentip does not install Codex CLI and it does not replace the shell
error with an ACP error.

## Sidebar Discovery {#codex-cli-terminal-threads-sidebar-discovery}

The terminal has a custom **Codex** title. The Agent Panel saves the same
terminal metadata that it saves for a normal Terminal Thread. The Threads
Sidebar already reads this metadata store, so it discovers the Codex terminal
without a separate sidebar path.

While Codex is the terminal's foreground process, the Agent Panel and Threads
Sidebar show the OpenAI icon. When Codex exits and the shell becomes the
foreground process, both surfaces show the terminal icon again. This process
state is live data and is not saved.

The saved `TerminalThreadMetadata` includes:

- the terminal ID
- the current and custom title
- the creation time
- the project and worktree paths
- the remote connection data
- the terminal working folder
- the agent CLI type and Codex session ID prefix

The Agent Panel emits its normal entry-change and terminal-started events.
Existing sidebar search, activation, notification, rename, and close behavior
therefore applies to the Codex terminal.

## Test Design {#codex-cli-terminal-threads-tests}

The focused Agent Panel test calls `AgentPanel::new_codex_terminal`. Test builds
use a display-only terminal, so the test does not require Codex CLI to be
installed. The test verifies these results:

- the terminal title is `Codex`
- the init command runs before `codex`
- the terminal metadata store contains the Codex terminal
- the sidebar display title is `Codex`

The existing sidebar integration test verifies that Agent Panel terminals
appear in the Threads Sidebar and sidebar search.

## Boundaries {#codex-cli-terminal-threads-boundaries}

- Codex owns authentication, models, tools, and its configuration files.
- Pentip owns the terminal surface, title, project association, and sidebar
  entry.
- This change does not replace ACP external agents.
- This change does not import Flint's separate `agent_threads` crate.
- This change does not scan Codex session history.
- This change does not add Codex settings to the Settings Editor.
- This change does not download or update Codex CLI.

## Session Resume {#codex-cli-terminal-threads-resume}

Codex writes its thread ID to the terminal title. Pentip saves the unique ID
prefix with the terminal metadata. When Pentip restores the terminal, it finds
the matching Codex session record, extracts the full session UUID, and runs
`codex resume <session-id>`. It does not use `codex resume --last`.

The session lookup runs in the restored terminal shell. It therefore uses the
same local, remote, WSL, or PowerShell environment as Codex. If the matching
session record is missing, the terminal shows an error and does not start a
different Codex session.

The generic new-thread action remembers only that the last entry was a
terminal. It does not remember that the terminal ran Codex. To start another
Codex terminal, select **Codex CLI** again.

## Future CLI Options {#codex-cli-terminal-threads-future-options}

Future agent CLI options can use `spawn_agent_cli_terminal`. Each option needs
a display title, a shell command, an action, and a new-thread menu item.

Before a new CLI option is added, verify these points:

- the command works in local, remote, WSL, and PowerShell terminals
- the CLI can use a stable custom title
- terminal bell and title events work with the existing notification path
- the CLI has clear installation and authentication instructions
- the terminal metadata is sufficient for sidebar display and search
