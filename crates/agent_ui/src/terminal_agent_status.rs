#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DetectedTerminalAgentStatus {
    Working,
    Blocked,
    Idle,
    Unknown,
}

pub(crate) fn classify(
    agent_cli: &str,
    screen_tail: &str,
    terminal_title: &str,
) -> DetectedTerminalAgentStatus {
    match agent_cli {
        "claude" => classify_claude(screen_tail, terminal_title),
        "codex" => classify_codex(screen_tail, terminal_title),
        "pi" => classify_pi(screen_tail),
        "agy" => classify_agy(screen_tail),
        _ => classify_generic(screen_tail),
    }
}

/// Ported from herdr's Pi detection manifest (`src/detect/manifests/pi.toml`
/// in the `herdr` repo). That manifest has a single rule, unlike herdr's
/// fuller Claude/Codex manifests: the literal "Working..." anywhere on
/// screen means Pi is working. There's no upstream rule for Blocked or
/// Idle, so anything else falls back to `classify_generic`'s bare
/// known-prompt check.
fn classify_pi(screen_tail: &str) -> DetectedTerminalAgentStatus {
    if screen_tail.to_ascii_lowercase().contains("working...") {
        return DetectedTerminalAgentStatus::Working;
    }
    classify_generic(screen_tail)
}

/// Ported from herdr's Antigravity (agy) detection manifest
/// (`src/detect/manifests/antigravity.toml` in the `herdr` repo), evaluated
/// highest-priority first with the first match winning, same as this file's
/// other classifiers. Unlike Claude/Codex/Pi, this is status detection only:
/// Antigravity has no way to assign or discover a session id from the
/// outside (`agy --conversation <id>` silently starts a fresh, differently
/// -id'd conversation when `<id>` doesn't already exist -- verified against
/// the real CLI, and the reason herdr's own upstream project `flint` leaves
/// Antigravity unregistered too), so there's no session tracking to layer on
/// top here.
fn classify_agy(screen_tail: &str) -> DetectedTerminalAgentStatus {
    let screen_tail_lower = screen_tail.to_ascii_lowercase();

    // permission_prompt (priority 300).
    if screen_tail_lower.contains("requesting permission for:")
        && (screen_tail_lower.contains("do you want to proceed?")
            || (screen_tail_lower.contains("tab amend")
                && screen_tail_lower.contains("edit command")))
    {
        return DetectedTerminalAgentStatus::Blocked;
    }

    // spinner_working (priority 100).
    if screen_tail.lines().any(is_agy_spinner_working_line) {
        return DetectedTerminalAgentStatus::Working;
    }

    // background_tasks_working (priority 90).
    if bottom_non_empty_lines(screen_tail, 5)
        .lines()
        .any(is_agy_background_tasks_line)
    {
        return DetectedTerminalAgentStatus::Working;
    }

    classify_generic(screen_tail)
}

fn is_agy_spinner_character(character: char) -> bool {
    matches!(character, '\u{2800}'..='\u{28FF}')
}

/// Antigravity's live "<braille spinner> <verb>ing…" activity line.
/// Simplified from herdr's exact regex
/// (`^\s*[\u{2800}-\u{28FF}]+\s+\p{Alphabetic}+\w*ing\b`) to "one or more
/// braille spinner glyphs, then a word ending in \"ing\"", which every
/// observed real form of this line satisfies.
fn is_agy_spinner_working_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    let spinner_end = trimmed
        .char_indices()
        .find(|(_, character)| !is_agy_spinner_character(*character))
        .map_or(trimmed.len(), |(index, _)| index);
    if spinner_end == 0 {
        return false;
    }
    let Some(word) = trimmed[spinner_end..]
        .strip_prefix(|character: char| character.is_whitespace())
        .map(|rest| rest.trim_start())
        .and_then(|rest| rest.split_whitespace().next())
    else {
        return false;
    };
    word.chars().next().is_some_and(char::is_alphabetic)
        && word
            .chars()
            .all(|character| character.is_alphanumeric() || character == '_')
        && word.ends_with("ing")
}

/// Antigravity's "· N task(s)" background-task status line.
fn is_agy_background_tasks_line(line: &str) -> bool {
    line.split('\u{b7}').skip(1).any(|segment| {
        let segment = segment.trim_start();
        let digits_end = segment
            .find(|character: char| !character.is_ascii_digit())
            .unwrap_or(0);
        digits_end > 0
            && !segment.starts_with('0')
            && segment[digits_end..]
                .trim_start()
                .to_ascii_lowercase()
                .starts_with("task")
    })
}

