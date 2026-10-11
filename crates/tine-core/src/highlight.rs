//! Syntax highlighting: lines of code become spans of a few token kinds
//! (the colors come from the UI's palette, so switching between light and
//! dark needs no new highlighting).
//!
//! Spans are byte ranges of the text given — the text as displayed (tabs
//! expanded, invalid UTF-8 replaced), so they always line up with it.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};

use crate::CancelToken;

/// What a piece of code is, for coloring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Token {
    Comment,
    String,
    Keyword,
    Number,
    Type,
    Function,
    Constant,
    Punctuation,
}

/// `start..end` (bytes) of a line is a `token`; text outside spans is plain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: u32,
    pub end: u32,
    pub token: Token,
}

/// The syntax definitions bundled with syntect (loaded once, on first use;
/// a few tens of milliseconds, so off the UI thread).
pub fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// The language of a file, by name or extension, else by its first line
/// (`#!/bin/sh`, `<?xml`…). `None`: plain text.
pub fn syntax_for(path: &[u8], first_line: Option<&str>) -> Option<&'static SyntaxReference> {
    let set = syntaxes();
    let name = String::from_utf8_lossy(path.rsplit(|&b| b == b'/').next().unwrap_or(path)).into_owned();
    let by_name = set.find_syntax_by_extension(&name).or_else(|| {
        let ext = name.rsplit_once('.').map(|(_, e)| e)?;
        set.find_syntax_by_extension(ext).or_else(|| set.find_syntax_by_extension(&ext.to_ascii_lowercase()))
    });
    let found = by_name.or_else(|| first_line.and_then(|l| set.find_syntax_by_first_line(l)))?;
    (found.name != "Plain Text").then_some(found)
}

/// The token kind a scope stands for, if any (most specific first).
fn token_of(scope: Scope) -> Option<Token> {
    let name = scope.build_string();
    let starts = |p: &str| name == p || name.starts_with(&format!("{p}."));
    Some(if starts("comment") {
        Token::Comment
    } else if starts("string") || starts("markup.raw") || starts("markup.underline.link") {
        Token::String
    } else if starts("constant.numeric") {
        Token::Number
    } else if starts("constant") || starts("support.constant") || starts("variable.language") {
        Token::Constant
    } else if starts("entity.name.function") || starts("support.function") || starts("variable.function") {
        Token::Function
    } else if starts("entity.name.type")
        || starts("entity.name.class")
        || starts("entity.name.struct")
        || starts("entity.name.enum")
        || starts("entity.name.trait")
        || starts("entity.other.inherited-class")
        || starts("support.type")
        || starts("support.class")
    {
        Token::Type
    } else if starts("keyword") || starts("storage") || starts("markup.heading") || starts("entity.name.tag") {
        Token::Keyword
    } else if starts("punctuation") {
        Token::Punctuation
    } else {
        return None;
    })
}

/// Where a [`Highlighter`] is between two lines.
type State = (ParseState, ScopeStack, bool);

/// Highlights consecutive lines of one file (the parser carries state
/// from line to line, e.g. inside a block comment).
pub struct Highlighter {
    syntax: &'static SyntaxReference,
    state: ParseState,
    stack: ScopeStack,
    tokens: HashMap<Scope, Option<Token>>,
    /// The parser failed: plain text from here on.
    broken: bool,
}

impl Highlighter {
    pub fn new(syntax: &'static SyntaxReference) -> Self {
        Highlighter { syntax, state: ParseState::new(syntax), stack: ScopeStack::new(), tokens: HashMap::new(), broken: false }
    }

    /// The parser's state between lines (a failed parser stays failed).
    fn state(&self) -> State {
        (self.state.clone(), self.stack.clone(), self.broken)
    }

    fn set_state(&mut self, (state, stack, broken): State) {
        self.state = state;
        self.stack = stack;
        self.broken = broken;
    }

