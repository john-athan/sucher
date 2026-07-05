// Type-to-select typeahead for the directory browser's Browse mode (ADR 0002).
//
// A timed *session*: a buffer of typed characters plus the `Instant` of the
// last keystroke. While the session is active (last keystroke within `TIMEOUT`)
// every printable key extends the prefix; once it lapses, the browser's vim
// motion bindings win again. This module is deliberately pure — no ratatui, no
// clock, no IO — so the precedence and timeout reset are unit-tested without a
// terminal or real waiting; the caller injects `Instant::now()`.

use std::time::{Duration, Instant};

/// A session stays active while `now - last < TIMEOUT` (ADR 0002 D1).
pub const TIMEOUT: Duration = Duration::from_millis(900);

/// What a printable keypress should do, decided by session state × binding.
pub enum Action {
    /// Session active: append the char to the buffer and re-match.
    Append,
    /// Idle and the char is bound to nothing: begin a session with it.
    StartNew,
    /// Idle and the char is a browse binding: let the vim motion run.
    PassThrough,
}

/// A session is active while `now - last < timeout`. No prior keystroke
/// (`last == None`) is never active.
pub fn active(now: Instant, last: Option<Instant>, timeout: Duration) -> bool {
    match last {
        Some(last) => now.duration_since(last) < timeout,
        None => false,
    }
}

/// Precedence (ADR 0002 D1): an active session captures every printable key;
/// when idle, a bound key runs its motion and an unbound key opens a session.
pub fn action(active: bool, key_is_bound: bool) -> Action {
    if active {
        Action::Append
    } else if key_is_bound {
        Action::PassThrough
    } else {
        Action::StartNew
    }
}

/// First entry whose name case-insensitively starts with `buffer`, in order.
/// `None` for an empty buffer or no match.
pub fn match_prefix<S: AsRef<str>>(names: &[S], buffer: &str) -> Option<usize> {
    if buffer.is_empty() {
        return None;
    }
    let needle = buffer.to_lowercase();
    names
        .iter()
        .position(|n| n.as_ref().to_lowercase().starts_with(&needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_within_and_after_timeout() {
        let now = Instant::now();
        // Last keystroke half a timeout ago → still active.
        let recent = now.checked_sub(TIMEOUT / 2).unwrap();
        assert!(active(now, Some(recent), TIMEOUT));
        // Last keystroke a whole timeout-plus ago → lapsed.
        let stale = now.checked_sub(TIMEOUT + Duration::from_millis(1)).unwrap();
        assert!(!active(now, Some(stale), TIMEOUT));
        // No prior keystroke → never active.
        assert!(!active(now, None, TIMEOUT));
    }

    #[test]
    fn action_three_cases() {
        // Active always appends, whatever the binding.
        assert!(matches!(action(true, true), Action::Append));
        assert!(matches!(action(true, false), Action::Append));
        // Idle: bound → the motion runs; unbound → a session starts.
        assert!(matches!(action(false, true), Action::PassThrough));
        assert!(matches!(action(false, false), Action::StartNew));
    }

    #[test]
    fn match_prefix_first_in_order() {
        let names = ["Cargo.toml", "README.md", "readme.txt", "src"];
        // First of several matches wins, and matching is case-insensitive.
        assert_eq!(match_prefix(&names, "rea"), Some(1));
        assert_eq!(match_prefix(&names, "READ"), Some(1));
        assert_eq!(match_prefix(&names, "s"), Some(3));
        // Empty buffer and no match both yield None.
        assert_eq!(match_prefix(&names, ""), None);
        assert_eq!(match_prefix(&names, "zzz"), None);
    }
}
