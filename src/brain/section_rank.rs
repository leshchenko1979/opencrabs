//! BM25 ranking over the sections of a brain file (#996).
//!
//! Separate from `brain_sections`, which owns parsing: that module answers
//! "what are the sections", this one answers "which of them is this message
//! about". Automatic recall needs the second question answered well, because
//! it runs unprompted on every turn and anything it injects competes with the
//! actual task.
//!
//! Counting shared query terms could not do it. Measured against a real
//! 156-section MEMORY.md and 437 real user messages, the hit-count rule fired
//! on 89.5% of them, and every threshold on hit count traded noise against
//! genuine recall roughly one for one. The cause is that `the`, `and`, `you`
//! and `can` all clear the length filter, so two of them co-occurring is true
//! of nearly any section.
//!
//! BM25 fixes the weighting: a term present in most sections contributes
//! almost nothing. Dividing by query length fixes the rest, and it is the part
//! that mattered most. Without it a long message accumulates score from many
//! weak matches, so a fixed threshold behaves like no threshold at all. With
//! it the trade-off finally has a knee.
//!
//! Deliberately no embeddings. They are unavailable when vectors are disabled,
//! when the ~300 MB model download fails, and permanently on a CPU without AVX,
//! so making recall depend on them would give memory that works on one machine
//! and not another. FTS-style lexical ranking needs no model, no network and no
//! particular CPU, which is why the rest of the codebase falls back to it.

use std::collections::HashMap;

use crate::brain::brain_sections::{Matches, Section, query_terms, split_sections, tokens};

/// Term-frequency saturation. Above this, repeating a word stops helping.
const K1: f64 = 1.5;
/// Length normalization, how much a long section is discounted.
const B: f64 = 0.75;

/// Extra term count given to a token that appears in a section's HEADING.
///
/// A heading is the section's own name, so a query word landing in it is a
/// stronger signal than the same word buried in the body — this is the field
/// boost every ranked-retrieval system carries, implemented as extra `tf`
/// rather than a second scoring field so both halves keep sharing one
/// `tf`/`df`/length table.
///
/// Measured on the committed fixtures, at the floor shipped alongside it
/// (`RECALL_MIN_SCORE`), before -> after:
///
/// | path | figure | before | after |
/// |---|---|---|---|
/// | Track B, shipped injection window | recall@10 | 0.017 | 0.083 |
/// | Track B, ranker at depth 10 | recall@10 | 0.033 | 0.100 |
/// | memory pair, shipped window | recall@2 | 0.917 | 1.000 |
/// | memory pair, shipped window | FPR | 0.250 | 0.167 |
///
/// The memory pair is not a trade: both halves improve, because the boosted
/// heading pulls the correct section over the bar instead of leaving a
/// body-word accident to clear it.
const HEADING_BOOST: usize = 4;

/// Fold Latin diacritics so an unaccented query still matches accented text.
///
/// Matching was accent-sensitive, which fails quietly and often: `operacion`
/// did not match `operación`, `configuracao` did not match `configuração`,
/// `revision` did not match `révision`. People type without accents constantly,
/// so for Spanish, Portuguese and French this meant the discriminating word in
/// a question frequently matched nothing at all.
///
/// Combining marks are dropped ONLY when the base character is ASCII. That
/// restriction is the whole care of this function: under NFD the Cyrillic `й`
/// decomposes to `и` plus a breve, so folding indiscriminately would merge two
/// distinct Russian letters. Latin `é` decomposes to an ASCII `e` plus an
/// acute, which is exactly the case worth folding.
fn fold_diacritics(text: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    use unicode_normalization::char::is_combining_mark;

    let mut kept: Vec<char> = Vec::with_capacity(text.len());
    let mut base_was_ascii = false;
    for c in text.nfd() {
        if is_combining_mark(c) {
            // Keep the mark when it belongs to a non-ASCII base, so scripts
            // that carry meaning in their marks are left intact.
            if !base_was_ascii {
                kept.push(c);
            }
        } else {
            base_was_ascii = c.is_ascii_alphanumeric();
            kept.push(c);
        }
    }
    // Recompose, and this step is load-bearing rather than cosmetic. The
    // tokenizer splits on anything `is_alphanumeric` rejects, and a combining
    // mark is a nonspacing mark, not alphabetic. Left decomposed, every mark we
    // deliberately KEPT would be thrown away by tokenization anyway: Russian
    // `бой` would tokenize as `бои` and become indistinguishable from it,
    // which is the exact collision the filter above exists to prevent.
    kept.into_iter().nfc().collect()
}

