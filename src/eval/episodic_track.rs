//! Track A — episodic recall over the LIVE messages corpus (#890 step 5).
//!
//! The question this leg asks is the one the product actually faces: given a
//! query phrased the way a person phrases it, does the SHIPPED search surface
//! the message it came from?
//!
//! # Why the corpus is BOUNDED
//!
//! The shipped path (`session_search` →
//! `MessageRepository::search_by_content_union`) is an unscoped `LIKE` with no
//! full-text index. Measured on the ops profile
//! at 44,667 messages, ONE such query takes ~75 s (#855), so 60 queries would
//! be 75 minutes. This leg therefore bounds the scan to the most recent
//! `slice_days` of messages and REPORTS the slice beside the number. The bound
//! changes the COST, never the SEMANTICS: the same `LIKE … ESCAPE '\' COLLATE
//! NOCASE` phrase arm, the same tokenized AND arm and the same union ranker run
//! against the slice.
//!
//! # Why MRR is published as the zero it is
//!
//! Since #941 the shipped search runs BOTH arms — the query as one contiguous
//! phrase, and every query word somewhere — merges them into one pool and
//! ranks that pool by query-term proximity span, then word-hit count, then
//! recency. Proximity is the only signal with any discriminating power here
//! (#941); before it, a known-item query with more than one matching message
//! landed wherever recency put it. MRR still measures how much of that
//! ordering the ranker can recover.
//!
//! # Why the dataset is GENERATED, not committed
//!
//! Tracks B and C commit their datasets because their corpora are committed.
//! This track reads the LIVE store, so a committed dataset would pin message
//! ids that exist on exactly one host — a fixture that silently measures
//! nothing anywhere else. The sampler is deterministic instead: same slice,
//! same 40 positives and 20 negatives, no clock or randomness beyond the
//! slice window.
//!
//! # Why the committed fixture can FAIL (#899)
//!
//! The fixture leg used to publish recall 1.000 / MRR 1.000 / FPR 0.000, and
//! every one of those was a tautology rather than a measurement:
//!
//! * the seeded corpus gave each message its own private nonsense vocabulary,
//!   so a query built from three of its words could match that message and
//!   nothing else — recall could not fail;
//! * negatives were accepted only after a count proved ZERO co-occurrence, so
//!   silence was guaranteed before the case was even added — FPR could not
//!   fail.
//!
//! Both sides are now falsifiable. The corpus draws its words from a handful
//! of topical clusters, so messages INSIDE a cluster share most of their
//! vocabulary: a query selects a neighbourhood of a dozen messages, and the
//! shipped recency-ordered search has to be lucky to put the gold on top.
//! Negatives come in two classes — near-misses that share vocabulary with a
//! non-gold message (a do-nothing ranker fires on them) and genuine absences —
//! so FPR is measured in both directions instead of being pinned at zero.

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::collections::{HashMap, HashSet};

/// Positives wanted per run. One per sampled message, spread across the slice.
pub const POSITIVES: usize = 40;
/// Negatives wanted per run: half near-misses, half genuine absences.
pub const NEGATIVES: usize = 20;

/// Document-frequency ceiling for a word to be paired into an ABSENT negative.
/// Two words that each appear in more sampled messages than this almost always
/// meet somewhere in a large slice, which is how the absent class starved
/// (#926).
const RARE_DF_CEILING: usize = 3;

/// How many verified-present rare words the absent sampler keeps to pair from.
/// Bounds the presence probe to one LIKE per candidate word.
const RARE_POOL_CAP: usize = 150;

/// Upper bound on `cooccur` probes spent building the absent half, so a slice
/// too dense to yield an absence fails in seconds with a precise message
/// rather than grinding through a quadratic search.
const ABSENT_PROBE_BUDGET: usize = 1_500;

