# Agentic Layout Modes

Status: Proposed

Date: 2026-09-21

## Summary {#agent-editor-modes-summary}

Pentip provides Agentic Layout and Classic Layout. Agentic Layout is the
default. It has two workspace modes: Agent Mode and Editor Mode. Only one mode
is visible at a time.

Agent Mode contains the Threads Sidebar and the Agent Panel. Editor Mode
contains the Editor and the normal editor panels. One action and one shortcut
switch between the modes. Each mode keeps its state while the other mode is
visible.

Classic Layout keeps the current dock-based workspace. It can show the Editor,
Threads Sidebar, Agent Panel, and other panels at the same time.

This design replaces the current Agentic Layout panel-position preset. It does
not combine the Terminal Panel and Terminal Threads.

## Problem {#agent-editor-modes-problem}

The Threads Sidebar and Agent Panel are closely related. The Threads Sidebar
selects a thread. The Agent Panel displays that thread. Independent controls
for these surfaces create combinations that do not have a clear purpose.

The current dock model can also show the Agent Panel beside the Editor. This
reduces the available width for both surfaces. A user must manage panels before
they can focus on agent work or editor work.

Pentip needs a small set of predictable workspace states. Switching between
agent work and editor work must be one operation. Users who need simultaneous
panels must be able to keep the current behavior through Classic Layout.

## Goals {#agent-editor-modes-goals}

- Provide one complete workspace for agent work.
- Provide one complete workspace for editor work.
- Keep the Agent Panel visible during agent work.
- Switch between the two modes with one stable shortcut.
- Restore the previous state of each mode after a switch.
- Make Agentic Layout the default layout.
- Keep the current dock-based behavior in Classic Layout.
- Keep the Terminal Panel and Terminal Threads as separate terminal types.

## Non-goals {#agent-editor-modes-non-goals}

- Do not unify the Terminal Panel and Terminal Threads.
- Do not change how an agent, shell, or task runs in a terminal.
- Do not change Agent Thread or Terminal Thread persistence.
- Do not remove the Project Panel, Git Panel, or other editor panels.
- Do not add independent layout variants inside Agent Mode.
- Do not add Agent Mode and Editor Mode to Classic Layout.

## Layout Choices {#agent-editor-modes-layout-choices}

### Agentic Layout {#agent-editor-modes-agentic-layout}

Agentic Layout is the default layout. It contains Agent Mode and Editor Mode.
Only one mode is visible at a time.

Selecting **Panel Layout > Agentic** does these operations:

1. Enable the two-mode workspace.
2. Save the current Classic Layout state.
3. Enter Agent Mode.
4. Restore saved Agentic Layout state when it exists.

### Classic Layout {#agent-editor-modes-classic-layout}

Classic Layout keeps the current dock-based workspace. The Editor, Agent
Panel, Threads Sidebar, and other panels can be visible at the same time.
Users can move panels between valid docks and control the Threads Sidebar
separately.

Selecting **Panel Layout > Classic** does these operations:

1. Disable the two-mode workspace.
2. Keep all running Agent Threads and terminals alive.
3. Restore the saved Classic dock layout and panel state.
4. Keep Agentic Layout state for a later return.

Classic Layout is an explicit preference. New installations and users who have
not selected a layout use Agentic Layout.

## Agentic Workspace Modes {#agent-editor-modes-workspace-modes}

### Agent Mode {#agent-editor-modes-agent-mode}

Agent Mode has this layout:

```text
[Threads Sidebar] [Agent Panel]
```

The Threads Sidebar and Agent Panel fill the workspace. The Editor, Project
Panel, Git Panel, Terminal Panel, and other editor panels are not visible.

The Agent Panel can display an Agent Thread or a Terminal Thread. The Agent
Panel stays visible while Agent Mode is active. The existing Threads Sidebar
toggle can hide or show the Threads Sidebar without changing the mode.

Agent Mode hides normal status-bar information and panel buttons. It keeps the
Agent Panel button as the mode switch. The buttons inside the Threads Sidebar
remain visible.

Agent Mode keeps these values:

