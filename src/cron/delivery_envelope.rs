//! The delivery envelope prepended to every cron turn message (#819).
//!
//! A cron turn is not a conversation. Its final text is delivered by the
//! scheduler, so the model never has to publish anything — but until #819
//! nothing in the turn said so, and the turn message was `job.prompt`
//! byte-for-byte (`scheduler::execute_job`). A job that decided to "post its
//! report" called a send tool, and the scheduler then delivered the same
//! content again.
//!
//! The duplicate is the smaller half. On 2026-11-12 a job used a third-party
//! MCP messaging tool, which posts on the OWNER's account; the bound chat then
//! ingested those five messages as inbound instructions from the owner and
//! executed them (six drafts, fixture edits, two commits). The envelope names
//! that failure mode explicitly, because "you will duplicate the post" is a
//! weaker deterrent than "your message will be obeyed as the owner's order".
//!
//! Placement is the turn message, not the system prompt: the envelope carries
//! per-job facts (name, targets) that the system brain has no identity to key
//! on, and the turn message is part of the history the compactor summarizes,
//! so the rule reaches the next context as well. Enforcement — bringing
//! MCP-provided sends under the send scope — is #819 leg 2, and closing the
//! self-echo loop is leg 3.

/// Marker line opening the envelope. Tests pin it, and a grep for it finds
/// every envelope ever written into a session history.
pub const DELIVERY_ENVELOPE_MARKER: &str = "--- CRON DELIVERY ---";

/// Collapse a job name to one line.
///
/// The name reaches this module from user input (`cron_jobs.name`), and a
/// newline in it would let a job title forge a section of the envelope —
/// including a second `Delivery targets:` line. Flattening costs nothing and
/// removes the class.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The envelope for a job that HAS at least one configured target.
fn with_targets(job_name: &str, targets: &[&str]) -> String {
    format!(
        "{DELIVERY_ENVELOPE_MARKER}\n\
         Scheduled job: {job_name}\n\
         Delivery targets: {list}\n\
         \n\
         Your FINAL TEXT is the deliverable. When this turn ends the scheduler posts it, \
         verbatim, to the target(s) above. Nothing else you produce is delivered.\n\
         \n\
         - Do NOT call a send tool to publish your result. The scheduler already does it; a \
         send tool delivers the same content a second time.\n\
         - Do not call `telegram_send`, `discord_send`, `slack_send`, `whatsapp_send`, or any \
         other tool that posts a message on your behalf. Some of those tools live in MCP \
         servers rather than in this binary, so this list cannot name them all.\n\
         - Never post on the owner's own account. A message sent that way is ingested by the \
         chat as an INBOUND instruction from the owner, and that session will execute it as \
         an order. That is worse than a duplicate.\n\
         - Reaching a DIFFERENT chat is outside this job's configuration: say so in your final \
         text instead of sending.\n\
         - Put everything a human must read in the final text, in the order it should be read.",
        list = targets.join(", "),
    )
}

/// The envelope for a job with NO delivery target: nothing is posted anywhere,
/// so the only record of the run is its row in the run history.
fn without_targets(job_name: &str) -> String {
    format!(
        "{DELIVERY_ENVELOPE_MARKER}\n\
         Scheduled job: {job_name}\n\
         Delivery targets: NONE\n\
         \n\
         This job has no delivery target. Nothing is posted anywhere: the run's only record is \
         its row in the cron run history.\n\
         \n\
         - Do NOT call a send tool. There is no configured destination, so any send reaches a \
         chat this job was never given.\n\
         - Your final text is the run's result; write it for whoever reads the run record."
    )
}

/// Build the envelope for one job.
///
/// `deliver_to` is the stored comma-separated target list; blank entries are
/// dropped, so `Some(" , ")` is treated as targetless rather than as a target
/// named " ".
pub fn delivery_envelope(job_name: &str, deliver_to: Option<&str>) -> String {
    let name = one_line(job_name);
    let targets: Vec<String> = deliver_to
        .unwrap_or("")
        .split(',')
        .map(one_line)
        .filter(|s| !s.is_empty())
        .collect();
    if targets.is_empty() {
        without_targets(&name)
    } else {
        let targets: Vec<&str> = targets.iter().map(String::as_str).collect();
        with_targets(&name, &targets)
    }
}

/// Prepend the envelope to a turn prompt.
///
/// The prompt itself is passed through byte-for-byte: the envelope is added
/// information, never a rewrite of the job's own instructions. Call this
/// exactly once per turn — the single call site in `scheduler::execute_job` is
/// pinned by `tests::cron_delivery_envelope_test`.
pub fn wrap_turn_prompt(prompt: &str, job_name: &str, deliver_to: Option<&str>) -> String {
    let envelope = delivery_envelope(job_name, deliver_to);
    if prompt.trim().is_empty() {
        envelope
    } else {
        format!("{envelope}\n\n{prompt}")
    }
}