/// Ported from herdr's Codex detection manifest
/// (`src/detect/manifests/codex.toml` in the `herdr` repo, via Flint's own
/// port of it at `agent_threads/src/attention_manifests/codex.toml`, which
/// matches it closely). `trust_directory` (herdr's one-time "do you trust
/// this directory?" prompt) and `transcript_viewer` (`state = "unknown"`
/// with `skip_state_update = true`) are dropped for the same reasons Flint's
/// port drops them: the former needs true top-of-scrollback access this
/// classifier doesn't have, and the latter only matters to a continuous
/// polling loop neither Flint nor Pentip has.
///
/// Pentip additionally launches Codex with
/// `-c 'tui.terminal_title=["activity","thread-name","thread-id","status"]'`,
/// giving it a structured title with an explicit status word that neither
/// herdr nor Flint can assume -- that's checked first, ahead of every
/// upstream rule, since it's a strictly more reliable signal when present.
fn classify_codex(screen_tail: &str, terminal_title: &str) -> DetectedTerminalAgentStatus {
    let terminal_title_lowercase = terminal_title.to_ascii_lowercase();
    if terminal_title_lowercase.starts_with("[ ! ] action required")
        || terminal_title_lowercase.starts_with("[ . ] action required")
    {
        return DetectedTerminalAgentStatus::Blocked;
    }
    match terminal_title_lowercase.rsplit(" | ").next().map(str::trim) {
        Some("starting" | "working" | "thinking" | "waiting") => {
            return DetectedTerminalAgentStatus::Working;
        }
        Some("ready") => return DetectedTerminalAgentStatus::Idle,
        _ => {}
    }

    // osc_title_blocked (1100).
    if terminal_title_lowercase.contains("action required") {
        return DetectedTerminalAgentStatus::Blocked;
    }

    // live_strong_blocker (900): scoped to the live turn -- text after the
    // last `›` prompt marker -- so an already-resolved prompt from earlier
    // in the conversation can't keep reporting Blocked once Codex has moved
    // on to a fresh turn.
    let after_last_prompt = after_last_codex_prompt_marker(screen_tail).to_ascii_lowercase();
    if [
        "press enter to confirm or esc to cancel",
        "enter to submit answer",
        "enter to submit all",
        "allow command?",
    ]
    .iter()
    .any(|pattern| after_last_prompt.contains(pattern))
    {
        return DetectedTerminalAgentStatus::Blocked;
    }

    // weak_blocker (600): unscoped, matching herdr/Flint's own accepted risk
    // for this looser fallback.
    let screen_tail_lower = screen_tail.to_ascii_lowercase();
    if screen_tail_lower.contains("[y/n]")
        || screen_tail_lower.contains("yes (y)")
        || ((screen_tail_lower.contains("do you want to")
            || screen_tail_lower.contains("would you like to"))
            && (screen_tail_lower.contains("yes") || screen_tail_lower.contains('❯')))
    {
        return DetectedTerminalAgentStatus::Blocked;
    }

    // osc_title_working (1050).
    if terminal_title
        .split_whitespace()
        .any(|word| word.chars().any(is_spinner_character))
    {
        return DetectedTerminalAgentStatus::Working;
    }

    // screen_working_fallback (500): the "<bullet> Working (... esc to
    // interrupt)" status line, unless the turn was actually interrupted --
    // its own text can otherwise linger in the same recent window.
    let bottom_three = bottom_non_empty_lines(screen_tail, 3);
    if bottom_three.lines().any(is_codex_working_fallback_line)
        && !bottom_three.contains("■ Conversation interrupted")
    {
        return DetectedTerminalAgentStatus::Working;
    }

    // osc_title_idle (100).
    if !terminal_title.trim().is_empty() {
        DetectedTerminalAgentStatus::Idle
    } else {
        DetectedTerminalAgentStatus::Unknown
    }
}

fn after_last_codex_prompt_marker(content: &str) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    let Some(index) = lines.iter().rposition(|line| is_codex_prompt_line(line)) else {
        return content;
    };
    slice_from_line_index(content, &lines, index + 1)
}

fn is_codex_prompt_line(line: &str) -> bool {
    line == "›" || line.starts_with("› ")
}

fn is_codex_working_fallback_line(line: &str) -> bool {
    let Some(rest) = line.strip_prefix('•').or_else(|| line.strip_prefix('◦')) else {
        return false;
    };
    let Some(rest) = rest.trim_start().strip_prefix("Working (") else {
        return false;
    };
    let Some(close_index) = rest.find(')') else {
        return false;
    };
    if !rest[..close_index].contains("esc to interrupt") {
        return false;
    }
    let after = rest[close_index + 1..].trim_start();
    after.is_empty() || after.starts_with('·')
}