- the selected project and thread
- the active Agent Thread or Terminal Thread
- the Threads Sidebar width
- the Agent Panel state
- keyboard focus
- scroll positions and draft input

If no thread exists, Agent Mode shows the new-thread view.

### Editor Mode {#agent-editor-modes-editor-mode}

Editor Mode has this general layout:

```text
[Project or Git Panel] [Editor] [Other Panels]
```

The Threads Sidebar and Agent Panel are not visible. The Editor and normal
editor panels use their existing layout rules. The Project Panel and Git Panel
continue to share their configured dock.

Editor Mode keeps these values:

- open files and panes
- the active file and selection
- pane sizes and arrangement
- open panel and dock states
- the active Terminal Panel terminal
- keyboard focus

## Agentic Mode Switch {#agent-editor-modes-switch}

Add the `workspace::ToggleAgentMode` action. The action has one result for each
current mode:

| Current mode | Result                                         |
| ------------ | ---------------------------------------------- |
| Editor Mode  | Enter Agent Mode and restore its saved state.  |
| Agent Mode   | Enter Editor Mode and restore its saved state. |

In Agentic Layout, the result does not depend on the focused control. Repeated
use always moves between the two modes.

Use these default shortcuts:

- Linux: `Ctrl+?`
- Windows: `Ctrl+Shift+/`
- macOS: `Cmd+?`

These shortcuts currently run `agent::ToggleFocus`. In Agentic Layout, the new
action replaces that panel-focus behavior. The status-bar Agent button and the
**Toggle Agent Mode** command use the same action.

In Classic Layout, the same shortcut and status-bar button keep the current
Agent Panel focus and toggle behavior. They do not change workspace modes.

## Navigation Between Modes {#agent-editor-modes-navigation}

In Agentic Layout, Pentip changes the workspace mode when an action needs
content from the other mode.

| Action                                                                    | Result                                               |
| ------------------------------------------------------------------------- | ---------------------------------------------------- |
| Select an Agent Thread or Terminal Thread from a notification or switcher | Enter Agent Mode and show the thread.                |
| Open a thread from Thread History                                         | Enter Agent Mode and show the thread.                |
| Open a file from Agent Mode                                               | Enter Editor Mode and show the file.                 |
| Open or focus the Terminal Panel from Agent Mode                          | Enter Editor Mode and show the Terminal Panel.       |
| Open a Terminal Thread from Editor Mode                                   | Enter Agent Mode and show the Terminal Thread.       |
| Agent activity starts in the background                                   | Keep the current mode.                               |
| An agent requests permission while Editor Mode is active                  | Keep Editor Mode and show the existing notification. |

An automatic change must occur only when the requested content cannot appear
in the current mode. Background activity must not take focus or change the
mode.

After Pentip changes to Editor Mode to show a file, the mode-switch shortcut
returns to the same thread in Agent Mode.

## Terminal Behavior {#agent-editor-modes-terminal-behavior}

This design keeps the two current terminal types.

- The Terminal Panel belongs to Editor Mode. Its existing toggle shortcut
  enters Editor Mode when necessary.
- A Terminal Thread belongs to Agent Mode. Selecting it enters Agent Mode.
- A user can run an agent in the Terminal Panel.
- A user can run general commands in a Terminal Thread.

The terminal type depends on its workspace container, not on the command that
runs in it.

## State and Persistence {#agent-editor-modes-persistence}

Pentip stores the selected layout as a user preference. Agentic Layout is the
default when this preference does not exist.

In Agentic Layout, each window stores its active mode. Each window also stores
the last state of both modes. A mode switch hides one mode and restores the
other mode. It must not destroy and rebuild either mode.

When Pentip restores a window, it restores:

1. the last active mode
2. the saved layout for that mode
3. the saved background state for the other mode

Existing thread, terminal, editor, pane, and dock persistence remains the
source of data for each surface.

Classic Layout state and Agentic Layout state are independent. A layout change
must not overwrite the saved state of the other layout.

## Layout Constraints {#agent-editor-modes-layout-constraints}

In Agentic Layout, Agent Mode has no independent left or right dock choice. The
Threads Sidebar is on the left, and the Agent Panel uses the remaining width.