/// Strip a common inflectional suffix, when a substantial stem remains.
///
/// Without this, `committing` does not match `commit` and `commands` does not
/// match `command`, which is most of what a natural question does to the words
/// it shares with an entry. Measured on the committed fixture, adding it lifts
/// recall from 0.750 to 0.917 at an unchanged false-positive rate.
///
/// Deliberately not a language-aware stemmer and deliberately not a wordlist.
/// The suffixes are English-shaped, so on another language this will sometimes
/// merge two words that are not related. That is tolerable here in a way a
/// hardcoded stopword list is not: it is applied symmetrically to the query and
/// to the sections, so a wrong merge shifts both sides equally and can only
/// blunt a score, never invent a match out of nothing. On a corpus containing
/// several languages the measured false-positive rate moved from 0.162 to
/// 0.174, which is the size of the risk in practice.
fn stem(word: &str) -> &str {
    for suffix in ["ing", "ed", "es", "s"] {
        if word.len() > suffix.len() + 3
            && let Some(stripped) = word.strip_suffix(suffix)
        {
            return stripped;
        }
    }
    word
}

/// Sections with the statistics BM25 needs, computed once.
///
/// Build cost is one pass over the content. Callers that score repeatedly
/// against an unchanged file (automatic recall does, on every turn) should
/// cache this rather than rebuild it.
pub struct Ranked {
    sections: Vec<Section>,
    /// Term counts per section.
    tf: Vec<HashMap<String, usize>>,
    /// How many sections contain each term.
    df: HashMap<String, usize>,
    /// Section lengths in tokens.
    lens: Vec<usize>,
    avg_len: f64,
    /// IDF of a term unique to one section, the scale scores are divided by.
    ///
    /// Without this the score scale tracks corpus size: a term appearing once
    /// is worth ~2.2 in a 13-section file and ~4.7 in a 156-section one, so a
    /// fixed threshold is effectively a different threshold per workspace. It
    /// showed up as a threshold that looked right on a mature MEMORY.md and
    /// recalled almost nothing on a small one.
    max_idf: f64,
}

impl Ranked {
    /// Split `content` on markdown headings and compute the ranking statistics.
    pub fn build(content: &str) -> Self {
        Self::from_sections(split_sections(content))
    }

    /// Rank over pieces that were already split by something else.
    ///
    /// Markdown headings are the right unit for a brain file, where a section
    /// is a rule. They are the wrong unit for content chunked for embedding,
    /// where the boundaries come from the chunker and both retrieval halves
    /// have to agree on what a unit is (#1000). Same scoring either way, so
    /// chunk-level lexical search inherits the stemming and diacritic folding
    /// rather than growing a second, subtly different implementation.
    pub fn from_sections(sections: Vec<Section>) -> Self {
        let mut tf = Vec::with_capacity(sections.len());
        let mut df: HashMap<String, usize> = HashMap::new();
        let mut lens = Vec::with_capacity(sections.len());

        for section in &sections {
            let mut counts: HashMap<String, usize> = HashMap::new();
            for token in tokens(&fold_diacritics(&section.body)) {
                *counts.entry(stem(&token).to_string()).or_insert(0) += 1;
            }
            // The heading is counted `1 + HEADING_BOOST` times, and the length
            // follows the counts, so a section is "longer" by exactly what the
            // boost adds. Keeping the two in step is what lets one `tf`/`df`/
            // length table serve both halves instead of a second scoring field.
            for token in tokens(&fold_diacritics(&section.heading)) {
                *counts.entry(stem(&token).to_string()).or_insert(0) += 1 + HEADING_BOOST;
            }
            let len: usize = counts.values().sum();
            for term in counts.keys() {
                *df.entry(term.clone()).or_insert(0) += 1;
            }
            tf.push(counts);
            lens.push(len);
        }

        let avg_len = if lens.is_empty() {
            0.0
        } else {
            lens.iter().sum::<usize>() as f64 / lens.len() as f64
        };

        let n = sections.len() as f64;
        let max_idf = (1.0 + (n - 1.0 + 0.5) / 1.5).ln();

        Self {
            sections,
            tf,
            df,
            lens,
            avg_len,
            max_idf,
        }
    }

    /// How many sections are indexed.
    pub fn len(&self) -> usize {
        self.sections.len()
    }

    /// The NAME of section `i`: its heading with the `#` markers stripped.
    ///
    /// The skill shortlist renders `/<name>`, so the stripping is part of what
    /// a section is called rather than a formatting detail of one caller.
    pub fn heading(&self, i: usize) -> String {
        self.sections[i]
            .heading
            .trim()
            .trim_start_matches('#')
            .trim()
            .to_string()
    }

