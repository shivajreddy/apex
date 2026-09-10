//! Small fzy-style fuzzy matcher shared by plugins.
//!
//! Scoring: dynamic programming over (query char, target char) with bonuses
//! for word starts, camelCase transitions, and consecutive runs; penalties
//! for gaps, leading offset, and target length.

const BONUS_WORD_START: i32 = 32;
const BONUS_CAMEL: i32 = 24;
const BONUS_CONSECUTIVE: i32 = 16;
const PENALTY_GAP: i32 = 1;
const PENALTY_LEAD_CAP: i32 = 8;
const PENALTY_LENGTH_CAP: i32 = 8;

const NEG: i32 = i32::MIN / 2;

/// Per-char word-start bonuses derived from the original-case string.
pub fn bonuses(original: &str) -> Vec<i32> {
    let chars: Vec<char> = original.chars().collect();
    let mut out = Vec::with_capacity(chars.len());
    for (i, &c) in chars.iter().enumerate() {
        let b = if i == 0 {
            BONUS_WORD_START
        } else {
            let prev = chars[i - 1];
            if matches!(prev, ' ' | '-' | '_' | '.' | '/' | '\\' | '(' | '[') {
                BONUS_WORD_START
            } else if prev.is_lowercase() && c.is_uppercase() {
                BONUS_CAMEL
            } else {
                0
            }
        };
        out.push(b);
    }
    out
}

/// Lowercase a string char-by-char, preserving char count so indices stay
/// aligned with [`bonuses`] of the original.
pub fn fold_case(original: &str) -> Vec<char> {
    original
        .chars()
        .map(|c| c.to_lowercase().next().unwrap_or(c))
        .collect()
}

/// Score `query` against `target` (both case-folded). `bonus` comes from
/// [`bonuses`] of the original target. `None` when query is not a
/// subsequence of target.
pub fn score(query: &[char], target: &[char], bonus: &[i32]) -> Option<i32> {
    let qn = query.len();
    let tn = target.len();
    if qn == 0 || qn > tn {
        return None;
    }

    // m[j]: best score with query[i] matched exactly at target[j]
    // g[j]: best score with query[i] matched at some k <= j, gap-decayed
    let mut m_prev = vec![NEG; tn];
    let mut g_prev = vec![NEG; tn];
    let mut m_cur = vec![NEG; tn];
    let mut g_cur = vec![NEG; tn];

    for j in 0..tn {
        if target[j] == query[0] {
            m_prev[j] = bonus[j] - (j as i32).min(PENALTY_LEAD_CAP);
        }
        g_prev[j] = if j == 0 {
            m_prev[0]
        } else {
            m_prev[j].max(g_prev[j - 1] - PENALTY_GAP)
        };
    }

    for i in 1..qn {
        for j in 0..tn {
            m_cur[j] = NEG;
            if target[j] == query[i] && j > 0 {
                let fresh = g_prev[j - 1].saturating_add(bonus[j]);
                let run = m_prev[j - 1].saturating_add(BONUS_CONSECUTIVE.max(bonus[j]));
                m_cur[j] = fresh.max(run);
            }
            g_cur[j] = if j == 0 {
                m_cur[0]
            } else {
                m_cur[j].max(g_cur[j - 1] - PENALTY_GAP)
            };
        }
        std::mem::swap(&mut m_prev, &mut m_cur);
        std::mem::swap(&mut g_prev, &mut g_cur);
    }

    let best = *m_prev.iter().max()?;
    if best <= NEG / 2 {
        return None;
    }
    let len_penalty = ((tn - qn) as i32 / 4).min(PENALTY_LENGTH_CAP);
    Some(best - len_penalty)
}

/// Convenience: score with case folding of both sides, no precomputed data.
#[cfg(test)]
fn score_str(query: &str, target: &str) -> Option<i32> {
    let q = fold_case(query);
    let t = fold_case(target);
    let b = bonuses(target);
    score(&q, &t, &b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_match_is_none() {
        assert_eq!(score_str("xyz", "Notepad"), None);
        assert_eq!(score_str("notepadd", "Notepad"), None);
        assert_eq!(score_str("", "Notepad"), None);
    }

    #[test]
    fn subsequence_matches() {
        assert!(score_str("np", "Notepad").is_some());
        assert!(score_str("notepad", "Notepad").is_some());
        assert!(score_str("vsc", "Visual Studio Code").is_some());
    }

    #[test]
    fn case_insensitive() {
        assert_eq!(score_str("NOTE", "Notepad"), score_str("note", "Notepad"));
    }

    #[test]
    fn prefix_beats_scattered() {
        let prefix = score_str("note", "Notepad").unwrap();
        let scattered = score_str("note", "OneNote").unwrap();
        assert!(prefix > scattered, "prefix {prefix} vs scattered {scattered}");
    }

    #[test]
    fn word_start_beats_middle() {
        let initials = score_str("vs", "Visual Studio").unwrap();
        let middle = score_str("vs", "Avast").unwrap();
        assert!(initials > middle, "initials {initials} vs middle {middle}");
    }

    #[test]
    fn shorter_target_wins_ties() {
        let short = score_str("term", "Terminal").unwrap();
        let long = score_str("term", "Terminal Preview Edition").unwrap();
        assert!(short > long, "short {short} vs long {long}");
    }

    #[test]
    fn initials_match_words() {
        assert!(score_str("gc", "Google Chrome").is_some());
        let initials = score_str("gc", "Google Chrome").unwrap();
        let plain = score_str("gc", "Logcat").unwrap();
        assert!(initials > plain, "initials {initials} vs plain {plain}");
    }
}
