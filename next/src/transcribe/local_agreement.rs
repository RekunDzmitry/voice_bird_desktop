use super::TranscriptOut;

const TIMESTAMP_TOLERANCE_MS: u64 = 300;
const SENTENCE_GAP_MS: u64 = 500;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Token {
    pub text: String,
    pub t_start_ms: u64,
    pub t_end_ms: u64,
}

pub(super) struct AgreementOutput {
    pub committed: Vec<TranscriptOut>,
    pub tentative: String,
    pub committed_upto: u64,
}

/// Compare only uncommitted tokens, accounting for timestamp drift in overlap.
pub(super) fn step(prev: &[Token], curr: &[Token], committed_upto: u64) -> AgreementOutput {
    let remaining = pending(prev, curr, committed_upto);
    let committed_upto = curr[..curr.len() - remaining.len()]
        .iter()
        .map(|token| token.t_end_ms)
        .max()
        .unwrap_or(committed_upto)
        .max(committed_upto);
    let curr = remaining;
    let prev = uncommitted(prev, committed_upto);
    let prefix = prev
        .iter()
        .zip(curr)
        .take_while(|(a, b)| agree(a, b))
        .count();
    let agreed = &curr[..prefix];
    AgreementOutput {
        committed: segments(agreed),
        tentative: text(&curr[prefix..]),
        committed_upto: agreed
            .iter()
            .map(|t| t.t_end_ms)
            .max()
            .unwrap_or(committed_upto)
            .max(committed_upto),
    }
}

pub(super) fn uncommitted(tokens: &[Token], watermark: u64) -> &[Token] {
    let first = tokens
        .iter()
        .position(|t| t.t_start_ms >= watermark)
        .unwrap_or(tokens.len());
    &tokens[first..]
}

/// End timestamps can shift past the watermark on the next inference. Match
/// retained committed tokens too, so a short word shifted right is not repeated.
pub(super) fn pending<'a>(prev: &[Token], curr: &'a [Token], watermark: u64) -> &'a [Token] {
    let first = curr
        .iter()
        .position(|token| {
            token.t_start_ms >= watermark
                && !prev
                    .iter()
                    .rev()
                    .take_while(|old| {
                        old.t_end_ms.saturating_add(TIMESTAMP_TOLERANCE_MS) >= token.t_start_ms
                    })
                    .any(|old| old.t_end_ms <= watermark && agree(old, token))
        })
        .unwrap_or(curr.len());
    &curr[first..]
}

fn agree(a: &Token, b: &Token) -> bool {
    a.t_start_ms.abs_diff(b.t_start_ms) <= TIMESTAMP_TOLERANCE_MS
        && normalize(&a.text).eq(normalize(&b.text))
}

fn normalize(s: &str) -> impl Iterator<Item = char> + '_ {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
}

/// Whisper tokens are subword pieces with their own leading spaces. Joining
/// trimmed tokens with spaces corrupts words ("trans" + "cription").
pub(super) fn text(tokens: &[Token]) -> String {
    tokens
        .iter()
        .map(|t| t.text.as_str())
        .collect::<String>()
        .trim()
        .to_owned()
}

pub(super) fn segments(tokens: &[Token]) -> Vec<TranscriptOut> {
    let mut out = Vec::new();
    let mut start = 0;
    for (index, token) in tokens.iter().enumerate() {
        if index > start
            && token.t_start_ms.saturating_sub(tokens[index - 1].t_end_ms) >= SENTENCE_GAP_MS
        {
            push_segment(&mut out, &tokens[start..index]);
            start = index;
        }
        if token.text.trim_end().ends_with(['.', '?', '!']) {
            push_segment(&mut out, &tokens[start..=index]);
            start = index + 1;
        }
    }
    push_segment(&mut out, &tokens[start..]);
    out
}

fn push_segment(out: &mut Vec<TranscriptOut>, tokens: &[Token]) {
    let Some(first) = tokens.first() else { return };
    let text = text(tokens);
    if !text.is_empty() {
        out.push(TranscriptOut::Committed {
            t_start_ms: first.t_start_ms,
            t_end_ms: tokens
                .iter()
                .map(|t| t.t_end_ms)
                .max()
                .unwrap()
                .max(first.t_start_ms),
            text,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn token(text: &str, start: u64, end: u64) -> Token {
        Token {
            text: text.into(),
            t_start_ms: start,
            t_end_ms: end,
        }
    }

    #[test]
    fn only_stable_prefix_commits_and_subwords_stay_intact() {
        let previous = [
            token(" trans", 0, 200),
            token("cription", 200, 400),
            token(" old", 400, 800),
        ];
        let current = [
            token(" trans", 0, 200),
            token("cription", 200, 400),
            token(" new", 400, 800),
        ];
        let first = step(&[], &current, 0);
        assert!(first.committed.is_empty());
        assert_eq!(first.tentative, "transcription new");
        let result = step(&previous, &current, 0);
        assert_eq!(
            result.committed,
            vec![TranscriptOut::Committed {
                t_start_ms: 0,
                t_end_ms: 400,
                text: "transcription".into(),
            }]
        );
        assert_eq!(result.tentative, "new");
        assert_eq!(result.committed_upto, 400);
    }

    #[test]
    fn punctuation_case_and_timestamp_tolerance() {
        let previous = [token(" Hello,", 100, 500)];
        let matching = [token(" hello", 400, 700)];
        assert_eq!(step(&previous, &matching, 0).committed_upto, 700);
        let outside = [token(" hello", 401, 701)];
        assert!(step(&previous, &outside, 0).committed.is_empty());
    }

    #[test]
    fn shifted_overlap_never_recommits_old_tokens_or_hides_new_tentative() {
        let previous = [token(" old", 0, 500), token(" new", 600, 900)];
        let current = [
            token(" old", 50, 550),
            token(" new", 650, 950),
            token(" tail", 1000, 1300),
        ];
        let result = step(&previous, &current, 500);
        assert_eq!(
            result.committed,
            vec![TranscriptOut::Committed {
                t_start_ms: 650,
                t_end_ms: 950,
                text: "new".into(),
            }]
        );
        assert_eq!(result.tentative, "tail");
        assert_eq!(step(&current, &current, 1300).tentative, "");
    }

    #[test]
    fn short_committed_token_shifted_beyond_watermark_is_not_repeated_on_finish() {
        let previous = [token(" word", 400, 500)];
        let current = [token(" word", 550, 650)];
        let output = step(&previous, &current, 500);
        assert!(output.committed.is_empty());
        assert_eq!(output.tentative, "");
        assert_eq!(output.committed_upto, 650);
        assert!(segments(uncommitted(&current, output.committed_upto)).is_empty());
    }

    #[test]
    fn sentence_and_gap_boundaries_keep_absolute_times() {
        let tokens = [
            token(" One.", 30000, 30500),
            token(" Two", 30500, 30800),
            token(" Three", 31300, 31800),
        ];
        assert_eq!(
            segments(&tokens),
            vec![
                TranscriptOut::Committed {
                    t_start_ms: 30000,
                    t_end_ms: 30500,
                    text: "One.".into()
                },
                TranscriptOut::Committed {
                    t_start_ms: 30500,
                    t_end_ms: 30800,
                    text: "Two".into()
                },
                TranscriptOut::Committed {
                    t_start_ms: 31300,
                    t_end_ms: 31800,
                    text: "Three".into()
                },
            ]
        );
    }
}
