//! Turn a shell command into a label a human can read at a glance (#790, #791).
//!
//! Every long-running command the agent issues starts by entering the working
//! directory, so a label that keeps the `cd` keeps the one part that is
//! identical for every command and drops the part that identifies it. At the 28
//! characters the input-border indicator allows, that leaves a field showing
//! nothing but a path.
//!
//! Multi-line commands carry a second problem: a newline reaching a single-line
//! renderer is not drawn, so the tail of one line touches the head of the next
//! and produces a token that was never in the command (`opencrabs` + `echo` ->
//! `opencrabsecho`). That reads as a real command and is not one, which is
//! worse than truncation.
//!
//! Both are fixed here, once, so the TUI rows, the border indicator and the
//! channel summaries cannot drift apart.

/// A single-line, human-readable label for `command`.
///
/// Leading `cd <path>` segments are dropped, every remaining segment is kept,
/// and segments are joined with `"; "` so nothing can fuse across a break.
/// Returns an empty string for an empty command; callers supply their own
/// placeholder.
pub fn command_label(command: &str) -> String {
    let segments = split_segments(command);
    if segments.is_empty() {
        return String::new();
    }

    // Strip only LEADING `cd`s and LEADING bare assignments. A `cd` in the
    // middle or at the end is doing something the reader needs to see, and
    // dropping it would misdescribe the command. Taking only the last segment
    // (the previous behaviour) silently discarded real work:
    // `cd /x && cargo build && cargo test` labelled as `cargo test` alone.
    //
    // A leading `VAR=value` is the same class of preamble as the `cd`: it says
    // where the run happened and who it ran as, never what it did. Left in, it
    // wins the label outright, because it comes first and the clip is 60
    // chars — a smoke command reading `SD=~/.opencrabs/profiles/ops/tmp/…;
    // SID=facd50af-…; grep …` labelled every row with its own setup and the
    // actual command never appeared (#808).
    let mut kept: Vec<&String> = segments
        .iter()
        .skip_while(|s| is_bare_cd(s) || is_pure_assignment(s))
        .collect();

    // A command that is nothing but `cd` still has to render as something.
    if kept.is_empty() {
        kept = segments.iter().collect();
    }

    kept.iter()
        .map(|s| collapse_whitespace(s))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Is this segment a bare directory change and nothing else?
///
/// Segments are already split on every separator, so everything after `cd ` is
/// the path, spaces and quotes included.
fn is_bare_cd(segment: &str) -> bool {
    segment
        .strip_prefix("cd ")
        .is_some_and(|rest| !rest.trim().is_empty())
}

/// Split on top-level `\n`, `;` and `&&`, ignoring separators inside quotes.
///
/// Quote tracking is what makes this safe to run on arbitrary commands: a
/// naive `split(';')` would cut `echo "a; b"` in half and invent a segment.
/// The scan itself lives in `shell_scan` because the background-task
/// classifier needs the same distinction between syntax and data.
fn split_segments(command: &str) -> Vec<String> {
    let scanned = crate::utils::shell_scan::scan(command);
    let is_syntax = |c: &crate::utils::shell_scan::ShellChar| !c.literal && !c.quote_mark;

    let mut segments = Vec::new();
    let mut current = String::new();
    let mut i = 0;
    while let Some(c) = scanned.get(i) {
        let double_amp = c.ch == '&'
            && is_syntax(c)
            && scanned
                .get(i + 1)
                .is_some_and(|n| n.ch == '&' && is_syntax(n));
        if double_amp {
            segments.push(std::mem::take(&mut current));
            i += 2;
            continue;
        }
        if is_syntax(c) && (c.ch == '\n' || c.ch == ';') {
            segments.push(std::mem::take(&mut current));
        } else {
            current.push(c.ch);
        }
        i += 1;
    }
    segments.push(current);

    segments
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Collapse runs of whitespace so a command formatted across columns reads as
/// one line. Any newline inside a segment is impossible (they are separators),
/// so this only tidies runs of spaces and tabs.
fn collapse_whitespace(segment: &str) -> String {
    segment.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Is this segment environment assignments and nothing else?
///
/// `FOO=bar` and `FOO="two words"` are pure setup, and so is a run of them
/// (`A=1 B=2`). `FOO=bar cargo test` assigns AND runs, so it is kept whole —
/// dropping it would lose the command. Anything that runs as part of the value
/// (`X=$(date)`, `` X=`date` ``) is work too, so it is kept: a substitution is a
/// command the reader may need to see, and a label that hides it describes a
/// different command.
fn is_pure_assignment(segment: &str) -> bool {
    let mut rest = segment.trim();
    loop {
        let Some((name, after_eq)) = rest.split_once('=') else {
            return false;
        };
        if !is_shell_name(name) {
            // `cargo test FOO=bar` — the assignment is an argument, not setup.
            return false;
        }
        match value_word_len(after_eq) {
            Some(consumed) => rest = after_eq[consumed..].trim_start(),
            None => return false,
        }
        if rest.is_empty() {
            // Consuming at least one `NAME=value` is what got us here, so this
            // is a pure assignment run. An empty segment never reaches the loop.
            return true;
        }
    }
}

/// `NAME` must be a valid shell identifier, else `=` belongs to something else.
fn is_shell_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Length of the single shell word at the head of `rest`, or `None` when the
/// value runs a command (`$(..)`, backticks) or is malformed.
fn value_word_len(rest: &str) -> Option<usize> {
    if rest.contains("$(") || rest.contains('`') {
        return None;
    }
    let bytes = rest.as_bytes();
    let Some(&first) = bytes.first() else {
        return Some(0);
    };
    if first == b'"' || first == b'\'' {
        let mut i = 1;
        while i < bytes.len() {
            if bytes[i] == b'\\' && first == b'"' {
                i += 2;
                continue;
            }
            if bytes[i] == first {
                return Some(i + 1);
            }
            i += 1;
        }
        return None;
    }
    Some(
        rest.find(char::is_whitespace)
            .unwrap_or(rest.len()),
    )
}