/// The fixture corpus's topical clusters: a topic word, ten subject nouns and
/// eight action words each. Every word is distinct and no word is a substring
/// of another, because the shipped search matches on `LIKE '%word%'` — an
/// overlapping pair would make two different words indistinguishable to it.
///
/// A message takes one subject and three actions from its cluster, so messages
/// in the same cluster share most of their words. That overlap is the whole
/// point: it is what makes a query answerable without being a lookup.
///
/// Test-only: the live leg reads a real store, so nothing in the shipped binary
/// needs this table. Ungated it would be `dead_code` in the non-test lib target,
/// which `-D warnings` turns into a build failure.
#[cfg(test)]
const CLUSTERS: &[(&str, &[&str], &[&str])] = &[
    (
        "rollout",
        &[
            "rollback",
            "canary",
            "staging",
            "pipeline",
            "release",
            "restart",
            "provision",
            "supervisor",
            "artifact",
            "bluegreen",
        ],
        &[
            "deploy",
            "healthcheck",
            "migration",
            "hotfix",
            "cutover",
            "downtime",
            "incident",
            "outage",
        ],
    ),
    (
        "billing",
        &[
            "invoice",
            "payment",
            "refund",
            "subscription",
            "chargeback",
            "receipt",
            "currency",
            "proration",
            "merchant",
            "settlement",
        ],
        &[
            "gateway",
            "payout",
            "dispute",
            "ledger",
            "taxrate",
            "discount",
            "dunning",
            "remittance",
        ],
    ),
    (
        "telegram",
        &[
            "message",
            "channel",
            "webhook",
            "broadcast",
            "sticker",
            "notification",
            "subscriber",
            "thread",
            "forward",
            "caption",
        ],
        &[
            "polling",
            "botapi",
            "attachment",
            "inline",
            "groupchat",
            "emoji",
            "contact",
            "voicenote",
        ],
    ),
    (
        "memory",
        &[
            "recall",
            "embedding",
            "summary",
            "context",
            "retrieval",
            "compaction",
            "semantic",
            "transcript",
            "indexing",
            "relevance",
        ],
        &[
            "archive", "vectors", "chunking", "pruning", "snippet", "glossary", "digest", "profile",
        ],
    ),
    (
        "workflow",
        &[
            "runner",
            "checkout",
            "compile",
            "coverage",
            "dispatch",
            "matrix",
            "caching",
            "linting",
            "tarball",
            "workspace",
        ],
        &[
            "clippy", "binaries", "gatepass", "badge", "failure", "retry", "timeout", "queue",
        ],
    ),
    (
        "server",
        &[
            "service",
            "systemd",
            "journal",
            "uptime",
            "backup",
            "cronjob",
            "certificate",
            "firewall",
            "monitoring",
            "container",
        ],
        &[
            "reboot", "sshkey", "kernel", "diskfull", "latency", "throttle", "snapshot", "replica",
        ],
    ),
    (
        "browser",
        &[
            "screenshot",
            "headless",
            "selector",
            "javascript",
            "scrolling",
            "viewport",
            "session",
            "cookie",
            "navigate",
            "rendering",
        ],
        &[
            "capture",
            "puppeteer",
            "chromium",
            "iframe",
            "websocket",
            "stall",
            "waitfor",
            "download",
        ],
    ),
    (
        "mailbox",
        &[
            "smtp",
            "greylist",
            "inbound",
            "enclosure",
            "spamtraps",
            "header",
            "bounce",
            "sending",
            "reply",
            "filterrule",
        ],
        &[
            "mailer",
            "postfix",
            "dkim",
            "spf",
            "blacklist",
            "throttling",
            "backlog",
            "autoresponder",
        ],
    ),
];

/// One known-item case: a query a person would type, and the message it came
/// from. `session_id` is carried so the leg can report the weaker-but-useful
/// question — did retrieval at least land in the right conversation?
#[derive(Debug, Clone)]
pub struct EpisodicCase {
    pub query: String,
    pub message_id: String,
    pub session_id: String,
}

