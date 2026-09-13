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
    if agent_cli != "codex" {
        return classify_generic(screen_tail);
    }
    classify_codex(screen_tail, terminal_title)
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

}
