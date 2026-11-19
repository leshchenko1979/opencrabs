//! Track A — episodic recall over the LIVE messages corpus (#890 step 5).
//!
//! The question this leg asks is the one the product actually faces: given a
//! query phrased the way a person phrases it, does the SHIPPED search surface
//! the message it came from?
//!
//! # Why the corpus is BOUNDED
//!
//! The shipped path (`session_search` → `MessageRepository::search_by_content`)
//! is an unscoped `LIKE` with no full-text index. Measured on the ops profile
//! at 44,667 messages, ONE such query takes ~75 s (#855), so 60 queries would
//! be 75 minutes. This leg therefore bounds the scan to the most recent
//! `slice_days` of messages and REPORTS the slice beside the number. The bound
//! changes the COST, never the SEMANTICS: the same `LIKE … ESCAPE '\' COLLATE
//! NOCASE` pattern and the same tokenized fallback run against the slice.
//!
//! # Why MRR is published as the zero it is
//!
//! `search_by_content` orders by `created_at DESC` — recency, not relevance —
//! and the tokenized fallback ranks by word-hit count then recency. There is
//! no ranker to reward, so a known-item query with more than one matching
//! message lands wherever recency puts it. #855 is the issue that would give
//! this leg a ranked message search; until it ships, MRR measures that
//! absence. It is not a quality regression to chase, and it must not be
//! reported as one.
//!
//! # Why the dataset is GENERATED, not committed
//!
//! Tracks B and C commit their datasets because their corpora are committed.
//! This track reads the LIVE store, so a committed dataset would pin message
//! ids that exist on exactly one host — a fixture that silently measures
//! nothing anywhere else. The sampler is deterministic instead: same slice,
//! same 40 positives and 20 negatives, no clock or randomness beyond the
//! slice window.

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::collections::HashSet;

/// Positives wanted per run. One per sampled message, spread across the slice.
pub const POSITIVES: usize = 40;
/// Negatives wanted per run: queries that must return nothing.
pub const NEGATIVES: usize = 20;
/// Tokens taken from a message to build its query.
const QUERY_TOKENS: usize = 3;

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
    let n: i64 = conn.query_row(sql, p, |r| r.get(0)).context("count query")?;
    Ok(n as usize)
}

