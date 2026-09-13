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

    let screen_tail = screen_tail.to_ascii_lowercase();
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

    if terminal_title_lowercase.contains("action required")
        || [
            "press enter to confirm or esc to cancel",
            "enter to submit answer",
            "enter to submit all",
            "allow command?",
            "[y/n]",
            "yes (y)",
        ]
        .iter()
        .any(|pattern| screen_tail.contains(pattern))
        || ((screen_tail.contains("do you want to") || screen_tail.contains("would you like to"))
            && (screen_tail.contains("yes") || screen_tail.contains('❯')))
    {
        return DetectedTerminalAgentStatus::Blocked;
    }

    if terminal_title
        .split_whitespace()
        .any(|word| word.chars().any(is_spinner_character))
        || screen_tail
            .lines()
            .rev()
            .take(3)
            .any(|line| line.contains("working (") && line.contains("esc to interrupt"))
    {
        return DetectedTerminalAgentStatus::Working;
    }

    if !terminal_title.trim().is_empty() {
        DetectedTerminalAgentStatus::Idle
    } else {
        DetectedTerminalAgentStatus::Unknown
    }
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
}
