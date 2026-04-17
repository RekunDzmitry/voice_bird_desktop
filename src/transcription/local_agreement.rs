use std::time::Duration;

use crate::transcription::{Segment, Token};

pub const TIMESTAMP_TOLERANCE_MS: i64 = 300;
pub const SENTENCE_GAP_MS: u64 = 500;

#[derive(Debug, Clone, Default)]
pub struct AgreementOutput {
    pub committed_segments: Vec<Segment>,
    pub tentative_text: String,
    pub new_committed_upto: Duration,
}

pub fn step(prev: &[Token], curr: &[Token], committed_upto: Duration) -> AgreementOutput {
    let prefix_len = longest_agreeing_prefix(prev, curr);
    let committed_tokens: Vec<Token> = curr[..prefix_len]
        .iter()
        .filter(|t| Duration::from_millis(t.t_end_ms) > committed_upto)
        .cloned()
        .collect();

    let new_upto = committed_tokens
        .last()
        .map(|t| Duration::from_millis(t.t_end_ms))
        .unwrap_or(committed_upto);

    let committed_segments = group_into_sentences(&committed_tokens);

    let tentative_tokens = &curr[prefix_len..];
    let tentative_text = tentative_tokens
        .iter()
        .map(|t| t.text.trim())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");

    AgreementOutput {
        committed_segments,
        tentative_text,
        new_committed_upto: new_upto,
    }
}

fn longest_agreeing_prefix(prev: &[Token], curr: &[Token]) -> usize {
    let mut n = 0;
    for (p, c) in prev.iter().zip(curr.iter()) {
        if tokens_agree(p, c) { n += 1; } else { break; }
    }
    n
}

fn tokens_agree(a: &Token, b: &Token) -> bool {
    let skew = (a.t_start_ms as i64 - b.t_start_ms as i64).abs();
    skew <= TIMESTAMP_TOLERANCE_MS && normalize(&a.text) == normalize(&b.text)
}

fn normalize(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

fn group_into_sentences(tokens: &[Token]) -> Vec<Segment> {
    if tokens.is_empty() { return Vec::new(); }
    let mut out = Vec::new();
    let mut group: Vec<Token> = Vec::new();

    for t in tokens {
        if let Some(prev) = group.last() {
            let gap = t.t_start_ms.saturating_sub(prev.t_end_ms);
            if gap >= SENTENCE_GAP_MS {
                out.push(make_segment(&group));
                group.clear();
            }
        }
        let ends_sentence = t.text.trim_end().ends_with(|c: char| matches!(c, '.'|'?'|'!'));
        group.push(t.clone());
        if ends_sentence {
            out.push(make_segment(&group));
            group.clear();
        }
    }
    if !group.is_empty() {
        out.push(make_segment(&group));
    }
    out
}

fn make_segment(tokens: &[Token]) -> Segment {
    let text = tokens
        .iter()
        .map(|t| t.text.trim())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    Segment {
        t_start: Duration::from_millis(tokens.first().unwrap().t_start_ms),
        t_end:   Duration::from_millis(tokens.last().unwrap().t_end_ms),
        text,
        tokens: tokens.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcription::Token;
    use std::time::Duration;

    fn tok(text: &str, t0: u64, t1: u64) -> Token {
        Token { text: text.into(), t_start_ms: t0, t_end_ms: t1 }
    }

    #[test]
    fn no_prev_produces_all_tentative() {
        let curr = vec![tok("hello", 0, 500), tok("world", 500, 1000)];
        let out = step(&[], &curr, Duration::from_millis(0));
        assert!(out.committed_segments.is_empty());
        assert_eq!(out.tentative_text, "hello world");
        assert_eq!(out.new_committed_upto, Duration::from_millis(0));
    }

    #[test]
    fn exact_match_commits_all() {
        let prev = vec![tok("hello", 0, 500), tok("world", 500, 1000)];
        let curr = prev.clone();
        let out = step(&prev, &curr, Duration::from_millis(0));
        assert_eq!(out.committed_segments.len(), 1);
        assert_eq!(out.committed_segments[0].text, "hello world");
        assert_eq!(out.new_committed_upto, Duration::from_millis(1000));
        assert_eq!(out.tentative_text, "");
    }

    #[test]
    fn partial_prefix_agreement_commits_prefix_only() {
        let prev = vec![tok("hello", 0, 500), tok("world", 500, 1000), tok("again", 1000, 1500)];
        let curr = vec![tok("hello", 0, 500), tok("world", 500, 1000), tok("friend", 1000, 1500)];
        let out = step(&prev, &curr, Duration::from_millis(0));
        assert_eq!(out.committed_segments[0].text, "hello world");
        assert_eq!(out.tentative_text, "friend");
    }

    #[test]
    fn normalizes_punctuation_and_case_when_matching() {
        let prev = vec![tok("Hello,", 0, 500), tok("World.", 500, 1000)];
        let curr = vec![tok("hello",  0, 500), tok("world",  500, 1000)];
        let out = step(&prev, &curr, Duration::from_millis(0));
        assert!(!out.committed_segments.is_empty(), "should commit via normalized match");
    }

    #[test]
    fn timestamp_skew_within_tolerance_matches() {
        let prev = vec![tok("hello", 0, 500)];
        let curr = vec![tok("hello", 100, 600)];  // 100ms skew, within 300
        let out = step(&prev, &curr, Duration::from_millis(0));
        assert!(!out.committed_segments.is_empty());
    }

    #[test]
    fn timestamp_skew_beyond_tolerance_does_not_match() {
        let prev = vec![tok("hello", 0, 500)];
        let curr = vec![tok("hello", 400, 900)]; // 400ms, over 300
        let out = step(&prev, &curr, Duration::from_millis(0));
        assert!(out.committed_segments.is_empty());
    }

    #[test]
    fn committed_upto_filter_skips_already_committed() {
        let prev = vec![tok("hello", 0, 500), tok("world", 500, 1000)];
        let curr = prev.clone();
        let out = step(&prev, &curr, Duration::from_millis(500));
        assert_eq!(out.committed_segments.len(), 1);
        assert_eq!(out.committed_segments[0].text, "world");
        assert_eq!(out.new_committed_upto, Duration::from_millis(1000));
    }

    #[test]
    fn sentence_split_on_period() {
        let prev = vec![
            tok("one",    0,   300),
            tok("two.",  300,  700),
            tok("three", 700, 1100),
        ];
        let curr = prev.clone();
        let out = step(&prev, &curr, Duration::from_millis(0));
        assert_eq!(out.committed_segments.len(), 2);
        assert_eq!(out.committed_segments[0].text, "one two.");
        assert_eq!(out.committed_segments[1].text, "three");
    }

    #[test]
    fn sentence_split_on_gap() {
        let prev = vec![
            tok("before",  0,    300),
            tok("pause", 300,   700),   // 900ms silence after
            tok("after", 1600, 2000),
        ];
        let curr = prev.clone();
        let out = step(&prev, &curr, Duration::from_millis(0));
        assert_eq!(out.committed_segments.len(), 2);
        assert_eq!(out.committed_segments[0].text, "before pause");
        assert_eq!(out.committed_segments[1].text, "after");
    }
}