/// Ported from herdr's Claude Code detection manifest
/// (`src/detect/manifests/claude.toml` in the `herdr` repo, via Flint's own
/// port of it at `agent_threads/src/attention_manifests/claude.toml`, which
/// is missing a few of herdr's rules -- notably `live_turn_working`, the
/// main screen-body "is it still working" signal). Both interpret their
/// rules from TOML through a generic priority/gate engine; Pentip has no
/// such engine, so the same rules are inlined directly here instead,
/// evaluated highest-priority first with the first match winning (safe here
/// since in practice at most one tier ever matches a given screen). Rules
/// that only exist to suppress herdr's own continuous-polling loop
/// (`transcript_viewer`, `model_picker_menu`; both `state = "unknown"` with
/// `skip_state_update = true`) are dropped, matching Flint's own precedent.
fn classify_claude(screen_tail: &str, terminal_title: &str) -> DetectedTerminalAgentStatus {
    // osc_title_working (priority 1100): the window title starts with a busy
    // spinner glyph followed by a space. Braille covers Claude Code <=
    // 2.1.227; half-circles are the 2.1.228+ busy spinner.
    if starts_with_working_spinner(terminal_title) {
        return DetectedTerminalAgentStatus::Working;
    }

    // live_blocked_form (980): a live "esc to cancel" confirmation/selection
    // prompt just below the input box's border.
    let after_last_rule = after_last_horizontal_rule(screen_tail).to_ascii_lowercase();
    if after_last_rule.contains("esc to cancel")
        && (after_last_rule.contains("enter to confirm")
            || (after_last_rule.contains("enter to select")
                && [
                    "tab/arrow keys to navigate",
                    "arrow keys to navigate",
                    "arrows to navigate",
                    "↑/↓ to navigate",
                    "↑↓ to navigate",
                ]
                .iter()
                .any(|pattern| after_last_rule.contains(pattern))))
    {
        return DetectedTerminalAgentStatus::Blocked;
    }

    let screen_tail_lower = screen_tail.to_ascii_lowercase();

    // dynamic_workflow_prompt (980).
    if screen_tail_lower.contains("run a dynamic workflow?")
        && screen_tail_lower.contains("esc to cancel")
    {
        return DetectedTerminalAgentStatus::Blocked;
    }

    // btw_overlay_working (975): the "/btw" quick-answer overlay is open.
    let bottom_five_lines: Vec<&str> = bottom_non_empty_lines(screen_tail, 5).lines().collect();
    if bottom_five_lines
        .iter()
        .any(|line| is_btw_command_line(line))
        && bottom_five_lines
            .iter()
            .any(|line| line_ends_with_esc_to_close(line))
    {
        return DetectedTerminalAgentStatus::Working;
    }

    // live_turn_working (970): the "<glyph> <verb>… (Ns · ...)" activity line
    // shown while thinking, streaming a reply, or running a tool, or its
    // "<pause/play> ... esc to interrupt" alternative form.
    if bottom_non_empty_lines(screen_tail, 12)
        .lines()
        .any(|line| is_claude_activity_line(line) || is_claude_interrupt_hint_line(line))
    {
        return DetectedTerminalAgentStatus::Working;
    }

    // background_shell_working (965): a background shell is still running.
    if bottom_non_empty_lines(screen_tail, 5)
        .lines()
        .any(is_claude_background_shell_line)
    {
        return DetectedTerminalAgentStatus::Working;
    }

    // background_agents_working (965): still waiting on background subagents,
    // shown as the last line of live status just above the input box.
    if is_claude_waiting_for_background_agents_line(last_non_empty_line(above_prompt_box(
        screen_tail,
    ))) {
        return DetectedTerminalAgentStatus::Working;
    }

    // live_prompt_box (950): a bare `❯` prompt inside the live input box (the
    // text strictly between the last two horizontal rules), as long as it
    // isn't actually a selection menu reusing the same box.
    if let Some(body) = prompt_box_body(screen_tail) {
        let body_lower = body.to_ascii_lowercase();
        let has_prompt_line = body.lines().any(|line| line.trim_start().starts_with('❯'));
        let looks_like_menu = [
            "enter to select",
            "esc to cancel",
            "tab/arrow keys",
            "arrow keys to navigate",
            "↑/↓ to navigate",
        ]
        .iter()
        .any(|pattern| body_lower.contains(pattern));
        if has_prompt_line && !looks_like_menu {
            return DetectedTerminalAgentStatus::Idle;
        }
    }

    // bash_permission_prompt (850): a "Do you want to proceed?" bash-tool menu.
    if screen_tail_lower.contains("do you want to proceed?")
        && [
            "bash command",
            "bash(",
            "contains expansion",
            "tab to amend",
            "ctrl+e to explain",
        ]
        .iter()
        .any(|pattern| screen_tail_lower.contains(pattern))
        && screen_tail.lines().any(|line| {
            line_selects_bare_word(line, "yes")
                || line_selects_choice(line, "1", "yes")
                || line_selects_choice(line, "2", "no")
        })
    {
        return DetectedTerminalAgentStatus::Blocked;
    }

    // generic_permission_prompt (840): any other "Do you want to proceed?"
    // menu, identified by the confirm hint and a numbered yes/no choice.
    if after_last_rule.contains("do you want to proceed?")
        && after_last_rule.contains("esc to cancel")
        && screen_tail.lines().any(|line| {
            line_selects_choice(line, "1", "yes")
                || line_selects_choice(line, "2", "yes")
                || line_selects_choice(line, "2", "no")
                || line_selects_choice(line, "3", "no")
        })
    {
        return DetectedTerminalAgentStatus::Blocked;
    }

    // legacy_no_prompt_blocker (300): older/looser confirmation phrasing,
    // vetoed if the screen is currently showing a bare idle prompt anywhere
    // (this is what keeps an already-resolved prompt from scrollback out of
    // this fallback once Claude has actually moved on).
    let has_bare_prompt_line = screen_tail.lines().any(|line| line.trim() == "❯");
    if !has_bare_prompt_line
        && ((screen_tail_lower.contains("do you want to")
            && (screen_tail_lower.contains("yes") || screen_tail_lower.contains('❯')))
            || (screen_tail_lower.contains("would you like to")
                && (screen_tail_lower.contains("yes") || screen_tail_lower.contains('❯')))
            || screen_tail_lower.contains("waiting for permission")
            || screen_tail_lower.contains("do you want to allow this connection?")
            || screen_tail_lower.contains("tab to amend")
            || screen_tail_lower.contains("ctrl+e to explain")
            || (screen_tail_lower.contains("do you want to proceed?")
                && screen_tail_lower.contains("esc to cancel"))
            || screen_tail_lower.contains("review your answers")
            || screen_tail_lower.contains("skip interview and plan immediately"))
    {
        return DetectedTerminalAgentStatus::Blocked;
    }

    // osc_title_idle (250): the window title starts with Claude Code's
    // per-session "✳" marker.
    if starts_with_idle_marker(terminal_title) {
        return DetectedTerminalAgentStatus::Idle;
    }

    classify_generic(&screen_tail_lower)
}

