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
//!
//! #1042 — the envelope is keyed on what the HARNESS will actually do, not on
//! `deliver_to` alone. #969 split send permission from auto-delivery: a
//! `deliver_mode = silent` job keeps its `deliver_to` send scope but the
//! harness posts nothing for it (`scheduler::auto_delivery_targets` is empty).
//! Until this arm existed such a job was told "Your FINAL TEXT is the
//! deliverable … Do NOT call a send tool. The scheduler already does it" — the
//! exact opposite of the truth — so it composed a correct message and posted
//! nothing, and the run row still read `success`. There are therefore THREE
//! arms, and `delivers_result` selects between the two that have targets:
//!
//! | `deliver_to` | `delivers_result()` | arm |
//! |---|---|---|
//! | none | either | [`without_targets`] — nothing to post to, no permission |
//! | some | true | [`with_targets`] — the harness posts the final text |
//! | some | false (`silent`) | [`with_targets_silent`] — the turn must send |
//!
//! The arms are mutually exclusive by construction: each one is reached from a
//! single branch of [`delivery_envelope`], so a non-silent job can never read
//! the silent text and a silent job can never read the auto-delivery text.

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

/// The envelope for a job that HAS at least one configured target but whose
/// harness auto-delivery is OFF — `deliver_mode = silent` (#1042).
///
/// #969 split send permission from auto-delivery, and this arm is where the
/// split becomes visible to the turn: the target list still earns the send
/// scope, so the turn may post, but `scheduler::auto_delivery_targets` is empty
/// for a silent job, so the harness posts nothing. Without this arm a silent
/// job read the auto envelope, obeyed "the scheduler already does it", and
/// discarded a correct message (measured 2026-11-27 on `nudge-check-linden-6c4a`:
/// a complete client nudge as final text, zero messages in the chat, run row
/// still `success`).
///
/// It is the mirror image of [`with_targets`] and states exactly three things:
/// nothing is posted for the turn, the turn HAS send permission to precisely
/// these targets and must use it, and its final text is discarded. The
/// owner-account warning stays because this arm actively invites a send, which
/// makes the self-echo more likely here than anywhere else.
///
/// The closing clause exists because the failure being repaired is a turn that
/// WITHHELD a composed message: "send nothing when there is nothing to say" is
/// correct on a quiet run, but a turn must never read it as licence to hold
/// back a message it wrote because the harness will post it.
fn with_targets_silent(job_name: &str, targets: &[&str]) -> String {
    format!(
        "{DELIVERY_ENVELOPE_MARKER}\n\
         Scheduled job: {job_name}\n\
         Delivery targets: {list}\n\
         Delivery mode: SILENT (the harness posts nothing for this job)\n\
         \n\
         This job is SILENT. Nothing is posted for you: when this turn ends the harness \
         delivers NOTHING to the target(s) above, and no send happens on your behalf.\n\
         \n\
         - You HAVE send permission for exactly the target(s) above, and you must use it. \
         Anything a human must read has to be posted with a send tool before the turn ends, \
         because nothing else you produce reaches anyone.\n\
         - Your final text is DISCARDED. It is kept only as the run record, so a report \
         written there is read by no one. Use `telegram_send`, `discord_send`, \
         `slack_send`, `whatsapp_send`, or any other tool that posts a message on your \
         behalf — some of those live in MCP servers rather than in this binary, so this list \
         cannot name them all. Send to the target(s) above and to no other chat: a different \
         chat is outside this job's configuration.\n\
         - Never post on the owner's own account. A message sent that way is ingested by the \
         chat as an INBOUND instruction from the owner, and that session will execute it as \
         an order.\n\
         - If this run produced nothing a human must read, sending nothing is the correct \
         outcome. Do not withhold a message you composed because the harness will post it — \
         it will not.",
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
///
/// `delivers_result` is `CronJob::delivers_result()` — false only for
/// `deliver_mode = "silent"`. It selects between the two arms that HAVE
/// targets, and it is the ONLY selector: the arms are mutually exclusive by
/// construction, because each is reached from exactly one branch here. A
/// silent job can never read the auto-delivery text and a non-silent job can
/// never read the silent text.
///
/// The targetless arm ignores `delivers_result`: a job with no `deliver_to`
/// posts nothing and may send nowhere in either mode, so there is nothing for
/// the mode to change.
pub fn delivery_envelope(
    job_name: &str,
    deliver_to: Option<&str>,
    delivers_result: bool,
) -> String {
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
        if delivers_result {
            with_targets(&name, &targets)
        } else {
            with_targets_silent(&name, &targets)
        }
    }
}

/// Prepend the envelope to a turn prompt.
///
/// The prompt itself is passed through byte-for-byte: the envelope is added
/// information, never a rewrite of the job's own instructions. Call this
/// exactly once per turn — the single call site in `scheduler::execute_job` is
/// pinned by `tests::cron_delivery_envelope_test`.
pub fn wrap_turn_prompt(
    prompt: &str,
    job_name: &str,
    deliver_to: Option<&str>,
    delivers_result: bool,
) -> String {
    let envelope = delivery_envelope(job_name, deliver_to, delivers_result);
    if prompt.trim().is_empty() {
        envelope
    } else {
        format!("{envelope}\n\n{prompt}")
    }
}