    /// Starts over, e.g. at a new diff hunk (its context is unknown).
    pub fn reset(&mut self) {
        self.state = ParseState::new(self.syntax);
        self.stack = ScopeStack::new();
        self.broken = false;
    }

    fn token_at(&mut self) -> Option<Token> {
        let mut innermost = None;
        for &scope in self.stack.as_slice().iter().rev() {
            let t = *self.tokens.entry(scope).or_insert_with(|| token_of(scope));
            // Inside a comment or string everything (quotes, `//`, escapes
            // aside) takes its color.
            if matches!(t, Some(Token::Comment | Token::String)) {
                return t;
            }
            innermost = innermost.or(t);
        }
        innermost
    }

    /// The spans of the next line (`text` without its line break).
    pub fn line(&mut self, text: &str) -> Vec<Span> {
        if self.broken {
            return Vec::new();
        }
        #[cfg(test)]
        if text.contains(tests::FAIL) {
            self.broken = true;
            return Vec::new();
        }
        let mut with_newline = String::with_capacity(text.len() + 1);
        with_newline.push_str(text);
        with_newline.push('\n');
        let ops = match self.state.parse_line(&with_newline, syntaxes()) {
            Ok(ops) => ops,
            Err(e) => {
                log::debug!("highlighting stopped: {e}");
                self.broken = true;
                return Vec::new();
            }
        };
        let mut spans: Vec<Span> = Vec::new();
        let mut push = |start: usize, end: usize, token: Option<Token>| {
            let end = end.min(text.len());
            let (Some(token), true) = (token, start < end) else { return };
            match spans.last_mut() {
                Some(last) if last.token == token && last.end as usize == start => last.end = end as u32,
                _ => spans.push(Span { start: start as u32, end: end as u32, token }),
            }
        };
        let mut at = 0;
        for (offset, op) in ops {
            if offset > at {
                let token = self.token_at();
                push(at, offset, token);
                at = offset;
            }
            if self.stack.apply(&op).is_err() {
                self.broken = true;
                return spans;
            }
        }
        let token = self.token_at();
        push(at, text.len(), token);
        spans
    }
}

/// Threads one highlighting uses at most.
pub const MAX_THREADS: usize = 8;

/// Long-lived highlighting threads. Fresh threads for every highlighting
/// would each build up their own regex caches, which then linger (on 24
/// cores, 100+ MiB after a few files instead of about 50). `None`: no
/// threads could be started; highlighting runs on the caller's thread.
fn pool() -> Option<&'static rayon::ThreadPool> {
    static POOL: OnceLock<Option<rayon::ThreadPool>> = OnceLock::new();
    POOL.get_or_init(|| {
        // More threads stop paying off around eight (measured).
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).clamp(1, MAX_THREADS);
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("tine-highlight-{i}"))
            .build()
            .map_err(|e| log::warn!("no highlighting threads: {e}"))
            .ok()
    })
    .as_ref()
}

