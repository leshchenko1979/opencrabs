//! Scope cron jobs so they may send only to their configured targets.
//!
//! A cron job runs with no channel origin: nothing binds its session to a
//! chat. The proactive send path resolves a destination from the tool input
//! first, and a job's turn can pick that input up from anywhere it reads,
//! including a recalled memory. On 2026-08-21 a memory note from two weeks
//! earlier recorded a chat id and thread id under the heading "CONTINUE THIS
//! TASK", and a job posted its report into that group — one it was never
//! configured for, whose members had asked nothing.
//!
//! The rule this enforces: a cron turn may send only to the target its job was
//! created with. With no target it sends to no channel at all, and its output
//! lives in its own session. Recalled text can shape what a report says; it can
//! never decide where the report goes.
//!
//! #148 extends this across all channel authorities (telegram, discord,
//! slack, whatsapp), matching the central resolver's authority grammar.

/// A permitted delivery destination for a cron turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermittedTarget {
    pub channel: &'static str,
    pub target_id: String,
}

tokio::task_local! {
    /// The permitted channel targets for this cron turn. `None` in the task
    /// local means not a cron turn (unscoped); an empty `Vec` means a cron
    /// turn whose job named no `deliver_to` (sends nowhere).
    static CRON_PERMITTED_TARGETS: Option<Vec<PermittedTarget>>;
}

/// Run `fut` with cron send scoping active, permitting only `targets`.
/// Task-local, so it covers every await inside the turn and never reaches
/// a sibling job on the scheduler.
pub async fn with_permitted_targets<F, T>(targets: Option<Vec<PermittedTarget>>, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    CRON_PERMITTED_TARGETS.scope(targets, fut).await
}

/// Compatibility wrapper for the Telegram-only single-chat form.
pub async fn with_send_target<F, T>(target: Option<i64>, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    let targets = match target {
        Some(chat) => Some(vec![PermittedTarget {
            channel: "telegram",
            target_id: chat.to_string(),
        }]),
        None => Some(Vec::new()),
    };
    with_permitted_targets(targets, fut).await
}

/// What this turn may do with a proactive send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendPermission {
    /// Not a cron turn. The ordinary channel rules apply.
    Unscoped,
    /// A cron turn with explicit permitted targets.
    Permitted(Vec<PermittedTarget>),
    /// A cron turn whose job named no target.
    Nowhere,
}

/// The permission in force for the current task.
pub fn permission() -> SendPermission {
    CRON_PERMITTED_TARGETS
        .try_with(|targets| match targets {
            Some(list) if list.is_empty() => SendPermission::Nowhere,
            Some(list) => SendPermission::Permitted(list.clone()),
            None => SendPermission::Unscoped,
        })
        .unwrap_or(SendPermission::Unscoped)
}

/// May the current task send to `(channel, target_id)`?
///
/// Outside a cron turn this is always true: the rule exists to stop a job
/// reaching destinations it was never given, not to police ordinary replies.
pub fn may_send(channel: &str, target_id: &str) -> bool {
    match permission() {
        SendPermission::Unscoped => true,
        SendPermission::Permitted(list) => list
            .iter()
            .any(|p| p.channel == channel && p.target_id == target_id),
        SendPermission::Nowhere => false,
    }
}

/// May the current task send to Telegram `chat_id`? (Compatibility helper).
pub fn may_send_to(chat_id: i64) -> bool {
    may_send("telegram", &chat_id.to_string())
}

