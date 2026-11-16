//! Finds similar past work without embeddings: query terms from a text, and a rank by how rare
//! each matched term is. The store finds the rows of each term; this module only computes.

use crate::privacy::PLACEHOLDER_PREFIX;
use std::collections::{BTreeMap, BTreeSet};

/// The most terms one query uses.
pub const MAX_TERMS: usize = 24;

/// Korean particles of two syllables. They are tried before the one-syllable particles.
const LONG_PARTICLES: [&str; 6] = ["에서", "으로", "에게", "까지", "부터", "처럼"];
const SHORT_PARTICLES: [&str; 13] = [
    "은", "는", "이", "가", "을", "를", "에", "의", "도", "로", "와", "과", "만",
];

fn is_hangul(c: char) -> bool {
    ('\u{AC00}'..='\u{D7A3}').contains(&c)
}

fn all_hangul(word: &str) -> bool {
    word.chars().all(is_hangul)
}

/// The text without `[REDACTED:...]` placeholders. A placeholder is not a word of the text.
fn without_placeholders(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(PLACEHOLDER_PREFIX) {
        out.push_str(&rest[..start]);
        out.push(' ');
        rest = match rest[start..].find(']') {
            Some(end) => &rest[start + end + 1..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

/// Removes one particle from a Hangul word of three or more syllables.
fn without_particle(word: &str) -> &str {
    if word.chars().count() < 3 || !all_hangul(word) {
        return word;
    }
    LONG_PARTICLES
        .iter()
        .chain(SHORT_PARTICLES.iter())
        .find_map(|particle| word.strip_suffix(particle))
        .unwrap_or(word)
}

/// The search terms of a text, in the order of the text, without repeats, at most `MAX_TERMS`.
///
/// A term is a lower-case run of letters and digits. A Hangul word of three or more syllables loses
/// one particle at its end. A term must have three or more characters, or be two Hangul syllables:
/// the trigram index finds the first kind, and `LIKE` finds the second.
pub fn query_terms(text: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for word in without_placeholders(text).split(|c: char| !c.is_alphanumeric()) {
        let lower = word.to_lowercase();
        let term = without_particle(&lower);
        let length = term.chars().count();
        let usable = length >= 3 || (length == 2 && all_hangul(term));
        if usable && !terms.iter().any(|known| known == term) {
            terms.push(term.to_owned());
            if terms.len() == MAX_TERMS {
                break;
            }
        }
    }
    terms
}

/// How the store finds the rows of one term.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TermQuery {
    /// An FTS5 `MATCH` argument: the term as one quoted phrase.
    Match(String),
    /// A `LIKE` pattern. A trigram index cannot find a term of fewer than three characters.
    Like(String),
}

pub fn term_query(term: &str) -> TermQuery {
    if term.chars().count() >= 3 {
        TermQuery::Match(format!("\"{}\"", term.replace('"', "\"\"")))
    } else {
        TermQuery::Like(format!("%{term}%"))
    }
}

/// Ranks the rows that the terms found and returns the candidates, best first.
///
/// `total` is the number of rows searched. `hits` holds, for each term, the rows that match it.
/// A term is rare if at most `max(2, total / 4)` rows match it. Only a row that matches a rare term
/// is a candidate, so a common word alone finds nothing. The score of a row is the sum of
/// `ln(1 + total / df)` over the terms it matches, where `df` is the number of rows of the term.
/// Rows with the same score are ordered by `tie`, lowest first.
pub fn rank<K: Ord + Clone, T: Ord>(total: usize, hits: &[BTreeSet<K>], tie: impl Fn(&K) -> T) -> Vec<K> {
    let rare_limit = (total / 4).max(2);
    let mut scores: BTreeMap<K, (f64, bool)> = BTreeMap::new();
    for rows in hits.iter().filter(|rows| !rows.is_empty()) {
        let weight = (1.0 + total as f64 / rows.len() as f64).ln();
        let rare = rows.len() <= rare_limit;
        for row in rows {
            let entry = scores.entry(row.clone()).or_insert((0.0, false));
            entry.0 += weight;
            entry.1 |= rare;
        }
    }
    let mut candidates: Vec<(K, f64)> = scores
        .into_iter()
        .filter(|(_, (_, rare))| *rare)
        .map(|(row, (score, _))| (row, score))
        .collect();
    candidates.sort_by(|(a, a_score), (b, b_score)| b_score.total_cmp(a_score).then_with(|| tie(a).cmp(&tie(b))));
    candidates.into_iter().map(|(row, _)| row).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Reverse;

    fn terms(text: &str) -> Vec<String> {
        query_terms(text)
    }

    #[test]
    fn one_particle_is_removed_from_a_long_hangul_word() {
        assert_eq!(
            terms("배포를 금요일에 서울에서 했다"),
            ["배포", "금요일", "서울", "했다"]
        );
        // The two-syllable particle is tried first, and only one particle goes.
        assert_eq!(terms("회의실에서는"), ["회의실에서"]);
        assert_eq!(terms("일정으로"), ["일정"]);
        // A two-syllable word keeps its last syllable: it may be no particle.
        assert_eq!(terms("배포 회의"), ["배포", "회의"]);
        // A stem of one syllable is too short to search.
        assert!(terms("집에서").is_empty());
        // Only a Hangul word loses a particle. `slack은` is mixed and keeps it.
        assert_eq!(terms("slack은 deploy를"), ["slack은", "deploy를"]);
    }

    #[test]
    fn placeholders_short_ascii_words_and_repeats_are_left_out() {
        assert_eq!(
            terms("[REDACTED:EMAIL] 주소로 [REDACTED:employee id] 메일을 보내"),
            ["주소", "메일", "보내"]
        );
        assert!(!terms("mail [REDACTED:EMAIL]").contains(&"redacted".to_owned()));
        // An unclosed placeholder hides the rest of the text.
        assert_eq!(terms("deploy [REDACTED:EMAIL tail"), ["deploy"]);
        // Two ASCII characters are too short for the trigram index, and LIKE would find too much.
        assert_eq!(terms("QA on PR 12 for the API"), ["for", "the", "api"]);
        // One Hangul syllable, or two characters that are not both Hangul, are too short.
        assert!(terms("집 a가 가a").is_empty());
        // Terms are lower case and come once, in the order of the text.
        assert_eq!(terms("Deploy DEPLOY deploy-plan"), ["deploy", "plan"]);
    }

    #[test]
    fn at_most_the_first_24_terms_are_used() {
        let text: Vec<String> = (100..140).map(|n| format!("w{n}")).collect();
        let found = terms(&text.join(" "));
        assert_eq!(found.len(), MAX_TERMS);
        assert_eq!(found[0], "w100");
        assert_eq!(found[23], "w123");
    }

    #[test]
    fn a_long_term_is_a_quoted_phrase_and_a_short_one_a_like_pattern() {
        assert_eq!(term_query("deploy"), TermQuery::Match("\"deploy\"".into()));
        assert_eq!(term_query("금요일"), TermQuery::Match("\"금요일\"".into()));
        assert_eq!(term_query("a\"b"), TermQuery::Match("\"a\"\"b\"".into()));
        assert_eq!(term_query("배포"), TermQuery::Like("%배포%".into()));
    }

    fn set(rows: &[i64]) -> BTreeSet<i64> {
        rows.iter().copied().collect()
    }

    #[test]
    fn a_rarer_term_weighs_more() {
        // Ten rows. `deploy` matches rows 1 and 2, `friday` only row 3.
        let hits = [set(&[1, 2]), set(&[3])];
        assert_eq!(rank(10, &hits, |row: &i64| Reverse(*row)), [3, 2, 1]);
        // A row that matches both rare terms is first.
        let hits = [set(&[1, 2]), set(&[3, 2])];
        assert_eq!(rank(10, &hits, |row: &i64| Reverse(*row)), [2, 3, 1]);
    }

    #[test]
    fn a_common_term_alone_makes_no_candidate_but_adds_to_the_score() {
        // Eight rows: `the` is in five of them (more than 8 / 4), `deploy` in two.
        let common = set(&[1, 2, 3, 4, 5]);
        let hits = [common.clone(), set(&[4, 6])];
        assert_eq!(rank(8, &hits, |row: &i64| Reverse(*row)), [4, 6]);
        assert!(rank(8, &[common], |row: &i64| Reverse(*row)).is_empty());
        // With few rows, a term of two rows still counts as rare.
        assert_eq!(rank(3, &[set(&[1, 2])], |row: &i64| Reverse(*row)), [2, 1]);
        assert!(rank(3, &[set(&[1, 2, 3])], |row: &i64| Reverse(*row)).is_empty());
        assert!(rank(0, &[set(&[])], |row: &i64| Reverse(*row)).is_empty());
    }

    #[test]
    fn equal_scores_follow_the_tie_order() {
        let hits = [set(&[1, 2, 3])];
        // Same score for all three: the preferred row first, then the newest.
        let preferred_then_newest = |row: &i64| (*row != 1, Reverse(*row));
        assert_eq!(rank(20, &hits, preferred_then_newest), [1, 3, 2]);
        assert_eq!(rank(20, &hits, |row: &i64| Reverse(*row)), [3, 2, 1]);
        // The tie order never beats a higher score.
        let hits = [set(&[1, 2, 3]), set(&[2])];
        assert_eq!(rank(20, &hits, preferred_then_newest), [2, 1, 3]);
    }
}