/// Run the SHIPPED retrieval semantics over the slice.
///
/// Mirrors `MessageRepository::search_by_content` (contiguous `LIKE`, recency
/// order) and, when that returns nothing for a multi-word query, its
/// `search_by_content_tokenized` companion (#1626): every token must appear,
/// the pool is `5 * k` capped at 200, and rows are re-ranked by token-hit
/// count with recency breaking ties. The only difference from the shipped
/// path is the `created_at` bound, which is the documented cost bound.
pub fn search_slice(conn: &Connection, since: i64, query: &str, k: usize) -> Result<Vec<Ranked>> {
    let escaped = |s: &str| s.replace('%', "\\%").replace('_', "\\_");
    let pattern = format!("%{}%", escaped(query));
    let limit = k as i64;

    let contiguous = {
        let mut stmt = conn.prepare(
            "SELECT id, session_id FROM messages \
             WHERE created_at >= ?1 AND content LIKE ?2 ESCAPE '\\' COLLATE NOCASE \
             ORDER BY created_at DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![since, pattern, limit], |r| {
            Ok(Ranked {
                message_id: r.get(0)?,
                session_id: r.get(1)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    if !contiguous.is_empty() {
        return Ok(contiguous);
    }

    // Tokenized fallback, mirroring the shipped arm.
    let mut tokens: Vec<String> = query.split_whitespace().map(str::to_string).collect();
    tokens.sort();
    tokens.dedup();
    tokens.truncate(8);
    if tokens.len() < 2 {
        return Ok(Vec::new());
    }
    let fetch_cap = (k.saturating_mul(5)).clamp(1, 200) as i64;
    let patterns: Vec<String> = tokens.iter().map(|t| format!("%{}%", escaped(t))).collect();
    let like_arms = vec!["content LIKE ? ESCAPE '\\' COLLATE NOCASE"; patterns.len()].join(" AND ");
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
    bind.push(&fetch_cap);
    let pool: Vec<(Ranked, usize)> = stmt
        .query_map(bind.as_slice(), |r| {
            let content: String = r.get(2)?;
            let hits = tokens
                .iter()
                .filter(|t| content.to_lowercase().contains(t.as_str()))
                .count();
            Ok((
                Ranked {
                    message_id: r.get(0)?,
                    session_id: r.get(1)?,
                },
                hits,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    // Stable sort by hit count descending keeps SQL recency order inside ties,
    // exactly as the shipped re-rank does.
    let mut ranked = pool;
    ranked.sort_by(|a, b| b.1.cmp(&a.1));
    Ok(ranked.into_iter().take(k).map(|(r, _)| r).collect())
}

/// Build a known-item case from one message, or `None` when it carries too
/// few content words to phrase a query from.
fn case_from(content: &str, id: &str, session_id: &str) -> Option<EpisodicCase> {
    let mut tokens = content_tokens(content);
    // Longest first: length is the cheap, deterministic proxy for rarity, and
    // alphabetical order inside a length band keeps the query from being a
    // verbatim phrase lifted out of the message.
    tokens.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    if tokens.len() < QUERY_TOKENS {
        return None;
    }
    Some(EpisodicCase {
        query: tokens[..QUERY_TOKENS].join(" "),
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
        let rows = stmt.query_map(params![since], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
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
        if let Some(case) = case_from(content, id, session_id) {
            positives.push(case);
        }
    }
    anyhow::ensure!(
        positives.len() >= POSITIVES,
        "track A: sampled only {} usable cases from {} candidates",
        positives.len(),
        candidates.len()
    );

    // Negatives: word pairs that no message in the slice contains together.
    // Built from the same candidate pool so they are drawn from the corpus's
    // own vocabulary — a negative made of nonsense words would be trivially
    // silent and measure nothing.
    let pool: Vec<String> = candidates
        .iter()
        .flat_map(|(_, _, c)| content_tokens(c))
        .take(400)
        .collect();
    let mut negatives = Vec::with_capacity(NEGATIVES);
    let mut i = 0usize;
    while negatives.len() < NEGATIVES && i + 7 < pool.len() {
        let query = format!("{} {}", pool[i], pool[i + 7]);
        let pat_a = format!("%{}%", pool[i]);
        let pat_b = format!("%{}%", pool[i + 7]);
        let hits = count(
            conn,
            "SELECT COUNT(*) FROM messages WHERE created_at >= ?1 \
             AND content LIKE ?2 ESCAPE '\\' COLLATE NOCASE \
             AND content LIKE ?3 ESCAPE '\\' COLLATE NOCASE",
            &[&since, &pat_a, &pat_b],
        )?;
        if hits == 0 {
            negatives.push(query);
        }
        i += 1;
    }
    anyhow::ensure!(
        negatives.len() == NEGATIVES,
        "track A: found only {} co-occurrence-free pairs, wanted {}",
        negatives.len(),
        NEGATIVES
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

/// A store with `n` messages whose content words are unique per message, so
/// every sampled query is a known item with exactly one right answer (#890).
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
        // Eighteen unique long tokens, padded past the 160-character floor the
        // sampler applies, and nothing shared between messages.
        let content = format!(
            "zq{i}alpha zq{i}bravo zq{i}charlie zq{i}delta zq{i}echo \
             zq{i}foxtrot zq{i}golf zq{i}hotel zq{i}india zq{i}juliet \
             zq{i}kilo zq{i}lima zq{i}mike zq{i}november zq{i}oscar \
             zq{i}papa zq{i}quebec zq{i}romeo zq{i}sierra zq{i}tango"
        );
        conn.execute(
            "INSERT INTO messages (id, session_id, role, content, created_at) \
             VALUES (?1, ?2, 'user', ?3, ?4)",
            params![
                format!("m{i}"),
                format!("session-{}", i % 5),
                content,
                now - (n as i64 - i as i64)
            ],
        )
        .expect("insert");
    }
    conn
}

/// The COMMITTED Track A fixture leg (#890 step 6): 90 seeded messages, a
/// 14-day slice, 40 positives + 20 negatives, scored at `k`. Deterministic by
/// construction, so it is CI-reproducible — unlike the real-store leg, which
/// reads the host's own history and stays out of CI.
#[cfg(test)]
pub fn committed_report(k: usize) -> EpisodicReport {
    let conn = seeded_store(90);
    let dataset = sample(&conn, 14).expect("sample");
    score(&conn, &dataset, k).expect("score")
}