/// Parse a job's configured `deliver_to` string into a list of permitted channel targets.
///
/// Returns `None` if `deliver_to` is `None` (unscoped / no delivery config).
/// Returns `Some(vec![])` if `deliver_to` is present but contains no channel targets
/// (e.g. empty, or targeting a session only).
pub fn parse_permitted_targets(deliver_to: Option<&str>) -> Option<Vec<PermittedTarget>> {
    deliver_to.map(|targets| {
        targets
            .split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .filter_map(|t| {
                if let Some(rest) = t.strip_prefix("telegram:") {
                    crate::cron::scheduler::parse_telegram_target(rest).map(|(chat_id, _)| {
                        PermittedTarget {
                            channel: "telegram",
                            target_id: chat_id.to_string(),
                        }
                    })
                } else if let Some(rest) = t.strip_prefix("discord:") {
                    Some(PermittedTarget {
                        channel: "discord",
                        target_id: rest.to_string(),
                    })
                } else if let Some(rest) = t.strip_prefix("slack:") {
                    Some(PermittedTarget {
                        channel: "slack",
                        target_id: rest.to_string(),
                    })
                } else {
                    t.strip_prefix("whatsapp:").map(|rest| PermittedTarget {
                        channel: "whatsapp",
                        target_id: rest.to_string(),
                    })
                }
            })
            .collect()
    })
}

/// The send scope a cron job runs under, derived from its own `deliver_to`.
///
/// This is the cron-entry-point shape of [`parse_permitted_targets`], and the
/// two differ on exactly one input. The parser answers "what did the job
/// configure?" — `None` there means the field is absent. A cron turn instead
/// asks "what may this job reach?", and a job that configured nothing may
/// reach nothing: an absent field maps to an empty scope, never to `None`
/// (Unscoped, "not a cron turn"), which would hand the turn every channel the
/// bot can reach (#317).
///
/// Callers that are not cron execution — a resumed ordinary session — want
/// `None` and must not come through here.
pub fn cron_job_scope(deliver_to: Option<&str>) -> Vec<PermittedTarget> {
    parse_permitted_targets(deliver_to).unwrap_or_default()
}

/// The channel authorities a cron send scope can name, as the `&'static str`
/// [`PermittedTarget`] stores.
///
/// The scope grammar is closed: a binding on any other channel (`cli`, `cron`,
/// `a2a`, …) has no channel destination to permit, so it contributes nothing.
/// Returning `None` for those keeps an unrecognised channel from silently
/// widening the scope.
fn channel_authority(channel: &str) -> Option<&'static str> {
    match channel {
        "telegram" => Some("telegram"),
        "discord" => Some("discord"),
        "slack" => Some("slack"),
        "whatsapp" => Some("whatsapp"),
        _ => None,
    }
}

/// Expand the `session:<id|prefix>` segments of a job's `deliver_to` into the
/// channel targets those sessions are BOUND to (#332, D1).
///
/// `parse_permitted_targets` recognises only the concrete channel prefixes, so
/// a job whose `deliver_to` is `session:<uuid>` produced an empty permitted
/// set. The turn was then scoped to Nowhere while the job HAD a destination —
/// the session's own bound chat — and every sibling send into that chat was
/// refused with a reason claiming the job declared no `deliver_to` at all.
///
/// This leg closes that gap: for each session segment, resolve the session and
/// read its OWN binding row, appending the bound channel target. Both halves
/// are shared code — [`resolve_job_session_target`] owns extraction, listing
/// and the archived policy; [`SessionBindingRepository::by_session`] owns the
/// binding read. This function owns neither, so the scope cannot drift from the
/// delivery path about where a report belongs.
///
/// Fails CLOSED at every step. An unresolvable target, a session with no
/// binding row, a binding on a channel outside the scope grammar, or a DB error
/// all contribute nothing — the turn keeps the targets it can prove, and a job
/// with nothing provable stays at Nowhere.
///
/// [`resolve_job_session_target`]: crate::cli::session_resolve::resolve_job_session_target
/// [`SessionBindingRepository::by_session`]: crate::db::repository::SessionBindingRepository::by_session
pub async fn expand_session_targets(
    pool: &crate::db::Pool,
    deliver_to: Option<&str>,
) -> Vec<PermittedTarget> {
    let Some(deliver_to) = deliver_to else {
        return Vec::new();
    };
    let bindings = crate::db::repository::SessionBindingRepository::new(pool.clone());
    let mut expanded = Vec::new();

    for segment in deliver_to
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if !crate::channels::target_resolver::is_session_target(segment) {
            continue;
        }
        let Some(session_id) =
            crate::cli::session_resolve::resolve_job_session_target(pool, segment).await
        else {
            continue;
        };
        let Ok(Some(binding)) = bindings.by_session(&session_id.to_string()).await else {
            continue;
        };
        let Some(channel) = channel_authority(&binding.channel) else {
            continue;
        };
        expanded.push(PermittedTarget {
            channel,
            target_id: binding.chat_id,
        });
    }

    expanded
}

