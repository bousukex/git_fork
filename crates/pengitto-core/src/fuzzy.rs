//! Fuzzy matching for Quick Launch: the query's characters must appear in
//! order; matches at word starts, in runs, and early in the text score
//! higher. Case-insensitive.

/// A successful match: higher `score` is better; `positions` are char
/// indices of the matched characters (for highlighting).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub score: i32,
    pub positions: Vec<usize>,
}

const MATCH: i32 = 16;
const WORD_START: i32 = 24;
const CONSECUTIVE: i32 = 28;
/// The query matched a whole segment (`main` in `origin/main`).
const SEGMENT: i32 = 30;
const FIRST_CHAR: i32 = 12;
const GAP: i32 = -1;
const LEADING_GAP: i32 = -1;

fn is_separator(c: char) -> bool {
    matches!(c, '/' | '-' | '_' | '.' | ' ' | ':' | '\\')
}

/// Bonus for matching at position `i` of `chars`.
fn boundary_bonus(chars: &[char], i: usize) -> i32 {
    if i == 0 {
        return WORD_START + FIRST_CHAR;
    }
    let (prev, cur) = (chars[i - 1], chars[i]);
    if is_separator(prev) || (prev.is_lowercase() && cur.is_uppercase()) || (!prev.is_alphanumeric() && cur.is_alphanumeric()) {
        WORD_START
    } else {
        0
    }
}