fn starts_with_working_spinner(title: &str) -> bool {
    let mut chars = title.trim_start().chars();
    match chars.next() {
        Some(character) if is_osc_working_spinner_character(character) => chars.next() == Some(' '),
        _ => false,
    }
}

fn is_osc_working_spinner_character(character: char) -> bool {
    matches!(character, '\u{2800}'..='\u{28FF}' | '\u{25D0}'..='\u{25D3}')
}

fn starts_with_idle_marker(title: &str) -> bool {
    title.trim_start().starts_with('\u{2733}')
}

/// The glyphs Claude Code's live activity line starts with (`*`, `·`, and a
/// handful of star/asterisk variants used as animation frames).
const CLAUDE_ACTIVITY_GLYPHS: [char; 6] = [
    '*', '\u{00B7}', '\u{2722}', '\u{2736}', '\u{273B}', '\u{273D}',
];

fn is_claude_activity_glyph(character: char) -> bool {
    CLAUDE_ACTIVITY_GLYPHS.contains(&character)
}

fn is_claude_running_glyph(character: char) -> bool {
    matches!(character, '⏸' | '⏵')
}

/// Claude Code's live "<glyph> <verb>… (Ns · ...)" activity line, shown while
/// thinking, streaming a reply, or running a tool. Simplified from herdr's
/// exact regex (which also requires a trailing duration marker or end of
/// line right after the ellipsis) to "glyph, space, non-empty text
/// containing an ellipsis", which every observed real form of this line
/// satisfies.
fn is_claude_activity_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    let mut chars = trimmed.chars();
    let Some(glyph) = chars.next() else {
        return false;
    };
    if !is_claude_activity_glyph(glyph) || chars.next() != Some(' ') {
        return false;
    }
    let rest = trimmed[glyph.len_utf8() + 1..].trim_start();
    !rest.is_empty() && rest.contains('…')
}

/// Claude Code's "<pause/play> ... esc to interrupt" activity line -- the
/// other alternative form of the live activity line.
fn is_claude_interrupt_hint_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.chars().next().is_some_and(is_claude_running_glyph)
        && trimmed.contains("esc to interrupt")
}

