## [ERR-20260913-002] codex-history-build

**Logged**: 2026-09-13
**Priority**: low
**Status**: resolved
**Area**: build

### Summary

The first Codex history build used an incorrect icon name and passed a vector to a method that takes an Arc slice.

### Suggested Fix

Read the existing icon use and the UI component method signature before adding a new row.

### Resolution

Use `IconName::AiOpenAi` and convert the project path vector to an Arc slice.

## [ERR-20260913-001] terminal-restore-build

**Logged**: 2026-09-13
**Priority**: low
**Status**: resolved
**Area**: tests

### Summary

The first test build failed after a function parameter change. One call had a missing argument. An unrelated call had an extra argument.

### Suggested Fix

Check all calls with `rg`, including calls with a receiver other than `self`.

### Resolution

The arguments were corrected. The focused restore test passed.

## [ERR-20260912-001] focused-test-assertion

**Logged**: 2026-09-12T22:50:00+08:00
**Priority**: low
**Status**: resolved
**Area**: tests

### Summary

The Codex Windows resume-command test expected an obsolete inline variable.

### Error

```text
assertion failed: windows_command.contains("resume $Matches[1]")
```

### Context

- The implementation now assigns the resolved UUID to `$sessionId`.
- The test still expected direct use of `$Matches[1]`.

### Suggested Fix

Assert that the command resumes `$sessionId`.

### Metadata

- Reproducible: yes
- Related Files: crates/agent_ui/src/agent_panel.rs
- Recurrence-Count: 2

### Resolution

- **Resolved**: 2026-09-12T22:51:00+08:00
- **Notes**: Updated the assertion to match the generated command.

---

## [ERR-20260913-001] apply-patch-context

**Logged**: 2026-09-13T06:15:00+08:00
**Priority**: low
**Status**: resolved
**Area**: frontend

### Summary

A combined sidebar status patch used text from before `cargo fmt` changed its layout.

### Error

```text
apply_patch verification failed: Failed to find expected lines
```

### Context

- The interrupted verification command formatted the source before the next edit.
- The patch used the earlier line layout.

### Suggested Fix

Read the current section and apply smaller patches.

### Metadata

- Reproducible: yes
- Related Files: crates/sidebar/src/sidebar.rs

### Resolution

- **Resolved**: 2026-09-13T06:16:00+08:00
- **Notes**: Refreshed the source context and split the edit.

---

## [ERR-20260913-002] gpui-test-context-mutability

**Logged**: 2026-09-13T06:20:00+08:00
**Priority**: low
**Status**: resolved
**Area**: tests

### Summary

A sidebar test tried to update an entity through the immutable context from `read_with`.

### Error

```text
expected mutable reference, found reference &gpui::App
```

### Context

- The test updated `TerminalThreadMetadataStore` inside `sidebar.read_with`.

### Suggested Fix

Use `VisualTestContext::update` when an entity update needs `&mut App`.

### Metadata

- Reproducible: yes
- Related Files: crates/sidebar/src/sidebar_tests.rs

### Resolution

- **Resolved**: 2026-09-13T06:21:00+08:00
- **Notes**: Moved the store update to `cx.update`.

---

## [ERR-20260913-001] Terminal startup test input

- Status: resolved
- Priority: low
- Area: tests

The real shell startup test expected only the init command. The Codex resume
change adds a shell function before that command. Update the input check when
the startup command list changes. Keep the check that the shell runs the init
command and that startup does not count as user keyboard input.

## [ERR-20260913-002] Codex terminal resume regressions

**Logged**: 2026-09-13T00:00:00+08:00
**Priority**: high
**Status**: resolved
**Area**: frontend

### Summary
A session-only Codex terminal title hid generated thread names, and terminal removal delayed process termination.

### Suggested Fix
Keep `thread-name` with `thread-id` in the Codex terminal title. Terminate the terminal processes before removing the terminal entity.

### Metadata
- Reproducible: yes
- Related Files: crates/agent_ui/src/agent_panel.rs, crates/terminal/src/terminal.rs

---

## [ERR-20260913-003] terminal-status-test-setup

**Status**: resolved
**Area**: tests

The status test set the active program before terminal startup events finished.
A pending event cleared the active program. Run until parked before setting
the test program and status.

The first upstream title source path returned HTTP 404. The bottom pane module
exports the type from `title_setup.rs`. Read the module exports to find it.

## [ERR-20260913-004] spinner-label-import

**Status**: resolved
**Area**: build

`SpinnerLabel` is exported by `ui`, but is absent from the UI prelude.
Import `crate::SpinnerLabel` when using it inside the UI crate.