/// The generated dataset plus the scope it was generated over.
#[derive(Debug, Clone)]
pub struct EpisodicDataset {
    pub positives: Vec<EpisodicCase>,
    pub negatives: Vec<String>,
    pub slice_days: u32,
    pub slice_messages: usize,
    pub corpus_messages: usize,
}

/// What one leg of retrieval returned, reduced to the two ids this track needs.
#[derive(Debug, Clone)]
pub struct Ranked {
    pub message_id: String,
    pub session_id: String,
}

/// Scored outcome. Every rate carries its own denominator beside it.
#[derive(Debug, Clone)]
pub struct EpisodicReport {
    pub k: usize,
    pub slice_days: u32,
    pub slice_messages: usize,
    pub corpus_messages: usize,
    pub positives: usize,
    pub negatives: usize,
    /// Positives whose source message appeared anywhere in the returned list.
    pub found_message: usize,
    /// Positives whose source SESSION appeared in the returned list.
    pub found_session: usize,
    /// Mean reciprocal rank over all positives; a miss contributes 0.
    pub mrr: f64,
    /// Negatives that returned at least one message.
    pub false_positives: usize,
}

impl EpisodicReport {
    pub fn recall_at_k(&self) -> f64 {
        if self.positives == 0 {
            return 0.0;
        }
        self.found_message as f64 / self.positives as f64
    }

    pub fn false_positive_rate(&self) -> f64 {
        if self.negatives == 0 {
            return 0.0;
        }
        self.false_positives as f64 / self.negatives as f64
    }

    /// Stable human-readable summary. The slice rides on the first line
    /// because a recall number without its corpus bound is not comparable to
    /// anything, including its own next run.
    pub fn render(&self) -> String {
        format!(
            "episodic@{k}: recall={r:.3} MRR={m:.3} source-message={fm}/{p} \
             source-session={fs}/{p} ({p} positive, {n} negative)\n\
             slice: last {d} days = {sm}/{cm} messages (unscoped LIKE is ~75 s/query, #855)\n\
             silence: {fp}/{n} negatives fired (FPR={fpr:.3})\n",
            k = self.k,
            r = self.recall_at_k(),
            m = self.mrr,
            fm = self.found_message,
            fs = self.found_session,
            p = self.positives,
            n = self.negatives,
            d = self.slice_days,
            sm = self.slice_messages,
            cm = self.corpus_messages,
            fp = self.false_positives,
            fpr = self.false_positive_rate(),
        )
    }
}

/// Words that carry no selection signal; excluded so a query is built from
/// content words rather than grammar.
const STOPWORDS: &[&str] = &[
    "about", "after", "again", "already", "because", "before", "being", "between", "could",
    "every", "first", "there", "these", "thing", "think", "those", "through", "under", "where",
    "which", "while", "would", "should", "still", "their", "them", "then", "they", "this", "what",
    "when", "with", "your", "have", "from", "that", "will", "just", "like", "make", "made",
];

/// Lowercase alphanumeric tokens of `len >= 5` that are not stopwords, in
/// first-appearance order, deduplicated.
fn content_tokens(text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for raw in text.split(|c: char| !c.is_alphanumeric()) {
        if raw.len() < 5 {
            continue;
        }
        let tok = raw.to_lowercase();
        if STOPWORDS.contains(&tok.as_str()) {
            continue;
        }
        if seen.insert(tok.clone()) {
            out.push(tok);
        }
    }
    out
}

/// Unix cutoff for `slice_days` ago. Computed here rather than in SQL so the
/// caller can report the same bound it queried with.
fn cutoff_unix(slice_days: u32) -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    now - i64::from(slice_days) * 86_400
}

fn count(conn: &Connection, sql: &str, p: &[&dyn rusqlite::ToSql]) -> Result<usize> {
    let n: i64 = conn
        .query_row(sql, p, |r| r.get(0))
        .context("count query")?;
    Ok(n as usize)
}