/// Claude Code's "<pause/play> ... · N shell(s) ..." background-shell status
/// line.
fn is_claude_background_shell_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    if !trimmed.chars().next().is_some_and(is_claude_running_glyph) {
        return false;
    }
    trimmed.split('·').skip(1).any(|segment| {
        let segment = segment.trim_start();
        let digits_end = segment
            .find(|character: char| !character.is_ascii_digit())
            .unwrap_or(0);
        digits_end > 0
            && !segment.starts_with('0')
            && segment[digits_end..].trim_start().starts_with("shell")
    })
}

/// Claude Code's "<glyph> Waiting for N background agents to finish" status
/// line, shown as the last line of live status just above the input box.
fn is_claude_waiting_for_background_agents_line(line: &str) -> bool {
    let trimmed = line.trim();
    let mut chars = trimmed.chars();
    let Some(glyph) = chars.next() else {
        return false;
    };
    if !is_claude_activity_glyph(glyph) {
        return false;
    }
    let Some(rest) = trimmed[glyph.len_utf8()..]
        .trim_start()
        .strip_prefix("Waiting for ")
    else {
        return false;
    };
    let digits_end = rest
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(0);
    digits_end > 0
        && !rest.starts_with('0')
        && rest[digits_end..]
            .trim_start()
            .starts_with("background agent")
}

fn is_btw_command_line(line: &str) -> bool {
    match line.trim_start().strip_prefix("/btw") {
        Some(rest) => rest.is_empty() || rest.starts_with(char::is_whitespace),
        None => false,
    }
}

fn line_ends_with_esc_to_close(line: &str) -> bool {
    line.trim_end()
        .to_ascii_lowercase()
        .ends_with("esc to close")
}

fn strip_choice_prefix(line: &str) -> &str {
    let trimmed = line.trim_start();
    match trimmed.strip_prefix('❯') {
        Some(rest) => rest.trim_start(),
        None => trimmed,
    }
}

fn word_starts(text: &str, word: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    match lower.strip_prefix(word) {
        Some(after) => after
            .chars()
            .next()
            .is_none_or(|character| !character.is_alphanumeric()),
        None => false,
    }
}

fn line_selects_bare_word(line: &str, word: &str) -> bool {
    word_starts(strip_choice_prefix(line), word)
}

fn line_selects_choice(line: &str, number: &str, word: &str) -> bool {
    let Some(rest) = strip_choice_prefix(line).strip_prefix(number) else {
        return false;
    };
    let Some(rest) = rest.strip_prefix('.') else {
        return false;
    };
    word_starts(rest.trim_start(), word)
}

fn is_horizontal_rule(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    let rule_chars = trimmed
        .chars()
        .take_while(|&character| character == '─')
        .count();
    if rule_chars == 0 {
        return false;
    }
    let rule_bytes = trimmed
        .char_indices()
        .nth(rule_chars)
        .map(|(index, _)| index)
        .unwrap_or(trimmed.len());
    let suffix = trimmed[rule_bytes..].trim_start();
    suffix.is_empty() || rule_chars >= 3
}

fn after_last_horizontal_rule(content: &str) -> &str {
    let mut last_rule_end = 0usize;
    let mut offset = 0usize;
    for line in content.lines() {
        let next_offset = offset + line.len() + 1;
        if is_horizontal_rule(line) {
            last_rule_end = next_offset.min(content.len());
        }
        offset = next_offset;
    }
    &content[last_rule_end..]
}

fn prompt_box_body(content: &str) -> Option<&str> {
    let lines: Vec<&str> = content.lines().collect();
    let top = prompt_box_top_border_index(&lines)?;
    let start = line_start_offset(content, &lines, top + 1);
    let end_index = lines[top + 1..]
        .iter()
        .position(|line| is_horizontal_rule(line))
        .map(|relative| top + 1 + relative)
        .unwrap_or(lines.len());
    let end = line_start_offset(content, &lines, end_index);
    Some(&content[start.min(content.len())..end.min(content.len())])
}

/// Everything before the live input box's top border -- the trailing status
/// line just above it (e.g. a "waiting for background agents" line) lives
/// here, not inside the box itself.
fn above_prompt_box(content: &str) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    let Some(top) = prompt_box_top_border_index(&lines) else {
        return content;
    };
    let end = line_start_offset(content, &lines, top);
    &content[..end.min(content.len())]
}

fn prompt_box_top_border_index(lines: &[&str]) -> Option<usize> {
    let mut border_count = 0;
    for index in (0..lines.len()).rev() {
        if is_horizontal_rule(lines[index]) {
            border_count += 1;
            if border_count == 2 {
                return Some(index);
            }
        }
    }
    None
}