Editor Mode keeps its normal panel configuration. Changes to the Editor Mode
layout must not change Agent Mode or saved Classic Layout state.

Classic Layout keeps the current valid panel positions and independent panel
visibility controls.

On a narrow window, Pentip can reduce the Threads Sidebar width to its minimum.
The first implementation must not add an overlay or a third saved layout.

## Accessibility {#agent-editor-modes-accessibility}

- In Agentic Layout, the mode-switch action must work from all focus contexts.
- After a switch, focus returns to the last focused control in the target mode.
- If the saved control no longer exists, Agent Mode focuses its active thread,
  and Editor Mode focuses its active pane.
- The command palette name must describe the result as a mode switch, not as a
  panel toggle.
- Screen readers must receive an announcement when the active mode changes.

## Defaults and Migration {#agent-editor-modes-migration}

Agentic Layout is the default for new installations. It is also the default
when Pentip cannot find an explicit layout preference.

An existing user keeps Classic Layout when their current settings match the
current Classic preset or contain custom panel positions. This avoids an
unexpected workspace replacement. An existing user whose settings match the
current Agentic preset migrates to the new Agentic Layout.

For a user who migrates to Agentic Layout, Pentip selects the initial mode from
the visible surfaces:

1. If the Agent Panel has focus, select Agent Mode.
2. Otherwise, select Editor Mode.

Pentip keeps existing Agent Panel, Threads Sidebar, Editor, and dock state for
restoration inside the applicable Agentic mode. Agentic Layout keeps the
independent open state of the Threads Sidebar. The Agent Panel stays open in
Agent Mode. Classic Layout continues to use the dock open state.

Custom keymaps that call `agent::ToggleFocus` continue to work. In Agentic
Layout, the action dispatches `workspace::ToggleAgentMode`. In Classic Layout,
it keeps its current behavior.

## Implementation Direction {#agent-editor-modes-implementation}

The settings system owns the selected layout. The workspace owns the active
Agentic mode and the mode-switch action. The existing Threads Sidebar and
Agent Panel remain separate views inside the Agent Mode container. The
existing Editor and docks remain inside the Editor Mode container. Classic
Layout continues to use the current workspace and dock structure.

The first implementation must:

1. Make Agentic Layout the default layout.
2. Add a window-level mode state for Agentic Layout.
3. Render only the active Agentic mode container.
4. Preserve both containers and their view state during a switch.
5. Route navigation actions to the required mode.
6. Make the shortcut and status-bar action depend on the selected layout.
7. Save and restore Classic Layout independently.
8. Migrate persisted layout, visibility, and shortcut behavior.

The implementation must not add more mode-specific copies of threads,
terminals, projects, or editor items.

## Test Design {#agent-editor-modes-tests}

Tests must verify these behaviors:

- A new installation uses Agentic Layout.
- An explicit Classic Layout preference remains Classic.
- Selecting Agentic Layout enters Agent Mode.
- Selecting Classic Layout restores its saved dock state.
- A layout change does not stop Agent Threads or terminals.
- The shortcut changes from Editor Mode to Agent Mode.
- The same shortcut changes back to Editor Mode.
- Focus does not change the action result.
- In Classic Layout, the shortcut keeps its current Agent Panel behavior.
- Agent Mode always shows the Agent Panel and can toggle the Threads Sidebar.
- Editor Mode does not show either agent surface.
- Each mode restores its selected item, sizes, focus, and input state.
- Opening a file from Agent Mode enters Editor Mode.
- Selecting a thread enters Agent Mode.
- Background agent activity does not change the mode.
- The Terminal Panel enters Editor Mode.
- A Terminal Thread enters Agent Mode.
- Window restoration restores the active mode and both saved states.
- The compatibility action dispatches the correct action for the selected
  layout.

Tests must cover Linux, Windows, and macOS keymaps. They must also cover local,
remote, and multi-project workspaces.

## Open Questions {#agent-editor-modes-open-questions}

- Must each window remember its mode independently when multiple windows show
  the same project?
- Should a file opened from an Agent Thread return to the exact source location
  when the user switches back to Agent Mode?
- Should the title-bar menu show the active Agentic mode below the selected
  layout?