/// The full permitted set for a job: its concrete channel targets plus the
/// channel targets its `session:` segments resolve to, deduplicated.
///
/// The two legs are complementary rather than overlapping in practice, but a
/// job may name both a channel and a session bound to that same channel — and
/// a duplicated entry reads as noise in [`refusal_for`]'s "may only send to
/// [...]" list. Order is preserved: concrete targets first, then expansions.
pub async fn cron_job_scope_async(
    pool: &crate::db::Pool,
    deliver_to: Option<&str>,
) -> Vec<PermittedTarget> {
    let mut targets = cron_job_scope(deliver_to);
    for extra in expand_session_targets(pool, deliver_to).await {
        if !targets.contains(&extra) {
            targets.push(extra);
        }
    }
    targets
}

/// Why a send was refused, for the tool result the model reads.
pub fn refusal_for(channel: &str, target_id: &str) -> String {
    match permission() {
        SendPermission::Permitted(list) => {
            let allowed_str = list
                .iter()
                .map(|p| format!("{}:{}", p.channel, p.target_id))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "Refused: this scheduled job may only send to [{allowed_str}], and this send \
                 targeted {channel}:{target_id}. If the report belongs in another channel, \
                 change the job's deliver_to; an address found in memory or in earlier context \
                 is not permission to post there."
            )
        }
        _ => format!(
            "Refused: this scheduled job has no deliver_to, so it may not send to any channel \
             (attempted {channel}:{target_id}). Its output stays in its own session. Set \
             deliver_to on the job if it should report to a channel."
        ),
    }
}

/// Extract the cron job UUID from a session title containing `[cron-job:<uuid>]`.
pub fn extract_cron_job_id_from_session_title(title: &str) -> Option<uuid::Uuid> {
    let start = title.find("[cron-job:")? + "[cron-job:".len();
    let rest = &title[start..];
    let end = rest.find(']')?;
    uuid::Uuid::parse_str(&rest[..end]).ok()
}

/// Resolve the permitted targets for a resumed session turn.
///
/// Returns `Some(targets)` (or `Some(vec![])` / Nowhere) if this session is
/// a cron session, ensuring it runs under the appropriate send_scope guard.
/// Returns `None` (Unscoped) if this is an ordinary non-cron session.
///
/// The targets are the job's full scope — concrete channel targets plus the
/// channel targets its `session:` segments expand to via their sessions'
/// bindings (#332) — so a resumed cron turn is scoped exactly like the firing
/// turn that created it.
pub async fn resolve_cron_session_scope(
    pool: &crate::db::Pool,
    session: Option<&crate::db::models::Session>,
    channel: &str,
) -> Option<Vec<PermittedTarget>> {
    let job_id = session
        .and_then(|s| s.title.as_deref())
        .and_then(extract_cron_job_id_from_session_title);

    if let Some(id) = job_id {
        let repo = crate::db::CronJobRepository::new(pool.clone());
        match repo.find_by_id(&id.to_string()).await {
            Ok(Some(job)) => Some(cron_job_scope_async(pool, job.deliver_to.as_deref()).await),
            Ok(None) | Err(_) => {
                // Cron job not found in DB or query error: fail closed to Nowhere
                Some(Vec::new())
            }
        }
    } else if channel == "cron" {
        // Channel is explicitly "cron" but no job id could be extracted: fail closed to Nowhere
        Some(Vec::new())
    } else {
        // Not a cron session
        None
    }
}

/// Compatibility refusal for Telegram chat_id.
pub fn refusal(chat_id: i64) -> String {
    refusal_for("telegram", &chat_id.to_string())
}
