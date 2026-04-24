//! Console command evaluator.
//!
//! Routes console input through a fast-path for trivial commands,
//! then falls back to NLP -> brain for everything else.
//! Wired into the console overlay by WU-5 (integration).

/// Result of evaluating a console command.
#[derive(Debug, Clone)]
pub enum EvalResult {
    /// Command executed successfully with optional output.
    Ok(Option<String>),
    /// Command failed with an error message.
    Err(String),
    /// Command is being processed asynchronously (brain/NLP).
    /// The caller should poll for results.
    Pending(String),
    /// Unrecognized command with suggestions.
    Unknown {
        input: String,
        suggestions: Vec<String>,
    },
}

/// Built-in trivial commands (fast path, no NLP needed).
const BUILTIN_COMMANDS: &[(&str, &str)] = &[
    ("clear", "Clear the console scrollback"),
    ("quit", "Exit Phantom"),
    ("exit", "Exit Phantom"),
    ("help", "Show available commands"),
    ("version", "Show Phantom version"),
    ("debug.draw on", "Enable debug draw overlay"),
    ("debug.draw off", "Disable debug draw overlay"),
    ("log.verbose", "Set log verbosity (0-4)"),
    ("log.channel", "Toggle a log channel on/off"),
];

/// Evaluate a console command string.
///
/// Returns immediately for built-in commands. For everything else,
/// returns `EvalResult::Pending` -- the caller should submit the input
/// to the NLP/brain pipeline via the job queue.
pub fn evaluate(input: &str) -> EvalResult {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return EvalResult::Ok(None);
    }

    // Fast path: exact match on built-in commands
    let lower = trimmed.to_lowercase();

    match lower.as_str() {
        "clear" => EvalResult::Ok(Some("Console cleared.".into())),
        "quit" | "exit" => EvalResult::Ok(Some("__quit__".into())),
        "help" => {
            let mut help = String::from("Available commands:\n");
            for (cmd, desc) in BUILTIN_COMMANDS {
                help.push_str(&format!("  {cmd:<20} {desc}\n"));
            }
            help.push_str("\nAnything else is routed through NLP -> brain.");
            EvalResult::Ok(Some(help))
        }
        "version" => EvalResult::Ok(Some(format!("Phantom v{}", env!("CARGO_PKG_VERSION")))),
        "debug.draw on" => EvalResult::Ok(Some("Debug draw enabled.".into())),
        "debug.draw off" => EvalResult::Ok(Some("Debug draw disabled.".into())),
        _ => {
            // Check for parameterized builtins
            if let Some(rest) = lower.strip_prefix("log.verbose ") {
                if let Result::Ok(level) = rest.trim().parse::<u8>() {
                    if level <= 4 {
                        return EvalResult::Ok(Some(format!("Verbosity set to {level}.")));
                    }
                }
                return EvalResult::Err("Usage: log.verbose <0-4>".into());
            }

            if lower.starts_with("log.channel ") {
                let parts: Vec<&str> = trimmed.split_whitespace().collect();
                if parts.len() == 3 && (parts[2] == "on" || parts[2] == "off") {
                    return EvalResult::Ok(Some(format!(
                        "Channel '{}' {}.",
                        parts[1], parts[2]
                    )));
                }
                return EvalResult::Err("Usage: log.channel <name> on|off".into());
            }

            // Not a builtin -- route to NLP -> brain
            EvalResult::Pending(format!("Routing to brain: \"{trimmed}\""))
        }
    }
}

/// Find commands that are similar to the input (for suggestions).
pub fn suggest(input: &str) -> Vec<String> {
    let lower = input.to_lowercase();
    BUILTIN_COMMANDS
        .iter()
        .filter(|(cmd, _)| cmd.starts_with(&lower) || levenshtein_close(cmd, &lower))
        .map(|(cmd, _)| (*cmd).to_string())
        .collect()
}

/// Simple "close enough" check (prefix match or shared start).
fn levenshtein_close(a: &str, b: &str) -> bool {
    if a.len() < 2 || b.len() < 2 {
        return false;
    }
    let prefix_len = a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count();
    prefix_len >= 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_clear() {
        let result = evaluate("clear");
        assert!(matches!(
            result,
            EvalResult::Ok(Some(ref s)) if s.contains("cleared")
        ));
    }

    #[test]
    fn builtin_quit() {
        let result = evaluate("quit");
        assert!(matches!(
            result,
            EvalResult::Ok(Some(ref s)) if s == "__quit__"
        ));
    }

    #[test]
    fn builtin_help() {
        let result = evaluate("help");
        assert!(matches!(
            result,
            EvalResult::Ok(Some(ref s)) if s.contains("Available")
        ));
    }

    #[test]
    fn builtin_version() {
        let result = evaluate("version");
        assert!(matches!(
            result,
            EvalResult::Ok(Some(ref s)) if s.starts_with("Phantom v")
        ));
    }

    #[test]
    fn empty_input() {
        let result = evaluate("");
        assert!(matches!(result, EvalResult::Ok(None)));
    }

    #[test]
    fn unknown_routes_to_brain() {
        let result = evaluate("deploy staging");
        assert!(matches!(result, EvalResult::Pending(_)));
    }

    #[test]
    fn log_verbose_valid() {
        let result = evaluate("log.verbose 3");
        assert!(matches!(
            result,
            EvalResult::Ok(Some(ref s)) if s.contains("3")
        ));
    }

    #[test]
    fn log_verbose_invalid() {
        let result = evaluate("log.verbose abc");
        assert!(matches!(result, EvalResult::Err(_)));
    }

    #[test]
    fn log_channel_toggle() {
        let result = evaluate("log.channel brain off");
        assert!(matches!(
            result,
            EvalResult::Ok(Some(ref s)) if s.contains("brain")
        ));
    }

    #[test]
    fn case_insensitive() {
        let result = evaluate("CLEAR");
        assert!(matches!(result, EvalResult::Ok(Some(_))));
    }

    #[test]
    fn suggest_finds_prefix() {
        let suggestions = suggest("cle");
        assert!(suggestions.contains(&"clear".to_string()));
    }

    #[test]
    fn suggest_empty_for_unknown() {
        let suggestions = suggest("xyz");
        assert!(suggestions.is_empty());
    }
}