fn bottom_non_empty_lines(content: &str, count: usize) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    let Some(start_index) = lines
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, line)| !line.trim().is_empty())
        .take(count)
        .last()
        .map(|(index, _)| index)
    else {
        return "";
    };
    slice_from_line_index(content, &lines, start_index)
}

fn last_non_empty_line(content: &str) -> &str {
    content
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
}

fn slice_from_line_index<'a>(content: &'a str, lines: &[&str], index: usize) -> &'a str {
    let byte_offset = line_start_offset(content, lines, index);
    &content[byte_offset.min(content.len())..]
}

fn line_start_offset(content: &str, lines: &[&str], index: usize) -> usize {
    lines[..index.min(lines.len())]
        .iter()
        .map(|line| line.len() + 1)
        .sum::<usize>()
        .min(content.len())
}

fn classify_generic(screen_tail: &str) -> DetectedTerminalAgentStatus {
    let last_line = screen_tail
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if last_line.ends_with("[y/n]")
        || last_line.ends_with("(y/n)")
        || last_line.ends_with("[yes/no]")
        || last_line.ends_with("(yes/no)")
        || last_line.ends_with("(yes/no/[fingerprint])?")
        || last_line.ends_with("password:")
        || last_line.ends_with("passphrase:")
    {
        DetectedTerminalAgentStatus::Blocked
    } else {
        DetectedTerminalAgentStatus::Unknown
    }
}