/// How many messages in the slice contain BOTH words. Zero is a genuine
/// absence; anything else means a do-nothing ranker fires on the pair.
fn cooccur(conn: &Connection, since: i64, a: &str, b: &str) -> Result<usize> {
    let pat_a = format!("%{a}%");
    let pat_b = format!("%{b}%");
    count(
        conn,
        "SELECT COUNT(*) FROM messages WHERE created_at >= ?1 \
         AND content LIKE ?2 ESCAPE '\\' COLLATE NOCASE \
         AND content LIKE ?3 ESCAPE '\\' COLLATE NOCASE",
        &[&since, &pat_a, &pat_b],
    )
}

/// How many messages in the slice contain `word` on its own. One `LIKE` rather
/// than [`cooccur`]'s two, because the absent sampler needs presence per word:
/// a word that appears nowhere makes silence trivial and measures nothing.
fn present(conn: &Connection, since: i64, word: &str) -> Result<usize> {
    let pat = format!("%{word}%");
    count(
        conn,
        "SELECT COUNT(*) FROM messages WHERE created_at >= ?1 \
         AND content LIKE ?2 ESCAPE '\\' COLLATE NOCASE",
        &[&since, &pat],
    )
}

/// Run the SHIPPED retrieval semantics over the slice.
///
/// Mirrors `MessageRepository::search_by_content_union` — which is the shipped
/// search since #941 — with the only difference being the `created_at` bound,
/// the documented cost bound. Both arms run ALWAYS: the contiguous `LIKE`
/// (the query as one phrase) and the tokenized AND (every query word
/// somewhere), each fetching
/// [`crate::relevance::episodic::tokenized_fetch_cap`] rows newest-first — the
/// SAME function the shipped arm calls, so a pool change cannot land in one
/// leg and not the other. The two sets are merged by
/// [`crate::relevance::episodic::merge_arms`] and ordered by
/// [`crate::relevance::episodic::rank`] — the very functions the shipped path
/// calls, so this leg cannot measure a ranker the binary does not run (#911).
///
/// The short-circuit that used to live here — return as soon as the phrase arm
/// came back non-empty — was the #941 defect, mirrored faithfully from
/// production: one unrelated row holding the phrase verbatim discarded the
/// whole tokenized candidate set. There is no branch on emptiness now, in
/// either leg.
pub fn search_slice(conn: &Connection, since: i64, query: &str, k: usize) -> Result<Vec<Ranked>> {
    let escaped = |s: &str| s.replace('%', "\\%").replace('_', "\\_");
    let tokens = crate::relevance::episodic::query_tokens(query);
    if tokens.is_empty() || k == 0 {
        return Ok(Vec::new());
    }
    let terms = crate::relevance::episodic::query_terms(query);
    let cap = crate::relevance::episodic::tokenized_fetch_cap(k);
    let phrase = format!("%{}%", escaped(query));
    let patterns: Vec<String> = tokens.iter().map(|t| format!("%{}%", escaped(t))).collect();

    // The phrase arm. `content` is carried through so the merged pool can be
    // scored with the same function the shipped arm scores with (#911).
    let contiguous = {
        let mut stmt = conn.prepare(
            "SELECT id, session_id, content FROM messages \
             WHERE created_at >= ?1 AND content LIKE ?2 ESCAPE '\\' COLLATE NOCASE \
             ORDER BY created_at DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![since, phrase, cap], |r| {
            Ok((
                Ranked {
                    message_id: r.get(0)?,
                    session_id: r.get(1)?,
                },
                r.get::<_, String>(2)?,
            ))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };

    // The tokenized arm. One token is the same substring as the phrase arm, so
    // the second scan is skipped — exactly as the shipped path does.
    let tokenized = if patterns.len() < 2 {
        Vec::new()
    } else {
        let like_arms =
            vec!["content LIKE ? ESCAPE '\\' COLLATE NOCASE"; patterns.len()].join(" AND ");
        let sql = format!(
            "SELECT id, session_id, content FROM messages \
             WHERE created_at >= ? AND {like_arms} \
             ORDER BY created_at DESC LIMIT ?"
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut bind: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(patterns.len() + 2);
        bind.push(&since);
        for p in &patterns {
            bind.push(p);
        }
        bind.push(&cap);
        let rows = stmt.query_map(bind.as_slice(), |r| {
            Ok((
                Ranked {
                    message_id: r.get(0)?,
                    session_id: r.get(1)?,
                },
                r.get::<_, String>(2)?,
            ))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };

    // ONE pool, ONE ranker: the shipped union rule, through the shipped code.
    let merged = crate::relevance::episodic::merge_arms(
        contiguous,
        tokenized,
        &terms,
        |pair: &(Ranked, String)| pair.0.message_id.clone(),
        |pair: &(Ranked, String)| pair.1.as_str(),
    );
    Ok(merged.into_iter().take(k).map(|c| c.row.0).collect())
}

/// Build a known-item case from one message, or `None` when it carries too
/// few content words to phrase a query from.
///
/// The query is shaped like a search-box query rather than a lifted phrase: one
/// SPECIFIC word first, then one BROAD area word. Two properties matter and both
/// come from `counts`:
///
/// * the specific word must be one the corpus SHARES (it occurs in at least two
///   messages). A word unique to this message would make the query a lookup —
///   exactly the tautology #899 removes;
/// * the broad word is the message's most widely used content word, so the query
///   names a topic as well as an incident.
///
/// A full-sentence question ("how did we fix the flaky deploy last week") is
/// deliberately NOT used here: the shipped arm ANDs every whitespace token, so a
/// sentence would return nothing and the leg would measure its own phrasing
/// instead of the search.
fn case_from(
    content: &str,
    id: &str,
    session_id: &str,
    counts: &HashMap<String, usize>,
) -> Option<EpisodicCase> {
    let tokens = content_tokens(content);
    if tokens.len() < 2 {
        return None;
    }

    // Narrowest shared word, first appearance breaking ties.
    let mut specific: Option<&String> = None;
    for tok in &tokens {
        let n = counts.get(tok).copied().unwrap_or(0);
        if n < 2 {
            continue;
        }
        match specific {
            None => specific = Some(tok),
            Some(cur) => {
                if n < counts.get(cur).copied().unwrap_or(0) {
                    specific = Some(tok);
                }
            }
        }
    }

    // Widest word in the message, first appearance breaking ties.
    let mut broad: Option<&String> = None;
    for tok in &tokens {
        let n = counts.get(tok).copied().unwrap_or(0);
        match broad {
            None => broad = Some(tok),
            Some(cur) => {
                if n > counts.get(cur).copied().unwrap_or(0) {
                    broad = Some(tok);
                }
            }
        }
    }

    let (specific, broad) = (specific?, broad?);
    if specific == broad {
        return None;
    }
    Some(EpisodicCase {
        query: format!("{specific} {broad}"),
        message_id: id.to_string(),
        session_id: session_id.to_string(),
    })
}

/// Sample the dataset from the live store.
///
/// Deterministic given the same slice: candidates are read in `created_at`
/// order and picked at a fixed stride, so two runs minutes apart on an
/// unchanged slice produce the same cases.
pub fn sample(conn: &Connection, slice_days: u32) -> Result<EpisodicDataset> {
    let since = cutoff_unix(slice_days);
    let corpus_messages = count(conn, "SELECT COUNT(*) FROM messages", &[])?;
    let slice_messages = count(
        conn,
        "SELECT COUNT(*) FROM messages WHERE created_at >= ?1",
        &[&since],
    )?;

    // Long messages carry enough content words to build a query that is not
    // one accidental substring.
    let candidates: Vec<(String, String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT id, session_id, content FROM messages \
             WHERE created_at >= ?1 AND length(content) >= 160 AND role IN ('user','assistant') \
             ORDER BY created_at ASC, id ASC",
        )?;
        let rows = stmt.query_map(params![since], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    anyhow::ensure!(
        candidates.len() >= POSITIVES,
        "track A: only {} messages in the last {} days carry enough content for {} cases — \
         widen --slice-days",
        candidates.len(),
        slice_days,
        POSITIVES
    );

    // How often each content word occurs across the candidate pool. A query
    // word that occurs ONCE names a single message — that is the tautology
    // #899 removes — so the specific half of every query is drawn from words
    // the corpus SHARES. Bounded so a live store of tens of thousands of
    // messages does not pay a full tokenisation pass.
    let mut counts: HashMap<String, usize> = HashMap::new();
    for (_, _, content) in candidates.iter().take(2000) {
        for tok in content_tokens(content) {
            *counts.entry(tok).or_insert(0) += 1;
        }
    }

    // Spread the picks across the slice, then fall back to a plain scan: a
    // message too short on content words is skipped rather than allowed to
    // shorten the dataset, and the fallback keeps that skip from quietly
    // yielding fewer than POSITIVES cases.
    let stride = (candidates.len() / POSITIVES).max(1);
    let mut order: Vec<usize> = (0..candidates.len()).step_by(stride).collect();
    order.extend(0..candidates.len());
    let mut positives: Vec<EpisodicCase> = Vec::with_capacity(POSITIVES);
    for idx in order {
        if positives.len() == POSITIVES {
            break;
        }
        let (id, session_id, content) = &candidates[idx];
        if positives.iter().any(|c| c.message_id == *id) {
            continue;
        }
        if let Some(case) = case_from(content, id, session_id, &counts) {
            positives.push(case);
        }
    }
    anyhow::ensure!(
        positives.len() >= POSITIVES,
        "track A: sampled only {} usable cases from {} candidates",
        positives.len(),
        candidates.len()
    );

    // Negatives come in TWO classes, and the split is the point (#899): a set
    // that can only be silent measures nothing.
    //
    //  * NEAR-MISS — a pair that DOES co-occur in some message, so a
    //    do-nothing ranker fires on it. This is the only way FPR can be above
    //    zero, and therefore the only way it can ever fail.
    //  * ABSENT — a pair that co-occurs nowhere in the slice. Silence here is a
    //    real result rather than a construction artefact.
    //
    // Both draw from the corpus's own vocabulary — a negative made of nonsense
    // words would be trivially silent and measure nothing.
    let pool: Vec<String> = candidates
        .iter()
        .flat_map(|(_, _, c)| content_tokens(c))
        .take(400)
        .collect();
    let half = NEGATIVES / 2;
    let mut negatives: Vec<String> = Vec::with_capacity(NEGATIVES);

    // Near-miss: consecutive words of one message share its vocabulary by
    // construction, so the shipped search returns rows for them.
    let mut i = 0usize;
    while negatives.len() < half && i + 1 < pool.len() {
        let (a, b) = (&pool[i], &pool[i + 1]);
        if a != b && !negatives.contains(&format!("{a} {b}")) && cooccur(conn, since, a, b)? > 0 {
            negatives.push(format!("{a} {b}"));
        }
        i += 1;
    }
    anyhow::ensure!(
        negatives.len() == half,
        "track A: found only {} near-miss negatives, wanted {}",
        negatives.len(),
        half
    );

    // Absent, attempt 1 — the frequency-ordered pool. This is what the leg has
    // always done, and it is CORRECT on a corpus with a small, uniform
    // vocabulary (the committed fixture): there, two pool tokens from different
    // topical clusters genuinely never meet.
    let mut j = 0usize;
    while negatives.len() < NEGATIVES && j + 7 < pool.len() {
        let (a, b) = (&pool[j], &pool[j + 7]);
        if a != b && !negatives.contains(&format!("{a} {b}")) && cooccur(conn, since, a, b)? == 0 {
            negatives.push(format!("{a} {b}"));
        }
        j += 1;
    }

    // Absent, attempt 2 — the RARE tail of the vocabulary (#926). Attempt 1
    // starves on a real store: `pool` is the first 400 tokens of the candidate
    // set, which on tens of thousands of messages is precisely the
    // high-frequency vocabulary, so any two of them meet somewhere in the slice
    // and no pair can be proved absent. Worse, widening `--slice-days` grows the
    // slice and makes the class HARDER, which inverted the old error message's
    // own advice to widen.
    //
    // Correctness still does not rest on the frequency estimate: `cooccur`
    // proves every pair absent over the whole slice. Rarity only decides where
    // to look, so a poor estimate costs probes, never a false negative — and if
    // the corpus is uniform (every token above the ceiling) this pass simply
    // finds nothing and attempt 1's result stands.
    //
    // Each word must ALSO occur in the slice on its own: a word that appears
    // nowhere makes silence trivial, which would measure the construction
    // rather than the retrieval — the same tautology #899 removed from the
    // positive half.
    let mut rare: Vec<(&String, usize)> = counts
        .iter()
        .filter(|(_, n)| **n <= RARE_DF_CEILING)
        .map(|(t, n)| (t, *n))
        .collect();
    // LEAST rare first within the band, and the token as a final tie-break.
    //
    // Ascending order would put df==1 tokens at the front, and on a real store
    // those are overwhelmingly unique machine identifiers — hex addresses, bare
    // numbers, uuids (measured on the host's own store: the first absent pairs a
    // df-ascending pass produced were all `25e83b5c`-style junk). Such a pair is
    // silent for a reason that has nothing to do with retrieval quality, so it
    // measures almost nothing. Preferring df==ceiling keeps the words real
    // vocabulary while still being provably absent.
    //
    // The token tie-break matters independently: `counts` is a HashMap, so
    // without a total order the dataset would vary run to run and the leg would
    // stop being reproducible.
    rare.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));

    let mut rare_present: Vec<&String> = Vec::new();
    for (tok, _) in rare {
        if rare_present.len() == RARE_POOL_CAP {
            break;
        }
        if present(conn, since, tok)? > 0 {
            rare_present.push(tok);
        }
    }

    let mut probes = 0usize;
    'absent: for (i, a) in rare_present.iter().enumerate() {
        for b in rare_present.iter().skip(i + 1) {
            if negatives.len() == NEGATIVES || probes == ABSENT_PROBE_BUDGET {
                break 'absent;
            }
            let (a, b) = (a.as_str(), b.as_str());
            let pair = format!("{a} {b}");
            if negatives.contains(&pair) {
                continue;
            }
            probes += 1;
            if cooccur(conn, since, a, b)? == 0 {
                negatives.push(pair);
                break;
            }
        }
    }

    anyhow::ensure!(
        negatives.len() == NEGATIVES,
        "track A: found only {} negatives, wanted {} — {} rare words available, {} probes spent. \
         The slice's vocabulary is too dense to build an absence, so NARROW --slice-days \
         (widening it makes this class harder, not easier)",
        negatives.len(),
        NEGATIVES,
        rare_present.len(),
        probes
    );

    Ok(EpisodicDataset {
        positives,
        negatives,
        slice_days,
        slice_messages,
        corpus_messages,
    })
}

/// Score a dataset through [`search_slice`].
pub fn score(conn: &Connection, dataset: &EpisodicDataset, k: usize) -> Result<EpisodicReport> {
    let since = cutoff_unix(dataset.slice_days);
    let mut found_message = 0usize;
    let mut found_session = 0usize;
    let mut rr = 0.0f64;

    for case in &dataset.positives {
        let ranked = search_slice(conn, since, &case.query, k)?;
        if let Some(pos) = ranked.iter().position(|r| r.message_id == case.message_id) {
            found_message += 1;
            rr += 1.0 / (pos as f64 + 1.0);
        }
        if ranked.iter().any(|r| r.session_id == case.session_id) {
            found_session += 1;
        }
    }

    let mut false_positives = 0usize;
    for query in &dataset.negatives {
        if !search_slice(conn, since, query, k)?.is_empty() {
            false_positives += 1;
        }
    }

    Ok(EpisodicReport {
        k,
        slice_days: dataset.slice_days,
        slice_messages: dataset.slice_messages,
        corpus_messages: dataset.corpus_messages,
        positives: dataset.positives.len(),
        negatives: dataset.negatives.len(),
        found_message,
        found_session,
        mrr: if dataset.positives.is_empty() {
            0.0
        } else {
            rr / dataset.positives.len() as f64
        },
        false_positives,
    })
}

/// A store with `n` messages drawn from the topical clusters in `CLUSTERS`.
///
/// Messages inside a cluster share most of their vocabulary — that overlap is
/// what makes the fixture able to FAIL (#899). A query built from one message's
/// words matches a dozen of its neighbours, so the shipped recency-ordered
/// search has to be lucky to rank the gold first, and it genuinely misses some.
/// Every message clears the 160-character floor the sampler applies.
///
/// Deterministic: message `i` takes its cluster from `i % CLUSTERS.len()` and
/// its words from pure index arithmetic inside that cluster. Only the
/// timestamps move, and they move together, so the relative order the search
/// ranks on is identical on every run.
///
/// Lives beside the track rather than in one test file because two consumers
/// seed it — the mechanism tests and the baseline guard — and a store built two
/// ways would make their numbers incomparable.
#[cfg(test)]
pub fn seeded_store(n: usize) -> Connection {
    let conn = Connection::open_in_memory().expect("in-memory store");
    conn.execute_batch(
        "CREATE TABLE messages (
            id TEXT PRIMARY KEY NOT NULL,
            session_id TEXT NOT NULL,
            role TEXT NOT NULL,
            content TEXT NOT NULL,
            created_at INTEGER NOT NULL
        );",
    )
    .expect("schema");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64;
    for i in 0..n {
        let (topic, subjects, actions) = CLUSTERS[i % CLUSTERS.len()];
        let j = i / CLUSTERS.len();
        let subject = subjects[j % subjects.len()];
        let slot = (j / subjects.len()) % actions.len();
        let a0 = actions[slot];
        let a1 = actions[(slot + 3) % actions.len()];
        let a2 = actions[(slot + 6) % actions.len()];
        let content = format!(
            "{topic} {subject}: {a0} and {a1} after {a2} was not on the list, \
             then {subject} {a0} {a1} {a2} again with the {topic} work, \
             and the {a0} {a1} {a2} {subject} note is fine"
        );
        conn.execute(
            "INSERT INTO messages (id, session_id, role, content, created_at) \
             VALUES (?1, ?2, 'user', ?3, ?4)",
            params![
                format!("m{i}"),
                format!("session-{}", i % 5),
                content,
                now - (n as i64 - i as i64) - 60
            ],
        )
        .expect("insert");
    }
    conn
}

/// The COMMITTED Track A fixture leg (#890 step 6): a 1,000-message seeded
/// corpus, a 14-day slice, 40 positives + 20 negatives, scored at `k`.
/// Deterministic by construction, so it is CI-reproducible — unlike the
/// real-store leg, which reads the host's own history and stays out of CI.
#[cfg(test)]
pub fn committed_report(k: usize) -> EpisodicReport {
    let conn = seeded_store(1000);
    let dataset = sample(&conn, 14).expect("sample");
    score(&conn, &dataset, k).expect("score")
}