    /// Whether the content produced no sections at all.
    pub fn is_empty(&self) -> bool {
        self.sections.is_empty()
    }

    /// Inverse document frequency: how much knowing a section contains this
    /// term narrows things down.
    fn idf(&self, term: &str) -> f64 {
        if self.max_idf <= 0.0 {
            return 0.0;
        }
        let n = self.sections.len() as f64;
        let df = *self.df.get(term).unwrap_or(&0) as f64;
        (1.0 + (n - df + 0.5) / (df + 0.5)).ln() / self.max_idf
    }

    /// BM25 score of section `i` for `terms`.
    fn score(&self, terms: &[String], i: usize) -> f64 {
        if self.avg_len == 0.0 {
            return 0.0;
        }
        let dl = self.lens[i] as f64;
        terms
            .iter()
            .filter_map(|term| {
                // Both tables are keyed by STEM. Looking up idf with the raw
                // term would miss every time, scoring df=0, which reads as
                // "unique to one section" and hands every word top weight.
                let folded = fold_diacritics(term);
                let stemmed = stem(&folded);
                let f = *self.tf[i].get(stemmed)? as f64;
                let denom = f + K1 * (1.0 - B + B * dl / self.avg_len);
                Some(self.idf(stemmed) * (f * (K1 + 1.0)) / denom)
            })
            .sum()
    }

    /// Section indices whose length-normalized BM25 score clears `min_score`, best
    /// first, bounded by section count and total characters.
    pub fn find_relevant_indices(
        &self,
        query: &str,
        max_sections: usize,
        max_chars: usize,
        min_score: f64,
    ) -> Vec<usize> {
        self.ranked_indices(query, max_sections, max_chars, min_score)
            .0
    }