fn is_spinner_character(character: char) -> bool {
    matches!(
        character,
        '⠋' | '⠙' | '⠹' | '⠸' | '⠼' | '⠴' | '⠦' | '⠧' | '⠇' | '⠏'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_status_overrides_old_screen_output() {
        for status in ["Starting", "Working", "Thinking", "Waiting"] {
            assert_eq!(
                classify(
                    "codex",
                    "Allow command?",
                    &format!("Fix sidebar | session | {status}")
                ),
                DetectedTerminalAgentStatus::Working
            );
        }
        assert_eq!(
            classify(
                "codex",
                "Working (1s • esc to interrupt)\nAllow command?",
                "Fix sidebar | session | Ready"
            ),
            DetectedTerminalAgentStatus::Idle
        );
        for prefix in ["[ ! ]", "[ . ]"] {
            assert_eq!(
                classify(
                    "codex",
                    "",
                    &format!("{prefix} Action Required | Fix sidebar | session")
                ),
                DetectedTerminalAgentStatus::Blocked
            );
        }
    }

    #[test]
    fn classifies_codex_status() {
        assert_eq!(
            classify("codex", "", "01a0960e ⠦ Working"),
            DetectedTerminalAgentStatus::Working
        );
        assert_eq!(
            classify("codex", "Allow command?", "01a0960e"),
            DetectedTerminalAgentStatus::Blocked
        );
        assert_eq!(
            classify("codex", "", "01a0960e"),
            DetectedTerminalAgentStatus::Idle
        );
    }

    #[test]
    fn classifies_pi_status() {
        assert_eq!(
            classify("pi", "Working...", ""),
            DetectedTerminalAgentStatus::Working
        );
        assert_eq!(
            classify("pi", "thinking about the task, Working... now", ""),
            DetectedTerminalAgentStatus::Working
        );
        assert_eq!(
            classify("pi", "Continue? [y/n]", ""),
            DetectedTerminalAgentStatus::Blocked
        );
        assert_eq!(
            classify("pi", "some other output", ""),
            DetectedTerminalAgentStatus::Unknown
        );
    }

    #[test]
    fn classifies_agy_status() {
        assert_eq!(
            classify(
                "agy",
                "requesting permission for: bash(rm -rf /tmp/x)\ndo you want to proceed?",
                "",
            ),
            DetectedTerminalAgentStatus::Blocked
        );
        assert_eq!(
            classify(
                "agy",
                "requesting permission for: edit(main.rs)\ntab amend · edit command",
                "",
            ),
            DetectedTerminalAgentStatus::Blocked
        );
        assert_eq!(
            classify("agy", "requesting permission for: bash(ls)", ""),
            DetectedTerminalAgentStatus::Unknown
        );
        assert_eq!(
            classify("agy", "⠋ Thinking", ""),
            DetectedTerminalAgentStatus::Working
        );
        assert_eq!(
            classify("agy", "  ⠙⠹ Analyzing the codebase", ""),
            DetectedTerminalAgentStatus::Working
        );
        assert_eq!(
            classify("agy", "⠋ done", ""),
            DetectedTerminalAgentStatus::Unknown
        );
        assert_eq!(
            classify("agy", "· 3 tasks running", ""),
            DetectedTerminalAgentStatus::Working
        );
        assert_eq!(
            classify("agy", "· 0 tasks running", ""),
            DetectedTerminalAgentStatus::Unknown
        );
        assert_eq!(
            classify("agy", "some other output", ""),
            DetectedTerminalAgentStatus::Unknown
        );
    }

    // Ported from herdr's upstream manifest: `live_strong_blocker` is scoped
    // to text after the last `›` prompt marker, and `screen_working_fallback`
    // is vetoed by "■ Conversation interrupted" -- both dropped from Pentip's
    // original unscoped codex classifier.

    #[test]
    fn codex_stale_prompt_before_last_marker_does_not_block_status() {
        // "Allow command?" resolved before Codex printed a fresh `›` marker
        // and moved on; only text after the last marker should count.
        let screen = "Allow command?\n› some new output\nEverything looks good";
        assert_ne!(
            classify("codex", screen, "01a0960e"),
            DetectedTerminalAgentStatus::Blocked
        );
    }

    #[test]
    fn codex_working_fallback_line_is_working() {
        for bullet in ['•', '◦'] {
            assert_eq!(
                classify(
                    "codex",
                    &format!("{bullet} Working (12s · esc to interrupt)"),
                    "",
                ),
                DetectedTerminalAgentStatus::Working,
                "{bullet} Working (...) should be Working",
            );
        }
    }

    #[test]
    fn codex_working_fallback_does_not_fire_when_conversation_interrupted() {
        let screen = "• Working (12s · esc to interrupt)\n■ Conversation interrupted";
        assert_ne!(
            classify("codex", screen, ""),
            DetectedTerminalAgentStatus::Working
        );
    }

    // Claude Code always keeps a bordered input box plus two footer status lines
    // below the live content, captured here (via a real Claude Code session) so
    // the classifier is tested against the shape it actually has to see through.
    const CLAUDE_FOOTER_CHROME: &str = "\
────────────────────────────────────────────────────────────────
❯
────────────────────────────────────────────────────────────────
  Pentip (main) | Sonnet 5 | ctx:6% | 5h:24%(@16:10) 7d:3%(in 5d1h)
  ⏵⏵ auto mode on (shift+tab to cycle) · ← for agents";

    // Ported from flint's `attention_detection` test suite (same rules,
    // adapted to this classifier's combined screen_tail+title signature).

    #[test]
    fn claude_permission_prompt_is_blocked() {
        let screen =
            "Do you want to proceed?\n❯ 1. Yes\n  2. No, and tell Claude what to do differently";
        assert_eq!(
            classify("claude", screen, ""),
            DetectedTerminalAgentStatus::Blocked
        );
    }

    #[test]
    fn claude_dynamic_workflow_prompt_is_blocked() {
        let screen = "Run a dynamic workflow?\nesc to cancel";
        assert_eq!(
            classify("claude", screen, ""),
            DetectedTerminalAgentStatus::Blocked
        );
    }

    #[test]
    fn claude_legacy_yes_no_prompt_is_blocked() {
        let screen = "Do you want to make this change?\n❯ Yes";
        assert_eq!(
            classify("claude", screen, ""),
            DetectedTerminalAgentStatus::Blocked
        );
    }

    #[test]
    fn claude_bare_prompt_marker_is_not_blocked_by_the_legacy_fallback() {
        // The legacy fallback's veto exists specifically to keep a bare `❯`
        // idle prompt (no accompanying "do you want to"/"would you like to"
        // text, and no bordered box to match `live_prompt_box` either) from
        // being misread as a yes/no blocker.
        assert_eq!(
            classify("claude", "❯", ""),
            DetectedTerminalAgentStatus::Unknown
        );
    }

    #[test]
    fn claude_busy_spinner_osc_title_is_working() {
        for title in ["⠋ Thinking", "◐ Claude Code", "◑ Claude Code"] {
            assert_eq!(
                classify("claude", "", title),
                DetectedTerminalAgentStatus::Working,
                "{title} should be Working",
            );
        }
    }

    #[test]
    fn claude_osc_title_idle_marker_beats_a_plain_screen() {
        assert_eq!(
            classify("claude", "", "✳ History of tea"),
            DetectedTerminalAgentStatus::Idle
        );
    }

    #[test]
    fn higher_priority_rule_wins_over_a_lower_priority_match() {
        // `live_blocked_form` (priority 980) and `legacy_no_prompt_blocker`
        // (priority 300) can both match the same screen; this only proves
        // priority resolution picks the higher one rather than e.g.
        // declaration order by accident.
        let screen = "───────\nesc to cancel\nenter to confirm";
        assert_eq!(
            classify("claude", screen, ""),
            DetectedTerminalAgentStatus::Blocked
        );
    }

    #[test]
    fn classifies_claude_status() {
        assert_eq!(
            classify(
                "claude",
                "Do you want to proceed?\n❯ 1. Yes\n\nEnter to confirm · Esc to cancel",
                "",
            ),
            DetectedTerminalAgentStatus::Blocked
        );
        assert_eq!(
            classify("claude", "", "◐ Poem about the ocean"),
            DetectedTerminalAgentStatus::Working
        );
        assert_eq!(
            classify(
                "claude",
                &format!("✻ Cogitated for 9s · done 3:09 PM\n\n{CLAUDE_FOOTER_CHROME}"),
                "✳ Poem about the ocean",
            ),
            DetectedTerminalAgentStatus::Idle
        );
    }

    #[test]
    fn claude_footer_chrome_does_not_hide_the_live_prompt_box() {
        // A window that only looks at the last 3-4 lines never reaches past
        // the persistent footer chrome to see the input box's border at all;
        // `live_prompt_box` finds it by scanning back for the last two
        // horizontal rules instead of counting lines, so it isn't affected
        // by how much footer chrome follows the box.
        assert_eq!(
            classify(
                "claude",
                &format!("✻ Cogitated for 9s · done 3:09 PM\n\n{CLAUDE_FOOTER_CHROME}"),
                "",
            ),
            DetectedTerminalAgentStatus::Idle
        );
    }

    // Ported from herdr's upstream manifest (`live_turn_working`,
    // `background_shell_working`, `background_agents_working`), which Flint's
    // own port dropped -- these are the main screen-body "is it still
    // working" signals for Claude Code, needed because the OSC title alone
    // (see `claude_busy_spinner_osc_title_is_working`) doesn't reliably flip
    // to a spinner glyph during a long streamed reply.

    #[test]
    fn claude_live_activity_line_is_working() {
        // The activity line sits 6+ lines above the bottom once the footer
        // chrome is included, which is exactly why `live_turn_working` reads
        // the bottom 12 non-empty lines rather than a much smaller window.
        assert_eq!(
            classify(
                "claude",
                &format!("✽ Flummoxing… (2s · ↓ 12 tokens)\n\n{CLAUDE_FOOTER_CHROME}"),
                "",
            ),
            DetectedTerminalAgentStatus::Working
        );
        // Mid-animation, before the "(Ns · ...)" duration suffix appears.
        assert_eq!(
            classify("claude", "· Flummoxing…", ""),
            DetectedTerminalAgentStatus::Working
        );
    }

    #[test]
    fn claude_interrupt_hint_line_is_working() {
        assert_eq!(
            classify("claude", "⏵ Running… (12s · esc to interrupt)", ""),
            DetectedTerminalAgentStatus::Working
        );
    }

    #[test]
    fn claude_background_shell_line_is_working() {
        assert_eq!(
            classify(
                "claude",
                "⏵ Working (12s) · 2 shells · esc to interrupt",
                ""
            ),
            DetectedTerminalAgentStatus::Working
        );
    }

    #[test]
    fn claude_waiting_for_background_agents_is_working() {
        let screen = format!("✻ Waiting for 2 background agents to finish\n{CLAUDE_FOOTER_CHROME}");
        assert_eq!(
            classify("claude", &screen, ""),
            DetectedTerminalAgentStatus::Working
        );
    }

    #[test]
    fn claude_btw_overlay_is_working() {
        let screen = "/btw what does this function do?\n\nEsc to close";
        assert_eq!(
            classify("claude", screen, ""),
            DetectedTerminalAgentStatus::Working
        );
    }

    #[test]
    fn claude_done_summary_line_is_not_mistaken_for_activity() {
        // The "done" summary line that replaces the activity line once a turn
        // finishes reuses the same glyphs but never contains an ellipsis.
        assert_eq!(
            classify(
                "claude",
                &format!("✻ Cogitated for 9s · done 3:09 PM\n\n{CLAUDE_FOOTER_CHROME}"),
                "",
            ),
            DetectedTerminalAgentStatus::Idle
        );
    }

    #[test]
    fn stale_claude_prompt_in_scrollback_does_not_block_status() {
        // A resolved confirmation prompt (no real horizontal rules around it)
        // stays in the terminal's scrollback well after Claude has moved on;
        // the live input box's bare `❯` line vetoes the legacy fallback that
        // would otherwise still see the old prompt text.
        let screen_tail = format!(
            "Do you want to proceed?\n❯ 1. Yes\nEnter to confirm · Esc to cancel\n{}{CLAUDE_FOOTER_CHROME}",
            "filler line\n".repeat(20)
        );
        assert_eq!(
            classify("claude", &screen_tail, ""),
            DetectedTerminalAgentStatus::Idle
        );
    }
}