fn lower(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

fn prepare_query(query: &str) -> Vec<char> {
    query.chars().filter(|c| !c.is_whitespace()).map(lower).collect()
}

/// A candidate text prepared once for matching many queries.
#[derive(Debug, Clone)]
pub struct Prepared {
    chars: Vec<char>,
    lower: Vec<char>,
    /// `lower` as bytes when the text is ASCII (fast rejection with memchr).
    ascii: Option<Vec<u8>>,
    /// Bonus for a match at each position (word starts).
    bonus: Vec<i32>,
}

impl Prepared {
    pub fn new(text: &str) -> Self {
        let chars: Vec<char> = text.chars().collect();
        let lower = chars.iter().copied().map(lower).collect();
        let bonus = (0..chars.len()).map(|i| boundary_bonus(&chars, i)).collect();
        let ascii = text.is_ascii().then(|| text.bytes().map(|b| b.to_ascii_lowercase()).collect());
        Prepared { chars, lower, ascii, bonus }
    }
}

const NONE: i32 = i32::MIN / 4;

/// Score of the best alignment, or `None`. With `trace`, also fills the
/// back-pointers needed for [`positions`] (`m * n` entries).
fn align(query: &[char], c: &Prepared, row: &mut Vec<i32>, next: &mut Vec<i32>, mut trace: Option<(&mut Vec<i32>, &mut Vec<usize>)>) -> Option<(i32, usize)> {
    let (m, n) = (query.len(), c.lower.len());
    if m == 0 {
        return Some((0, 0));
    }
    if !is_subsequence(query, c) {
        return None;
    }
    row.clear();
    row.extend(c.lower.iter().enumerate().map(|(j, &ch)| {
        if ch == query[0] { MATCH + c.bonus[j] + LEADING_GAP * j as i32 } else { NONE }
    }));
    if let Some((best, _)) = trace.as_mut() {
        best.clear();
        best.extend_from_slice(row);
    }
    for (i, &q) in query.iter().enumerate().skip(1) {
        next.clear();
        next.resize(n, NONE);
        // Max over k <= j-2 of row[k] - GAP * k (the gap cost is linear).
        let mut far: (i32, usize) = (NONE, usize::MAX);
        for j in 1..n {
            if j >= 2 {
                let k = j - 2;
                if row[k] > NONE && row[k] - GAP * k as i32 > far.0 {
                    far = (row[k] - GAP * k as i32, k);
                }
            }
            if c.lower[j] != q {
                continue;
            }
            let mut cand = (NONE, usize::MAX);
            if row[j - 1] > NONE {
                cand = (row[j - 1] + CONSECUTIVE, j - 1);
            }
            if far.0 > NONE && far.0 + GAP * (j as i32 - 1) > cand.0 {
                cand = (far.0 + GAP * (j as i32 - 1), far.1);
            }
            if cand.0 > NONE {
                next[j] = cand.0 + MATCH + c.bonus[j];
                if let Some((_, from)) = trace.as_mut() {
                    from[i * n + j] = cand.1;
                }
            }
        }
        std::mem::swap(row, next);
        if let Some((best, _)) = trace.as_mut() {
            best.extend_from_slice(row);
        }
    }
    row.iter().enumerate().filter(|(_, s)| **s > NONE).max_by_key(|(j, s)| (**s, std::cmp::Reverse(*j))).map(|(j, s)| (*s, j))
}

/// Quick reject: every query character must occur, in order.
fn is_subsequence(query: &[char], c: &Prepared) -> bool {
    if let Some(bytes) = &c.ascii {
        let mut at = 0;
        for &q in query {
            if !q.is_ascii() {
                return false;
            }
            match memchr::memchr(q as u8, &bytes[at..]) {
                Some(i) => at += i + 1,
                None => return false,
            }
        }
        return true;
    }
    let mut qi = 0;
    for &ch in &c.lower {
        if qi < query.len() && ch == query[qi] {
            qi += 1;
        }
    }
    qi == query.len()
}

/// Where the query occurs as a whole segment of the text (`main` in
/// `origin/main`): contiguous, starting at a word start, ending at a
/// separator or the end. A property of the text alone, so the score used
/// to pick the best candidates is the same as the final one.
fn whole_segment(query: &[char], c: &Prepared) -> Option<usize> {
    let m = query.len();
    if m == 0 || m > c.lower.len() {
        return None;
    }
    (0..=c.lower.len() - m).find(|&start| {
        c.bonus[start] > 0
            && c.lower[start..start + m] == *query
            && c.chars.get(start + m).is_none_or(|&ch| is_separator(ch))
    })
}

/// Score of a candidate: the best alignment plus the segment bonus.
fn total_score(query: &[char], c: &Prepared, row: &mut Vec<i32>, next: &mut Vec<i32>) -> Option<i32> {
    let (score, _) = align(query, c, row, next, None)?;
    Some(score + if whole_segment(query, c).is_some() { SEGMENT } else { 0 })
}

/// Full match with positions (for the few results that are shown).
fn match_prepared(query: &[char], c: &Prepared) -> Option<Match> {
    if query.is_empty() {
        return Some(Match { score: 0, positions: Vec::new() });
    }
    let (m, n) = (query.len(), c.lower.len());
    let (mut row, mut next, mut best) = (Vec::new(), Vec::new(), Vec::new());
    let mut from = vec![usize::MAX; m * n];
    let (score, end) = align(query, c, &mut row, &mut next, Some((&mut best, &mut from)))?;
    if let Some(start) = whole_segment(query, c) {
        // Highlight the segment that earned the bonus.
        return Some(Match { score: score + SEGMENT, positions: (start..start + m).collect() });
    }
    let mut positions = vec![0; m];
    let mut j = end;
    for i in (0..m).rev() {
        positions[i] = j;
        j = from[i * n + j];
    }
    Some(Match { score, positions })
}

/// Matches `query` against `text`; `None` if not all query characters occur
/// in order. An empty query matches everything with score 0.
pub fn fuzzy_match(query: &str, text: &str) -> Option<Match> {
    match_prepared(&prepare_query(query), &Prepared::new(text))
}

/// The `limit` best matches of `query` among prepared candidates, best
/// first; ties keep the original order. Scores every candidate cheaply and
/// works out highlight positions only for the results.
pub fn top_prepared(query: &str, items: &[Prepared], limit: usize) -> Vec<(usize, Match)> {
    let q = prepare_query(query);
    let score_range = |range: std::ops::Range<usize>| -> Vec<(i32, usize)> {
        let (mut row, mut next) = (Vec::new(), Vec::new());
        range
            .filter_map(|i| total_score(&q, &items[i], &mut row, &mut next).map(|s| (s, i)))
            .collect()
    };
    // Big lists (every file of a large repository) are split over threads.
    const PER_THREAD: usize = 8192;
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(items.len().div_ceil(PER_THREAD)).max(1);
    let mut found: Vec<(i32, usize)> = if threads == 1 {
        score_range(0..items.len())
    } else {
        let chunk = items.len().div_ceil(threads);
        std::thread::scope(|s| {
            let handles: Vec<_> = (0..threads)
                .map(|t| {
                    let range = t * chunk..((t + 1) * chunk).min(items.len());
                    s.spawn(|| score_range(range))
                })
                .collect();
            handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
        })
    };
    let key = |(s, i): &(i32, usize)| (std::cmp::Reverse(*s), *i);
    if found.len() > limit {
        found.select_nth_unstable_by_key(limit, key);
        found.truncate(limit);
    }
    let mut out: Vec<(usize, Match)> =
        found.into_iter().filter_map(|(_, i)| match_prepared(&q, &items[i]).map(|m| (i, m))).collect();
    out.sort_by_key(|(i, m)| (std::cmp::Reverse(m.score), *i));
    out
}

/// [`top_prepared`] for plain texts (prepares them on the fly).
pub fn top_matches<T>(query: &str, items: &[T], text: impl Fn(&T) -> &str, limit: usize) -> Vec<(usize, Match)> {
    let prepared: Vec<Prepared> = items.iter().map(|it| Prepared::new(text(it))).collect();
    top_prepared(query, &prepared, limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score(q: &str, t: &str) -> i32 {
        fuzzy_match(q, t).map_or(i32::MIN, |m| m.score)
    }

    #[test]
    fn subsequence_required() {
        assert!(fuzzy_match("abc", "a-b-c").is_some());
        assert!(fuzzy_match("acb", "abc").is_none());
        assert!(fuzzy_match("x", "").is_none());
        assert_eq!(fuzzy_match("", "anything").unwrap().score, 0);
    }

    #[test]
    fn case_insensitive_and_unicode() {
        assert!(fuzzy_match("MAIN", "feature/main").is_some());
        assert_eq!(fuzzy_match("日本", "feature/日本語").unwrap().positions, vec![8, 9]);
    }

    #[test]
    fn prefers_word_starts_runs_and_early_matches() {
        // Word starts beat scattered letters.
        assert!(score("fb", "feature/bar") > score("fb", "fooxbar"));
        // A run beats a split.
        assert!(score("main", "main") > score("main", "m-a-i-n"));
        assert!(score("main", "origin/main") > score("main", "my-awesome-index-node"));
        // Earlier is better, all else equal.
        assert!(score("ab", "ab-xxxx") > score("ab", "xxxx-ab"));
        // camelCase boundaries count as word starts.
        assert!(score("qL", "quickLaunch") > score("qL", "quicklaunch"));
    }

    #[test]
    fn positions_mark_the_best_alignment() {
        let m = fuzzy_match("rs", "src/repo/remote.rs").unwrap();
        assert_eq!(m.positions, vec![16, 17], "the run at the end beats r..s scattered");
    }

    #[test]
    fn picking_the_top_uses_the_final_score() {
        // Equal alignment scores, but only the second is a whole segment.
        let items = ["a/xb", "abzzz", "zz/ab"];
        let top = top_matches("ab", &items, |s| s, 1);
        assert_eq!(items[top[0].0], "zz/ab");
    }

    #[test]
    fn top_k_equals_full_sort() {
        // Pseudo-random texts over a small alphabet with separators.
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let alphabet: Vec<char> = "abcAB/-_.".chars().collect();
        let texts: Vec<String> = (0..2000)
            .map(|_| (0..(next() % 12 + 1)).map(|_| alphabet[(next() % alphabet.len() as u64) as usize]).collect())
            .collect();
        for query in ["a", "ab", "abc", "bA", "a/b", "cab"] {
            let mut full: Vec<(usize, i32)> =
                texts.iter().enumerate().filter_map(|(i, t)| fuzzy_match(query, t).map(|m| (i, m.score))).collect();
            full.sort_by_key(|(i, s)| (std::cmp::Reverse(*s), *i));
            for k in [1, 5, 50] {
                let top: Vec<(usize, i32)> = top_matches(query, &texts, |s| s, k).into_iter().map(|(i, m)| (i, m.score)).collect();
                assert_eq!(top, full.iter().copied().take(k).collect::<Vec<_>>(), "{query} top {k}");
            }
        }
    }

    #[test]
    fn top_matches_orders_and_limits() {
        let items = ["release/v2", "main", "origin/main", "maintenance", "xmxaxixn"];
        let top = top_matches("main", &items, |s| s, 3);
        let names: Vec<&str> = top.iter().map(|(i, _)| items[*i]).collect();
        assert_eq!(names, ["main", "origin/main", "maintenance"]);
    }
}