    /// [`Self::find_relevant_indices`] plus how many sections cleared
    /// `min_score` BEFORE the count and character caps — the number a recall
    /// summary's `+K` marker names (#928).
    ///
    /// Both come out of ONE scoring pass, so the count can never describe a
    /// ranking other than the one whose indices it travels with: a separate
    /// recount would be a second BM25 pass that is free to disagree.
    pub fn ranked_indices(
        &self,
        query: &str,
        max_sections: usize,
        max_chars: usize,
        min_score: f64,
    ) -> (Vec<usize>, usize) {
        let terms = query_terms(query);
        if terms.is_empty() {
            return (Vec::new(), 0);
        }
        let norm = terms.len() as f64;

        let mut scored: Vec<(f64, usize)> = (0..self.sections.len())
            .filter_map(|i| {
                let score = self.score(&terms, i) / norm;
                (score >= min_score).then_some((score, i))
            })
            .collect();

        // Best first; file order breaks ties so repeated calls agree.
        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.1.cmp(&b.1))
        });

        let total = scored.len();
        let mut indices = Vec::new();
        let mut chars = 0usize;
        for (_, i) in scored {
            if indices.len() >= max_sections {
                break;
            }
            let section = &self.sections[i];
            let len = section.render().chars().count();
            if chars + len > max_chars && !indices.is_empty() {
                break;
            }
            chars += len;
            indices.push(i);
        }

        (indices, total)
    }

    /// Whether any section's RAW BM25 score clears `min_score`.
    ///
    /// Raw means the length normalization [`Self::find_relevant`] applies is
    /// left off, so a shared term counts at full weight however long the
    /// query is. That normalization is what makes a fixed `min_score` mean
    /// the same thing for a three-word question and a forty-word one — and
    /// also what makes a forty-word message touching eight topics score
    /// every entry it genuinely names at a fraction of a focused question's
    /// strength. Callers that must not go silent on such messages use this
    /// door beside the normalized one.
    pub fn any_clears_raw(&self, query: &str, min_score: f64) -> bool {
        let terms = query_terms(query);
        if terms.is_empty() {
            return false;
        }
        (0..self.sections.len()).any(|i| self.score(&terms, i) >= min_score)
    }

    /// Indices a SELECTION rule admits, best first: the entry must agree on at
    /// least `min_terms` distinct query terms, those terms must carry at least
    /// `min_summed_idf` between them, and the entry must score at least
    /// `ratio` of the best admitted entry (#939).
    ///
    /// [`Self::ranked_indices`] at floor `0.0` answers a different question —
    /// "in what order" — and admits every section sharing one stemmed term. A
    /// caller that reads its output as a SHORTLIST therefore lists the top N
    /// sections that share a word, which on a skill index with generic
    /// descriptions means a full list on almost every turn. Three predicates
    /// fix that, each aimed at one way the list fills with noise:
    ///
    /// - `min_terms` — one incidental word is not a match. It takes two terms
    ///   agreeing on the same section.
    /// - `min_term_idf` — WHICH terms are allowed to count toward the two
    ///   above. Presence alone is not evidence (#953): `the`, `and`, `you`,
    ///   `file` and `skill` sit in enough descriptions to be matched by
    ///   accident, and two of them satisfied `min_terms` on almost every turn,
    ///   which is how the same long descriptions kept winning. A term has to
    ///   be specific to a handful of entries before it counts at all, so
    ///   filler can no longer pad the count. Measured on the live 39-entry
    ///   index, `0.7` admits a term carried by at most three descriptions.
    /// - `min_summed_idf` — the same requirement measured by WEIGHT instead of
    ///   count, and the reason no stopword list is needed: on a skill index
    ///   `the` carries an idf near 0.2, so a section matching only filler
    ///   words cannot reach the sum, while two content words clear it. A
    ///   hardcoded wordlist would have to be maintained per language; an idf
    ///   sum is measured off the corpus the query is ranked against.
    /// - `ratio` — the tail. Scores are not comparable across queries, so no
    ///   fixed floor can separate "the two entries this message is about" from
    ///   "the five that share a word"; a share of the best admitted score is
    ///   query-relative by construction, and with `ratio <= 1.0` the best
    ///   admitted entry always survives.
    ///
    /// The reference for `ratio` is the best entry that passes the two content
    /// predicates, never the best raw score, so a section with a high score but
    /// no term agreement cannot empty a list that had a genuine match.
    ///
    /// Order is score order with file order breaking ties — the same order
    /// [`Self::ranked_indices`] produces.
    pub fn admitted_indices(
        &self,
        query: &str,
        max_sections: usize,
        min_terms: usize,
        min_term_idf: f64,
        min_summed_idf: f64,
        ratio: f64,
    ) -> Vec<usize> {
        let terms = query_terms(query);
        if terms.is_empty() || self.sections.is_empty() {
            return Vec::new();
        }
        let norm = terms.len() as f64;

        // Distinct STEMS. `query_terms` keeps duplicates, and counting
        // agreement with them would let one repeated filler word ("the ... the
        // ... the") read as three matching terms.
        let mut distinct: Vec<String> = Vec::new();
        for term in &terms {
            let stemmed = stem(&fold_diacritics(term)).to_string();
            if !distinct.contains(&stemmed) {
                distinct.push(stemmed);
            }
        }

        let mut admitted: Vec<(f64, usize)> = Vec::new();
        for i in 0..self.sections.len() {
            // Count CONTENT agreement only (#953). A term carried by much of
            // the index is matched by accident, so it cannot stand in for the
            // second term `min_terms` asks for — that substitution is what let
            // `you` + `file` admit a skill about neither.
            let mut content_terms = 0usize;
            let mut summed_idf = 0.0_f64;
            for term in &distinct {
                if !self.tf[i].contains_key(term) {
                    continue;
                }
                let idf = self.idf(term.as_str());
                if idf >= min_term_idf {
                    content_terms += 1;
                    summed_idf += idf;
                }
            }
            if content_terms < min_terms || summed_idf < min_summed_idf {
                continue;
            }
            admitted.push((self.score(&terms, i) / norm, i));
        }
        if admitted.is_empty() {
            return Vec::new();
        }

        admitted.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.1.cmp(&b.1))
        });

        let best = admitted[0].0;
        admitted
            .into_iter()
            .filter(|(score, _)| *score >= ratio * best)
            .take(max_sections)
            .map(|(_, i)| i)
            .collect()
    }

    /// Sections whose length-normalized BM25 score clears `min_score`, best
    /// first, bounded by section count and total characters.
    ///
    /// Normalizing by query length is what makes `min_score` mean the same
    /// thing for a three-word question and a forty-word one.
    pub fn find_relevant(
        &self,
        query: &str,
        max_sections: usize,
        max_chars: usize,
        min_score: f64,
    ) -> Matches {
        let terms = query_terms(query);
        if terms.is_empty() {
            return Matches {
                sections: Vec::new(),
                omitted: 0,
            };
        }
        let (indices, total) = self.ranked_indices(query, max_sections, max_chars, min_score);
        let sections = indices
            .into_iter()
            .map(|i| self.sections[i].clone())
            .collect::<Vec<_>>();

        let omitted = total.saturating_sub(sections.len());
        Matches { sections, omitted }
    }
}