/// Highlights independent streams of lines (each starting where a file
/// starts: a whole file, or one side of a diff hunk), using all cores.
///
/// Long streams are cut into chunks parsed in parallel from the initial
/// state. Joining them, a chunk whose real starting state differs (it
/// begins inside a block comment, say) is parsed again from the real state
/// until it reaches a state the parallel run also reached — from there on
/// the parallel result is the same. So the result is exactly that of one
/// sequential pass. `None` if cancelled, or once more than `max_spans`
/// spans come out (counted as parsed, so memory stays bounded).
pub fn highlight_streams(
    syntax: &'static SyntaxReference,
    streams: &[Vec<&str>],
    cancel: &CancelToken,
    max_spans: usize,
) -> Option<Vec<Vec<Vec<Span>>>> {
    let total: usize = streams.iter().map(Vec::len).sum();
    let threads = pool().map_or(1, rayon::ThreadPool::current_num_threads);
    // A couple of chunks per thread keep all of them busy to the end.
    let chunk = (total / (threads * 2)).max(256);
    // Work: (stream, first line, end line).
    let tasks: Vec<(usize, usize, usize)> = streams
        .iter()
        .enumerate()
        .flat_map(|(s, lines)| (0..lines.len()).step_by(chunk).map(move |a| (s, a, (a + chunk).min(lines.len()))))
        .collect();
    struct Done {
        spans: Vec<Vec<Span>>,
        /// State after each line (a wrongly started chunk can take many
        /// lines to meet the right state again; keeping all of them is
        /// cheaper than parsing whole chunks again).
        states: Vec<Arc<State>>,
        end: State,
    }
    let results: Vec<Mutex<Option<Done>>> = tasks.iter().map(|_| Mutex::new(None)).collect();
    let next = AtomicUsize::new(0);
    let (spent, over) = (AtomicUsize::new(0), AtomicBool::new(false));
    // Each worker takes chunks until none are left.
    let work = || {
        let mut h = Highlighter::new(syntax);
        loop {
            let i = next.fetch_add(1, Ordering::Relaxed);
            let Some(&(stream, a, b)) = tasks.get(i) else { return };
            h.reset();
            let mut spans = Vec::with_capacity(b - a);
            let mut states: Vec<Arc<State>> = Vec::with_capacity(b - a);
            for line in &streams[stream][a..b] {
                if cancel.is_cancelled() || over.load(Ordering::Relaxed) {
                    return;
                }
                let line = h.line(line);
                if spent.fetch_add(line.len(), Ordering::Relaxed) + line.len() > max_spans {
                    over.store(true, Ordering::Relaxed);
                    return;
                }
                spans.push(line);
                // Consecutive lines often end in the same state: share it
                // (one is kept for every line).
                let state = h.state();
                match states.last() {
                    Some(last) if **last == state => states.push(last.clone()),
                    _ => states.push(Arc::new(state)),
                }
            }
            let done = Done { spans, states, end: h.state() };
            *results[i].lock().unwrap_or_else(|e| e.into_inner()) = Some(done);
        }
    };
    match pool() {
        Some(pool) => pool.scope(|s| {
            for _ in 0..threads.min(tasks.len()) {
                s.spawn(|_| work());
            }
        }),
        None => work(),
    }
    if cancel.is_cancelled() || over.load(Ordering::Relaxed) {
        return None;
    }
    // Join each stream's chunks in order, fixing those that started wrong.
    let mut out: Vec<Vec<Vec<Span>>> = streams.iter().map(|l| Vec::with_capacity(l.len())).collect();
    let initial = Highlighter::new(syntax).state();
    let mut fixer = Highlighter::new(syntax);
    let mut real: Option<State> = None;
    let mut total = 0;
    for (i, &(stream, a, _)) in tasks.iter().enumerate() {
        let done = results[i].lock().unwrap_or_else(|e| e.into_inner()).take()?;
        if a == 0 {
            real = None; // a stream starts in the initial state, as assumed
        }
        let mut spans = done.spans;
        // Where the chunk ends: as the parallel run found, unless fixing
        // it never met that run again.
        let mut end_state = done.end;
        if let Some(state) = real.take().filter(|s| *s != initial) {
            fixer.set_state(state);
            let mut converged = false;
            for (j, line) in streams[stream][a..a + spans.len()].iter().enumerate() {
                #[cfg(test)]
                tests::fixer_line(cancel);
                if cancel.is_cancelled() {
                    return None;
                }
                spans[j] = fixer.line(line);
                if done.states.get(j).is_some_and(|s| fixer.state() == **s) {
                    converged = true; // from here the parallel run was right
                    break;
                }
            }
            if !converged {
                end_state = fixer.state();
            }
        }
        real = Some(end_state);
        total += spans.iter().map(Vec::len).sum::<usize>();
        if total > max_spans {
            return None;
        }
        out[stream].extend(spans);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    /// A line containing this makes the parser fail (as a broken grammar
    /// would).
    pub(super) const FAIL: &str = "\u{1}fail\u{1}";

    thread_local! {
        /// Lines the joining step may fix before the test cancels.
        static CANCEL_AFTER_FIXES: Cell<Option<usize>> = const { Cell::new(None) };
    }

    pub(super) fn fixer_line(cancel: &CancelToken) {
        CANCEL_AFTER_FIXES.with(|c| match c.get() {
            Some(0) => cancel.cancel(),
            Some(n) => c.set(Some(n - 1)),
            None => {}
        });
    }

    const NO_LIMIT: usize = usize::MAX;

    fn tokens(path: &str, lines: &[&str]) -> Vec<Vec<(String, Token)>> {
        let syntax = syntax_for(path.as_bytes(), lines.first().copied()).expect("a known language");
        let mut h = Highlighter::new(syntax);
        lines
            .iter()
            .map(|l| h.line(l).iter().map(|s| (l[s.start as usize..s.end as usize].to_owned(), s.token)).collect())
            .collect()
    }

    fn has(spans: &[(String, Token)], text: &str, token: Token) -> bool {
        spans.iter().any(|(t, k)| t.contains(text) && *k == token)
    }

    #[test]
    fn rust() {
        let t = tokens("src/main.rs", &["fn main() {", "    let x = 42; // answer", "    println!(\"hi\");", "}"]);
        assert!(has(&t[0], "fn", Token::Keyword), "{:?}", t[0]);
        assert!(has(&t[0], "main", Token::Function), "{:?}", t[0]);
        assert!(has(&t[1], "42", Token::Number), "{:?}", t[1]);
        assert!(has(&t[1], "// answer", Token::Comment), "{:?}", t[1]);
        assert!(has(&t[2], "\"hi\"", Token::String), "{:?}", t[2]);
    }

    #[test]
    fn python_and_state_across_lines() {
        let t = tokens("x.py", &["def f():", "    \"\"\"doc", "    still doc\"\"\"", "    return None"]);
        assert!(has(&t[0], "def", Token::Keyword), "{:?}", t[0]);
        // Python's grammar calls docstrings comments; either way the state
        // carried over from the line before.
        assert!(has(&t[2], "still doc", Token::String) || has(&t[2], "still doc", Token::Comment), "{:?}", t[2]);
        assert!(has(&t[3], "None", Token::Constant), "{:?}", t[3]);
    }

    #[test]
    fn javascript_and_markdown() {
        let js = tokens("app.js", &["const n = 'x';"]);
        assert!(has(&js[0], "const", Token::Keyword) && has(&js[0], "'x'", Token::String), "{js:?}");
        let md = tokens("README.md", &["# Title", "`code`"]);
        assert!(has(&md[0], "Title", Token::Keyword), "{md:?}");
        assert!(has(&md[1], "code", Token::String), "{md:?}");
    }

    #[test]
    fn languages_by_name_and_first_line() {
        assert!(syntax_for(b"Makefile", None).is_some());
        assert!(syntax_for(b"dir/script", Some("#!/bin/bash")).is_some(), "shebang");
        assert!(syntax_for(b"notes.txt", None).is_none(), "plain text");
        assert!(syntax_for(b"UPPER.RS", None).is_some(), "extension in another case");
    }

    #[test]
    fn parallel_chunks_equal_one_sequential_pass() {
        // A file long enough for several chunks, with block comments and
        // strings crossing many chunk borders.
        let mut lines: Vec<String> = Vec::new();
        for i in 0..3000 {
            lines.push(match i % 7 {
                0 => format!("/* block comment {i}"),
                1 | 2 => "   still inside the comment".into(),
                3 => "   end */ let s = \"a".into(),
                4 => "multi-line string\";".into(),
                _ => format!("fn f{i}(x: u32) -> u32 {{ x + {i} }}"),
            });
        }
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let syntax = syntax_for(b"a.rs", None).unwrap();
        let mut h = Highlighter::new(syntax);
        let sequential: Vec<Vec<Span>> = refs.iter().map(|l| h.line(l)).collect();
        let parallel = highlight_streams(syntax, &[refs.clone(), refs[..10].to_vec()], &CancelToken::default(), NO_LIMIT).unwrap();
        if let Some(i) = (0..sequential.len()).find(|&i| parallel[0][i] != sequential[i]) {
            panic!("line {i} ({:?}) differs: {:?} vs {:?}", refs[i], parallel[0][i], sequential[i]);
        }
        assert_eq!(parallel[0], sequential);
        assert_eq!(parallel[1], sequential[..10]);
        let cancel = CancelToken::default();
        cancel.cancel();
        assert!(highlight_streams(syntax, &[refs], &cancel, NO_LIMIT).is_none());
    }

    /// Lines in one block comment (`comment`) after plain code: chunks after
    /// the first start inside the comment, so joining fixes many lines.
    fn commented(code: usize, comment: usize) -> Vec<String> {
        let mut lines: Vec<String> = (0..code).map(|i| format!("let v{i} = {i};")).collect();
        lines.push("/* a long comment".into());
        lines.extend((0..comment).map(|i| format!("   comment line {i} with \"quotes\" and fn")));
        lines.push("*/ let end = 1;".into());
        lines
    }

    #[test]
    fn cancelling_while_joining_stops_there() {
        let lines = commented(10, 5000);
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let syntax = syntax_for(b"a.rs", None).unwrap();
        assert!(highlight_streams(syntax, std::slice::from_ref(&refs), &CancelToken::default(), NO_LIMIT).is_some());
        CANCEL_AFTER_FIXES.with(|c| c.set(Some(10)));
        let result = highlight_streams(syntax, &[refs], &CancelToken::default(), NO_LIMIT);
        CANCEL_AFTER_FIXES.with(|c| c.set(None));
        assert!(result.is_none(), "the joining step noticed");
    }

    #[test]
    fn too_many_spans_give_up() {
        let lines: Vec<String> = (0..3000).map(|i| format!("let v{i} = \"{i}\"; // n")).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let syntax = syntax_for(b"a.rs", None).unwrap();
        let all = highlight_streams(syntax, std::slice::from_ref(&refs), &CancelToken::default(), NO_LIMIT).unwrap();
        let n: usize = all[0].iter().map(Vec::len).sum();
        assert!(highlight_streams(syntax, std::slice::from_ref(&refs), &CancelToken::default(), n).is_some(), "exactly at the limit");
        assert!(highlight_streams(syntax, &[refs], &CancelToken::default(), n - 1).is_none());
    }

    #[test]
    fn a_failure_stays_failed_across_chunks() {
        // The parser fails early in the comment: the rest is plain, also in
        // chunks parsed in parallel from a fresh (unfailed) state.
        let mut lines = commented(5, 3000);
        lines[300] = format!("{} {FAIL}", lines[300]);
        lines.extend((0..500).map(|i| format!("fn after{i}() {{ \"s\" }}")));
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let syntax = syntax_for(b"a.rs", None).unwrap();
        let mut h = Highlighter::new(syntax);
        let sequential: Vec<Vec<Span>> = refs.iter().map(|l| h.line(l)).collect();
        assert!(sequential[301..].iter().all(Vec::is_empty));
        let parallel = highlight_streams(syntax, &[refs], &CancelToken::default(), NO_LIMIT).unwrap();
        assert_eq!(parallel[0], sequential);
    }

    #[test]
    fn spans_line_up_with_wide_and_replaced_characters() {
        let line = "let s = \"日本語\u{fffd}\"; // コメント";
        let t = tokens("a.rs", &[line]);
        assert!(has(&t[0], "\"日本語\u{fffd}\"", Token::String), "{t:?}");
        assert!(has(&t[0], "// コメント", Token::Comment), "{t:?}");
    }
}
