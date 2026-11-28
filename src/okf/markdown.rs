//! Markdown structure the catalog tools need: frontmatter/body splits, ATX
//! headings and their GitHub anchor slugs, HTML anchors, footnotes, and
//! links with their resolution to bundle paths.
//!
//! Port of the corresponding helpers in `tools/okf.py`. Everything uses the
//! linear-time `regex` crate; the two places where Python's patterns use
//! lookaround (`LINK_RE`'s `(?<!!)` and `slugify`'s underscore rule) are
//! done by hand so no input can trip a backtracking limit.
//!
//! # Line endings
//!
//! `okf.py` reads files for markdown analysis in Python text mode, which turns
//! `\r\n` and lone `\r` into `\n` (universal newlines). The functions here
//! take text as given, so callers must read it the same way: through
//! [`Workspace::read_text`], or apply the same newline conversion to text
//! from elsewhere. Raw `\r\n` text would make `## Closing ##\r` the heading
//! `Closing ##` (slug `closing-`) where `okf.py` sees `Closing`. Frontmatter
//! editing ([`crate::okf::doc`]) stays byte-preserving and never normalizes.
//!
//! # Known divergences from `okf.py`
//!
//! * **`resolve_link` directory detection is lexical.** Python asks the OS
//!   about an unnormalized path. Here `"/a/zz/../b"` gives `a/b/index.md`
//!   (Python: `a/b` when `zz` does not exist), `"/infra/x.md/.."` gives
//!   `infra/index.md` (Python: `infra`), and `".."` from `y.md` gives `..`
//!   (Python: `../index.md`). Later broken-link code must treat
//!   `escapes_root` paths explicitly.
//! * **Unicode tables differ.** Python 3.14 (Unicode 16) and the `regex`
//!   crate classify about 55 code points differently, which can change a
//!   slug; this cannot be fixed in code.
//! * **`headings` assumes its lines contain no `\n`.**

use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap};
use std::sync::LazyLock;

use indexmap::IndexMap;
use regex::bytes::Regex as BytesRegex;
use regex::Regex;

use crate::okf::paths::{dirname, join, normpath};
use crate::okf::pystr::py_unquote;
use crate::okf::pystr::{is_py_word, py_rstrip, py_strip};
use crate::okf::workspace::Workspace;

/// The text of a CommonMark angle-bracket link destination, between `<` and
/// `>`: anything except a line break, `<`, `>` or the `\x1c-\x1f` control
/// bytes, so it may hold spaces and `)`. A macro, not a `const`, because
/// `concat!` takes only literals. Shared by [`LINK`] and [`SLUG_LINK`] so the
/// two cannot disagree about where a destination ends.
macro_rules! angle_destination {
    () => {
        r"[^<>\n\r\x1c-\x1f]+"
    };
}

/// The byte that stands for a code span's bytes in masked text
/// ([`CodeRanges::mask`]). It never occurs in UTF-8, so it can't be mistaken
/// for text. [`LINK`] and [`SLUG_LINK`] accept it only in a link's text.
const SPAN_BYTE: u8 = 0xFF;
/// The byte that stands for a code block's bytes in masked text. It never
/// occurs in UTF-8, and no part of a link accepts it.
const BLOCK_BYTE: u8 = 0xFE;

/// A link's text between `[` and `]`, for [`LINK`] and [`SLUG_LINK`]: any char
/// but `]` and `\`, an escape, or a masked code span (CommonMark allows code
/// in link text). A macro for the same reason as [`angle_destination`].
macro_rules! link_label {
    () => {
        r"(?:[^\]\\]|\\.|(?-u:\xFF))*"
    };
}

/// A markdown link `[text](target "title")` or `[text](<target> "title")`,
/// not an image (`![...]`). Group 3 is a plain destination (no whitespace, no
/// `)`); group 4 is an angle-bracket destination (CommonMark): anything
/// between `<` and `>` except a line break, `<`, `>` or the `\x1c-\x1f`
/// control bytes, so it may hold spaces and `)`. The two are plain
/// alternatives of the linear-time `regex` crate.
///
/// It runs on masked bytes ([`CodeRanges::mask`]). Its Unicode classes match
/// only valid UTF-8, so a mask byte matches only where it is named: a code
/// span in the text, and either mask byte as the char before the `[`. A
/// destination, the space before a title and the title itself never match
/// one, so every byte from a match's `(` to its `)` is the original text.
static LINK: LazyLock<BytesRegex> = LazyLock::new(|| {
    BytesRegex::new(concat!(
        r#"(?:\A|[^!]|(?-u:[\xFE\xFF]))(\[("#,
        link_label!(),
        r#")\]\((?:<("#,
        angle_destination!(),
        r#")>|([^)\s\x1c-\x1f]+))(?:[\s\x1c-\x1f]+"[^"]*")?\))"#,
    ))
    .expect("LINK regex is valid")
});
static HEADING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(#{1,6})(?:[ \t]+(.*?))?(?:[ \t]+#+)?[ \t]*$").expect("HEADING regex is valid")
});
static FENCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\s\x1c-\x1f]{0,3}(```|~~~)").expect("FENCE regex is valid"));
static FOOTNOTE_DEF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[\^([^\]]+)\]:").expect("FOOTNOTE_DEF regex is valid"));
/// A footnote reference `[^label]` (`okf.py`'s `FOOTREF_RE` without its
/// `(?!:)` lookahead, which [`footnote_refs`] applies by hand).
static FOOTNOTE_REF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[\^([^\]]+)\]").expect("FOOTNOTE_REF regex is valid"));
static HTML_ANCHOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)<a[\s\x1c-\x1f][^>]*(?:name|[i\u{130}\u{131}]d)=["']([^"']+)["']"#)
        .expect("HTML_ANCHOR regex is valid")
});
static URI_SCHEME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^[a-zA-Z\u{130}\u{131}][a-zA-Z0-9+.\-\u{130}\u{131}]*:")
        .expect("URI_SCHEME regex is valid")
});
/// A link or image whose text `slugify` keeps. Group 1 is the text. The
/// destination is an angle-bracket one (as in [`LINK`], with an optional
/// title) or, failing that, anything up to the first `)`. It runs on masked
/// bytes and, like [`LINK`], accepts a mask byte only in the text.
static SLUG_LINK: LazyLock<BytesRegex> = LazyLock::new(|| {
    BytesRegex::new(concat!(
        r#"!?\[("#,
        link_label!(),
        r#")\]\((?:<"#,
        angle_destination!(),
        r#">(?:[\s\x1c-\x1f]+"[^"]*")?|[^)]*)\)"#,
    ))
    .expect("SLUG_LINK regex is valid")
});
static SLUG_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<[^>]+>").expect("SLUG_TAG regex is valid"));
static SLUG_PUNCTUATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[^\p{L}\p{N}_\- ]").expect("SLUG_PUNCTUATION regex is valid"));

/// One link occurrence in a line of text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkMatch {
    /// The link text between the brackets.
    pub text: String,
    /// The raw target between the parentheses, without the `<` `>` of an
    /// angle-bracket destination.
    pub target: String,
    /// Byte range of the whole `[text](target)` in the line.
    pub range: std::ops::Range<usize>,
    /// Byte range of the target within the line (inside the `<` `>` of an
    /// angle-bracket destination, so replacing it keeps the brackets).
    pub target_range: std::ops::Range<usize>,
}

/// Byte ranges of a text that CommonMark reads as code: inline code spans
/// (backticks included) and fenced or indented code blocks. Link syntax
/// inside them is literal text, not a link.
///
/// `pulldown-cmark` finds them, so matching backtick runs, nested fences
/// and indented blocks follow the spec. It parses once, in linear time. A
/// leading `---` frontmatter block is read as metadata, so it can't pair
/// backticks with the body.
///
/// The ranges come back sorted and disjoint.
struct CodeRanges(Vec<CodeRange>);

/// One [`CodeRanges`] entry: its byte range and whether it is a code block.
struct CodeRange {
    range: std::ops::Range<usize>,
    block: bool,
}

impl CodeRanges {
    /// Parses `text`. Text with no backtick, tilde, tab or run of four
    /// spaces can't hold code, so it skips the parse.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "per-element predicate or scanner called in a hot loop (parser or analysis)"
        )
    )]
    fn of(text: &str) -> Self {
        if !text.contains(['`', '~', '\t']) && !text.contains("    ") {
            return Self(Vec::new());
        }
        let parser = pulldown_cmark::Parser::new_ext(
            text,
            pulldown_cmark::Options::ENABLE_YAML_STYLE_METADATA_BLOCKS,
        );
        let ranges = parser
            .into_offset_iter()
            .filter_map(|(event, range)| match event {
                pulldown_cmark::Event::Code(_) => Some(CodeRange {
                    range,
                    block: false,
                }),
                pulldown_cmark::Event::Start(pulldown_cmark::Tag::CodeBlock(_)) => {
                    Some(CodeRange { range, block: true })
                }
                _ => None,
            })
            .collect();
        Self(ranges)
    }

    /// `text` as bytes, with every code byte but line breaks replaced by
    /// [`SPAN_BYTE`] (in a code span) or [`BLOCK_BYTE`] (in a code block):
    /// the same length, so byte offsets and lines are unchanged. [`LINK`] and
    /// [`SLUG_LINK`] run on it in one linear pass, and by construction:
    ///
    /// * code can't open, close or swallow a link (`` `[` [x](a.md) `` has
    ///   the link `[x](a.md)`), since no mask byte is `[`, `]` or `(`;
    /// * code can't become a destination, a title or the space before one,
    ///   since neither regex accepts a mask byte there. A match's `(...)` is
    ///   always the original bytes, so masking never makes a link `okf.py`
    ///   would not see. CommonMark reads a real link's destination as raw
    ///   text, so code found there means it is no link (`` [x](`a b.md`) ``);
    /// * only a code span may sit in a link's text, as CommonMark allows; a
    ///   code block ends any link around it.
    ///
    /// A link's text and target are read from the original.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "per-element predicate or scanner called in a hot loop (parser or analysis)"
        )
    )]
    fn mask<'a>(&self, text: &'a str) -> Cow<'a, [u8]> {
        if self.0.is_empty() {
            return Cow::Borrowed(text.as_bytes());
        }
        let mut bytes = text.as_bytes().to_vec();
        for code in &self.0 {
            let mask = if code.block { BLOCK_BYTE } else { SPAN_BYTE };
            // `get_mut` instead of indexing: pulldown-cmark's ranges lie
            // within the text, and a bad one is skipped rather than a panic.
            for byte in bytes.get_mut(code.range.clone()).into_iter().flatten() {
                if !matches!(*byte, b'\n' | b'\r') {
                    *byte = mask;
                }
            }
        }
        Cow::Owned(bytes)
    }
}

/// Finds every markdown link (not image) in `text`, left to right, except
/// links written as code: inside an inline code span or a fenced or
/// indented code block.
///
/// All ranges are byte offsets into `text` (which may span lines), lie on
/// `char` boundaries, and are safe to pass to `String::replace_range`.
///
/// # Examples
/// ```
/// use okf_mcp::okf::markdown::find_links;
/// let links = find_links("See [the server](/infra/x.md#ports) and ![img](a.png).");
/// assert_eq!(links.len(), 1);
/// assert_eq!(links[0].target, "/infra/x.md#ports");
/// assert!(find_links("Write `[text](a.md)` for a link.").is_empty());
/// ```
#[tracing::instrument(level = "debug", skip_all)]
pub fn find_links(text: &str) -> Vec<LinkMatch> {
    links_in_masked(text, &CodeRanges::of(text).mask(text))
}

/// Every line of `text` (split on `\n`) with its links, found as
/// [`find_links`] finds them but with the code ranges taken from the whole
/// text, so a fenced block or a code span that runs over several lines
/// hides the links on every line it covers. Link ranges are relative to
/// their line.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) fn link_lines(text: &str) -> impl Iterator<Item = (&str, Vec<LinkMatch>)> {
    let masked = CodeRanges::of(text).mask(text).into_owned();
    let mut masked_start = 0;
    text.split('\n').map(move |line| {
        // Masking keeps every `\n`, so the lines line up byte for byte.
        let masked_line = &masked[masked_start..masked_start + line.len()];
        masked_start += line.len() + 1;
        (line, links_in_masked(line, masked_line))
    })
}

/// The links of `text`, found on `masked`: `text` with its code masked
/// ([`CodeRanges::mask`]), the same length.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "per-element predicate or scanner called in a hot loop (parser or analysis)"
    )
)]
fn links_in_masked(text: &str, masked: &[u8]) -> Vec<LinkMatch> {
    // Python's `(?<!!)` means: the char before `[` is not `!`. `regex` has no
    // lookbehind, so LINK consumes that char instead (`[^!]`, or `\A` at the
    // text start) and the real link is group 1. Because a rejected `![` simply
    // fails to match, the scan stays linear. After a match we resume at its
    // closing `)` (one byte, never `!`), so an adjacent link `[a](b)[c](d)`
    // can use that `)` as its preceding char, as in Python.
    let mut links = Vec::new();
    let mut pos = 0;
    // `captures_at` searches from `pos` but still sees the text before it, so
    // `\A` matches only at the true start of `masked`.
    while let Some(caps) = LINK.captures_at(masked, pos) {
        // Exactly one of group 3 (plain) and 4 (angle-bracket) matched.
        let (Some(whole), Some(label), Some(target)) = (
            caps.get(1),
            caps.get(2),
            caps.get(3).or_else(|| caps.get(4)),
        ) else {
            break; // unreachable: groups 1, 2 and one of 3, 4 are mandatory
        };
        // The ranges are the same in `text`, which still holds the code. They
        // start and end at a `[`, `]`, `<`, `>`, `(`, `)` or whitespace char,
        // all of which are whole chars of `text`, so slicing can't split one.
        links.push(LinkMatch {
            text: text[label.range()].to_string(),
            target: text[target.range()].to_string(),
            range: whole.range(),
            target_range: target.range(),
        });
        pos = whole.end() - 1;
    }
    links
}

/// Splits a document's text into `(frontmatter, body, number of lines before the body)`.
///
/// ```
/// use okf_mcp::okf::markdown::split_frontmatter;
/// assert_eq!(split_frontmatter("---\na: 1\n---\nbody"), ("a: 1", "body", 3));
/// ```
///
/// Port of `okf.py`'s `split_frontmatter`: frontmatter needs a `---\n`
/// first line and a later line starting with `---`.
#[tracing::instrument(level = "debug", skip_all)]
pub fn split_frontmatter(text: &str) -> (&str, &str, usize) {
    if let Some(rest) = text.strip_prefix("---\n") {
        if let Some(end) = rest.find("\n---").map(|p| p + 4) {
            let body_start = text[end + 4..]
                .find('\n')
                .map_or(text.len(), |p| end + 4 + p + 1);
            let offset = text[..body_start].matches('\n').count();
            return (&text[4..end], &text[body_start..], offset);
        }
    }
    ("", text, 0)
}

/// Splits a document's text on `\n` into (frontmatter lines with delimiters, body lines).
///
/// Port of `okf.py`'s `body_split`. Without frontmatter the first part is empty.
///
/// ```
/// use okf_mcp::okf::markdown::body_split;
/// assert_eq!(body_split("---\na: 1\n---\nx"), (vec!["---", "a: 1", "---"], vec!["x"]));
/// ```
#[tracing::instrument(level = "debug", skip_all)]
pub fn body_split(text: &str) -> (Vec<&str>, Vec<&str>) {
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.first().map(|l| l.trim_end_matches('\r')) == Some("---") {
        if let Some(j) = (1..lines.len())
            .find(|&j| matches!(py_rstrip(lines[j].trim_end_matches('\r')), "---" | "..."))
        {
            return (lines[..=j].to_vec(), lines[j + 1..].to_vec());
        }
    }
    (Vec::new(), lines)
}

/// The fence marker (`` ``` `` or `~~~`) if `line` opens or closes a fenced
/// block (`okf.py`'s `FENCE_RE.match`).
///
/// # Returns
/// The marker, borrowed from `line`; `None` if the line is not a fence.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "per-element predicate or scanner called in a hot loop (parser or analysis)"
    )
)]
pub(crate) fn fence_marker(line: &str) -> Option<&str> {
    FENCE
        .captures(line)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str())
}

/// The open fence after a fence line with `marker`: a fence closes only with
/// the marker that opened it (`okf.py`'s
/// `None if fence == f.group(1) else (fence or f.group(1))`).
///
/// # Arguments
/// * `open` - the marker of the fence open before this line, if any.
/// * `marker` - this line's marker, from [`fence_marker`].
///
/// # Returns
/// The marker of the fence open after this line, if any.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "per-element predicate or scanner called in a hot loop (parser or analysis)"
    )
)]
pub(crate) fn next_fence<'a>(open: Option<&'a str>, marker: &'a str) -> Option<&'a str> {
    match open {
        Some(open) if open == marker => None,
        Some(open) => Some(open),
        None => Some(marker),
    }
}
/// An ATX heading found in a list of lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heading {
    /// Index of the heading's line.
    pub index: usize,
    /// 1 to 6.
    pub level: usize,
    /// The heading text, trimmed, without closing `#`s.
    pub text: String,
    /// Its GitHub anchor slug, numbered for repeats (`x`, `x-1`, `x-2`).
    pub slug: String,
}

/// An ATX heading line's level and its text as written: `HEADING_RE`'s
/// groups 1 and 2, the text not stripped (`cmd_split` re-levels with it).
///
/// # Arguments
/// * `line` - one line, without its `\n`.
///
/// # Returns
/// `(number of #, text)` (`""` for a bare `#`), or `None` for any other line.
/// It does not know about fences: [`headings`] skips fenced code itself.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "per-element predicate or scanner called in a hot loop (parser or analysis)"
    )
)]
pub(crate) fn atx_heading(line: &str) -> Option<(usize, &str)> {
    // `?` on an `Option`: a line that is not a heading returns `None` here.
    let caps = HEADING.captures(line)?;
    // Indexing is safe: group 1 (the `#`s) is mandatory in HEADING. Group 2
    // is absent for a bare `#`, so it is read with `get`, which cannot panic.
    Some((caps[1].len(), caps.get(2).map_or("", |text| text.as_str())))
}

/// The ATX headings in `lines`, skipping fenced code, with their anchor slugs.
#[tracing::instrument(level = "debug", skip_all)]
pub fn headings(lines: &[&str]) -> Vec<Heading> {
    let mut found = Vec::new();
    let mut fence: Option<&str> = None;
    for (index, line) in lines.iter().enumerate() {
        if let Some(marker) = fence_marker(line) {
            fence = next_fence(fence, marker);
            continue;
        }
        if fence.is_some() {
            continue;
        }
        if let Some((level, text)) = atx_heading(line) {
            found.push(Heading {
                index,
                level,
                text: py_strip(text).to_string(),
                slug: String::new(),
            });
        }
    }
    // Only looked up, never iterated, so plain hashing is enough.
    let mut seen: HashMap<String, usize> = HashMap::new();
    for heading in &mut found {
        let base = slugify(&heading.text);
        let slug = match seen.get(&base) {
            Some(count) => format!("{base}-{count}"),
            None => base.clone(),
        };
        *seen.entry(base).or_insert(0) += 1;
        seen.entry(slug.clone()).or_insert(1);
        heading.slug = slug;
    }
    found
}

/// GitHub's heading anchor: rendered text, lowercased, punctuation dropped,
/// spaces turned into `-`.
///
/// # Examples
/// ```
/// use okf_mcp::okf::markdown::slugify;
/// assert_eq!(slugify("What it *wouldn't* change"), "what-it-wouldnt-change");
/// assert_eq!(slugify("Use `okf.py` [here](/x.md)"), "use-okfpy-here");
/// assert_eq!(slugify("snake_case stays"), "snake_case-stays");
/// ```
#[tracing::instrument(level = "debug", skip_all)]
pub fn slugify(text: &str) -> String {
    // A link written as code (`` `[a](b)` ``) is literal heading text, so
    // SLUG_LINK runs on the text with its code masked; the kept link text is
    // read from the original.
    let masked = CodeRanges::of(text).mask(text);
    let mut linked = String::new();
    let mut copied = 0;
    for caps in SLUG_LINK.captures_iter(&masked) {
        let (Some(whole), Some(label)) = (caps.get(0), caps.get(1)) else {
            continue; // unreachable: groups 0 and 1 always match
        };
        linked.push_str(&text[copied..whole.start()]);
        linked.push_str(&text[label.range()]);
        copied = whole.end();
    }
    let text = if copied == 0 {
        Cow::Borrowed(text)
    } else {
        linked.push_str(&text[copied..]);
        Cow::Owned(linked)
    };
    let text = SLUG_TAG.replace_all(&text, "");
    // Only allocate the backtick-free copy when there is a backtick.
    let text = if text.contains('`') {
        Cow::Owned(text.replace('`', ""))
    } else {
        text
    };
    let text = strip_emphasis(&text);
    let lowered = py_strip(&text).to_lowercase();
    SLUG_PUNCTUATION.replace_all(&lowered, "").replace(' ', "-")
}

/// Removes emphasis markers the way `slugify`'s Python pattern
/// `\*+|(?<!\w)_+|_+(?!\w)` does, in one linear pass.
///
/// Every `*` goes. A run of `_` goes whole unless it sits between two word
/// characters (so `snake_case` keeps its underscore). Neighbours are judged in
/// the original text, so `a*_b` loses both.
#[tracing::instrument(level = "trace", skip_all)]
fn strip_emphasis(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.char_indices().peekable();
    let mut previous: Option<char> = None;
    while let Some((start, c)) = chars.next() {
        match c {
            '*' => {}
            '_' => {
                let mut end = start + 1;
                while let Some(&(at, '_')) = chars.peek() {
                    end = at + 1;
                    chars.next();
                }
                let next = text[end..].chars().next();
                let between_words =
                    previous.is_some_and(is_py_word) && next.is_some_and(is_py_word);
                if between_words {
                    out.push_str(&text[start..end]);
                }
                previous = Some('_');
                continue;
            }
            _ => out.push(c),
        }
        previous = Some(c);
    }
    out
}

/// Every fragment a link may target in a document: heading slugs plus HTML
/// `<a name|id>` anchors.
#[tracing::instrument(level = "debug", skip_all)]
pub fn anchors_in(text: &str) -> BTreeSet<String> {
    let (_, body, _) = split_frontmatter(text);
    let lines: Vec<&str> = body.split('\n').collect();
    let mut anchors: BTreeSet<String> = headings(&lines).into_iter().map(|h| h.slug).collect();
    anchors.extend(HTML_ANCHOR.captures_iter(body).map(|c| c[1].to_string()));
    anchors
}

/// Footnote definitions: label to the line indexes of its definition and
/// indented continuation lines, in first-definition order. A blank line
/// ends a definition, as in `okf.py` (see [`footnote_paragraphs`] for the
/// markdown reading, where it need not).
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "one-line delegation to an instrumented function"
    )
)]
pub fn footnote_defs(lines: &[&str]) -> IndexMap<String, Vec<usize>> {
    collect_footnotes(lines, false)
}

/// Like [`footnote_defs`], but a definition runs on across blank lines into
/// later indented paragraphs, as a markdown renderer reads it: in
/// `"[^n]: One.\n\n    Two."` both paragraphs belong to `n`. The blank
/// lines themselves are not listed.
///
/// # Examples
/// ```
/// use okf_mcp::okf::markdown::footnote_paragraphs;
/// let defs = footnote_paragraphs(&["[^n]: One.", "", "    Two.", "Text."]);
/// assert_eq!(defs.get("n"), Some(&vec![0, 2]));
/// ```
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "one-line delegation to an instrumented function"
    )
)]
pub fn footnote_paragraphs(lines: &[&str]) -> IndexMap<String, Vec<usize>> {
    collect_footnotes(lines, true)
}

/// The one walk behind [`footnote_defs`] and [`footnote_paragraphs`];
/// `across_blank` says whether a blank line keeps the current definition open.
#[tracing::instrument(level = "trace", skip_all)]
fn collect_footnotes(lines: &[&str], across_blank: bool) -> IndexMap<String, Vec<usize>> {
    let mut defs: IndexMap<String, Vec<usize>> = IndexMap::new();
    let mut current: Option<String> = None;
    for (index, line) in lines.iter().enumerate() {
        let blank = py_strip(line).is_empty();
        if let Some(caps) = FOOTNOTE_DEF.captures(line) {
            // Indexing is safe: group 1 is mandatory in FOOTNOTE_DEF.
            let label = caps[1].to_string();
            defs.entry(label.clone()).or_default().push(index);
            current = Some(label);
        } else if current.is_some()
            && !blank
            && (line.starts_with("    ") || line.starts_with('\t'))
        {
            if let Some(lines) = current.as_deref().and_then(|label| defs.get_mut(label)) {
                lines.push(index);
            }
        } else if !(blank && across_blank) {
            current = None;
        }
    }
    defs
}

/// The footnote labels `text` cites: `set(FOOTREF_RE.findall(text))`,
/// every `[^label]` that is not followed by `:` (which makes it a
/// definition).
///
/// # Arguments
/// * `text` - any markdown (several lines joined with `\n` is fine).
///
/// # Returns
/// The labels, sorted (a `BTreeSet`, so callers iterate deterministically).
///
/// # Examples
/// ```
/// use okf_mcp::okf::markdown::footnote_refs;
/// let refs = footnote_refs("Cited.[^a]\n\n[^a]: The definition.");
/// assert_eq!(refs.into_iter().collect::<Vec<_>>(), ["a"]);
/// ```
#[tracing::instrument(level = "debug", skip_all)]
pub fn footnote_refs(text: &str) -> BTreeSet<String> {
    // Python's `(?!:)` rejects a match followed by `:`, and Python then tries
    // every later start inside it. Each of those ends at the same `]` (a
    // label holds no `]`), so each is rejected too: skipping the whole match,
    // as `captures_iter` does, finds the same labels.
    FOOTNOTE_REF
        .captures_iter(text)
        .filter(|caps| {
            let end = caps.get(0).map_or(text.len(), |whole| whole.end());
            !text[end..].starts_with(':')
        })
        .map(|caps| caps[1].to_string())
        .collect()
}

/// Where a link points inside the bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLink {
    /// Bundle-relative target path (a directory link becomes its `index.md`).
    pub path: String,
    /// Was the link written bundle-absolute (`/dir/x.md`)?
    pub absolute: bool,
}

/// Resolves a link target written in `source` to a bundle path.
///
/// A link destination is URL-encoded (CommonMark), so the path part is
/// percent-decoded once before it is read as a path: `a%20b.md` is the file
/// `a b.md`, and a file literally named `a%20b.md` is written `a%2520b.md`.
/// A `%` not followed by two hex digits stays literal. The `#fragment` is
/// left alone here (the anchor checks decode it themselves). This is the one
/// place links are decoded; [`encode_link_path`] is its inverse for the
/// operations that write a destination.
///
/// # Arguments
/// * `workspace` - the catalog, to tell directories from files.
/// * `link` - the raw link target, possibly with a `#fragment`.
/// * `source` - bundle-relative path of the document containing the link.
///
/// # Returns
/// `None` for external links (any URI scheme) and pure `#fragment` links.
#[tracing::instrument(level = "debug", skip_all)]
pub fn resolve_link(workspace: &dyn Workspace, link: &str, source: &str) -> Option<ResolvedLink> {
    let raw_target = link.split('#').next().unwrap_or_default();
    // The scheme check reads the destination as written: `mailto%3Aa` is a
    // path, not an address.
    if raw_target.is_empty() || URI_SCHEME.is_match(raw_target) {
        return None;
    }
    let decoded = py_unquote(raw_target);
    let target = decoded.as_ref();
    let absolute = target.starts_with('/');
    let mut path = if absolute {
        target.trim_start_matches('/').to_string()
    } else {
        normpath(&join(dirname(source), target))
    };
    if target.ends_with('/') || workspace.is_dir(&path) {
        path = join(&path, "index.md");
    }
    Some(ResolvedLink {
        path: normpath(&path),
        absolute,
    })
}

/// Percent-encodes a bundle path for use as a markdown link destination, so
/// that [`find_links`] still parses it and [`resolve_link`] reads it back as
/// the same path.
///
/// Encodes every character that would end or change a destination: controls,
/// every Unicode whitespace character (the link parser's `\s`, such as
/// U+00A0), `%`, `#`, `?`, `(`, `)`, `<`, `>`, `[`, `]`, `\\`, `"`, `'` and
/// `:` (so a name like `note:part.md` is not read as a URI scheme). A
/// non-ASCII character is encoded as its UTF-8 bytes. Everything else,
/// including other non-ASCII text and `/`, stays as written.
///
/// # Arguments
/// * `path` - a bundle path, or a relative path such as `../a b.md`.
///
/// # Returns
/// `path` itself (borrowed) when nothing needs encoding.
///
/// # Examples
/// ```
/// use okf_mcp::okf::markdown::encode_link_path;
/// assert_eq!(encode_link_path("a b/100%.md"), "a%20b/100%25.md");
/// assert_eq!(encode_link_path("a\u{a0}b:c.md"), "a%C2%A0b%3Ac.md");
/// assert_eq!(encode_link_path("plain/x.md"), "plain/x.md");
/// ```
#[tracing::instrument(level = "trace", skip_all)]
pub fn encode_link_path(path: &str) -> std::borrow::Cow<'_, str> {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let needs = |ch: char| {
        ch.is_whitespace()
            || ch.is_control()
            || matches!(
                ch,
                '%' | '#' | '?' | '(' | ')' | '<' | '>' | '[' | ']' | '\\' | '"' | '\'' | ':'
            )
    };
    if !path.chars().any(needs) {
        return std::borrow::Cow::Borrowed(path);
    }
    let mut out = String::with_capacity(path.len() + 8);
    for ch in path.chars() {
        if needs(ch) {
            // `encode_utf8` writes the char's UTF-8 bytes into a buffer; each
            // byte becomes `%XX`.
            let mut buf = [0u8; 4];
            for byte in ch.encode_utf8(&mut buf).bytes() {
                out.push('%');
                out.push(char::from(HEX[usize::from(byte >> 4)]));
                out.push(char::from(HEX[usize::from(byte & 15)]));
            }
        } else {
            out.push(ch);
        }
    }
    std::borrow::Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::okf::workspace::MemWorkspace;

    #[test]
    fn footnote_refs_skip_definitions() {
        let text = "A[^a] and [^b c]. [^d]: def\n[^a]: again [^e]";

        let refs = footnote_refs(text);

        let expected = BTreeSet::from(["a".to_string(), "b c".to_string(), "e".to_string()]);
        assert_eq!(refs, expected);
    }

    #[test]
    fn atx_heading_keeps_the_text_as_written() {
        let cases = [
            atx_heading("### Detail ##"),
            atx_heading("#"),
            atx_heading("#nospace"),
            atx_heading("####### seven"),
        ];

        assert_eq!(cases, [Some((3, "Detail")), Some((1, "")), None, None]);
    }

    #[test]
    fn crlf_text_read_for_analysis_matches_okf_py() {
        let raw =
            "---\r\nk: v\r\n---\r\n## Closing ##\r\n```\r\n# no\r\n```\r\n[^n]: a\r\n    b\r\n";
        let workspace = MemWorkspace::from_files([("crlf.md", raw)]);
        let text = workspace.read_text("crlf.md").expect("readable");
        let (_, body) = body_split(&text);
        let found = headings(&body);
        assert_eq!(found.len(), 1);
        assert_eq!(
            (found[0].text.as_str(), found[0].slug.as_str()),
            ("Closing", "closing")
        );
        assert_eq!(footnote_defs(&body).get("n"), Some(&vec![4, 5]));
    }

    #[test]
    fn find_links_skips_images_and_keeps_inner_links() {
        let links = find_links("![a [b](c)](d) and [e](f \"title\") [g\\]h](i)");
        let targets: Vec<_> = links.iter().map(|l| l.target.as_str()).collect();
        assert_eq!(targets, ["c", "f", "i"]);
        assert_eq!(links[2].text, "g\\]h");
        let line = "x [t](/a.md) y";
        let link = &find_links(line)[0];
        assert_eq!(&line[link.range.clone()], "[t](/a.md)");
        assert_eq!(&line[link.target_range.clone()], "/a.md");
    }

    #[test]
    fn find_links_reads_angle_bracket_destinations() {
        let line = "a [x](<a b.md>) b [y](<a b.md> \"title\") c [z](<a(1).md>) d [w](<q)r.md#h>)";
        let links = find_links(line);
        let targets: Vec<_> = links.iter().map(|l| l.target.as_str()).collect();
        assert_eq!(targets, ["a b.md", "a b.md", "a(1).md", "q)r.md#h"]);
        // The target range sits inside the brackets; the whole link spans them.
        assert_eq!(&line[links[0].target_range.clone()], "a b.md");
        assert_eq!(&line[links[0].range.clone()], "[x](<a b.md>)");
        assert_eq!(&line[links[1].range.clone()], "[y](<a b.md> \"title\")");
    }

    #[test]
    fn find_links_rejects_malformed_angle_bracket_destinations() {
        for line in [
            "![img](<a b.md>)",
            "[x](<>)",
            "[x](<a b.md)",
            "[x](<a<b>.md>)",
            "[x](<a\nb.md>)",
            "[x](<a\u{1c}b.md>)",
            "[x](<a b.md>junk)",
        ] {
            assert!(
                find_links(line).iter().all(|l| l.target != "a b.md"
                    && l.target != "a<b>.md"
                    && l.target != "a\nb.md"
                    && l.target != "a\u{1c}b.md"
                    && !l.target.is_empty()),
                "{line:?}"
            );
        }
        // `[x](<a)` is still the plain destination `<a`, as before.
        assert_eq!(find_links("[x](<a)")[0].target, "<a");
    }

    #[test]
    fn find_links_stays_linear_on_unclosed_angle_brackets() {
        let line = format!("[x](<{}", "a ".repeat(200_000));
        assert!(find_links(&line).is_empty());
    }

    fn targets(text: &str) -> Vec<String> {
        find_links(text).into_iter().map(|l| l.target).collect()
    }

    #[test]
    fn a_link_in_a_code_span_is_not_found() {
        assert!(targets("Write `[x](<a b.md>)` to link.").is_empty());
        assert!(targets("`[x](a.md)`").is_empty());
    }

    #[test]
    fn a_link_in_a_fenced_block_is_not_found() {
        assert!(targets("```\n[x](a.md)\n```\n").is_empty());
        assert!(targets("~~~md\n[x](a.md)\n~~~\n").is_empty());
        // A longer fence holds a shorter one.
        assert!(targets("````\n```\n[x](a.md)\n```\n````\n").is_empty());
        // After the block closes, links count again.
        assert_eq!(targets("```\n[x](a.md)\n```\n[y](b.md)"), ["b.md"]);
    }

    #[test]
    fn a_link_next_to_a_code_span_is_still_found() {
        let text = "`code` [x](a.md) and `more` [y](b.md)";
        assert_eq!(targets(text), ["a.md", "b.md"]);
        let found = find_links(text);
        assert_eq!(&text[found[0].range.clone()], "[x](a.md)");
        // Code inside the link text leaves the link intact.
        assert_eq!(targets("[`x`](a.md)"), ["a.md"]);
    }

    #[test]
    fn a_link_in_an_indented_code_block_is_not_found() {
        assert!(targets("Intro\n\n    [x](a.md)\n\nOutro").is_empty());
        assert_eq!(targets("Intro\n\n    [x](a.md)\n\n[y](b.md)"), ["b.md"]);
    }

    #[test]
    fn a_double_backtick_span_may_hold_a_single_backtick() {
        assert!(targets("``a ` [x](a.md) b``").is_empty());
        assert_eq!(targets("``a ` b`` [x](a.md)"), ["a.md"]);
    }

    #[test]
    fn an_unclosed_backtick_is_literal_and_the_link_is_found() {
        assert_eq!(targets("a ` [x](a.md)"), ["a.md"]);
        // A run of a different length does not close the span.
        assert_eq!(targets("``a ` [x](a.md)"), ["a.md"]);
    }

    #[test]
    fn a_link_after_a_code_span_with_an_opening_bracket_is_found() {
        let text = "`[` [x](a.md)";
        assert_eq!(targets(text), ["a.md"]);
        let found = find_links(text);
        assert_eq!(&text[found[0].range.clone()], "[x](a.md)");
        // A `]` in code does not close the label, and the label keeps its code.
        let text = "[a `]` b](x.md)";
        assert_eq!(targets(text), ["x.md"]);
        assert_eq!(find_links(text)[0].text, "a `]` b");
        // Several code-span brackets before the link, as lines too.
        assert_eq!(targets("`[` `]` `(` [x](a.md)"), ["a.md"]);
        let found: Vec<_> = link_lines("`[`\n[x](a.md)").map(|(_, l)| l.len()).collect();
        assert_eq!(found, [0, 1]);
    }

    #[test]
    fn many_code_span_brackets_before_a_link_stay_linear() {
        let text = format!("{}[x](a.md)", "`[` ".repeat(50_000));
        assert_eq!(targets(&text), ["a.md"]);
    }

    #[test]
    fn code_never_becomes_a_destination_or_title() {
        // A bare destination may not hold a space, so this is no link and
        // CommonMark reads `` `a b.md` `` as code. Code cannot stand in for a
        // destination's bytes.
        assert!(find_links("[x](`a b.md`)").is_empty());
        assert!(find_links("[x](a`b c`)").is_empty());
        // Nor for the whitespace before a title, or an angle destination.
        assert!(find_links("[x](a.md` `\"t\")").is_empty());
        assert!(find_links("[x](<a` b>`)").is_empty());
        // Code that CommonMark never sees, in a valid destination, is kept.
        assert_eq!(targets("[x](<a` `b>)"), ["a` `b"]);
        // A real link after or inside the rejected one is still found.
        assert_eq!(targets("[x](`a b.md`) [y](b.md)"), ["b.md"]);
        assert_eq!(targets("[x](`a b`[y](b.md))"), ["b.md"]);
        // The heading anchor keeps the text literal, as GitHub renders it.
        assert_eq!(slugify("See [x](`a b`)"), "see-xa-b");
    }

    #[test]
    fn a_destination_with_backticks_and_no_space_is_a_link() {
        // CommonMark scans a destination as raw text, so `` `a.md` `` is the
        // destination and not code; okf.py's LINK_RE reads the same target.
        let text = "[x](`a.md`)";
        let found = find_links(text);
        assert_eq!(targets(text), ["`a.md`"]);
        assert_eq!(&text[found[0].range.clone()], text);
        assert_eq!(targets("[x](a`b`.md \"t\")"), ["a`b`.md"]);
    }

    #[test]
    fn a_link_label_cannot_span_a_code_block() {
        assert!(targets("[a\n```\n[b]\n```\nc](d.md)").is_empty());
        // A code span right before a link still lets it open.
        assert_eq!(targets("`c`[x](a.md)"), ["a.md"]);
    }

    #[test]
    fn a_code_span_that_swallows_the_closing_bracket_hides_the_link() {
        assert!(targets("[a `b](c.md)` d").is_empty());
    }

    #[test]
    fn frontmatter_does_not_pair_backticks_with_the_body() {
        let text = "---\ntitle: a ` b\n---\nSee [x](a.md) and `c`.\n";
        assert_eq!(targets(text), ["a.md"]);
    }

    #[test]
    fn link_lines_hide_links_on_every_line_a_block_or_span_covers() {
        let text = "[a](a.md)\n```\n[b](b.md)\n```\n`x\n[c](c.md) y`\n[d](d.md)";
        let found: Vec<Vec<String>> = link_lines(text)
            .map(|(_, links)| links.into_iter().map(|l| l.target).collect())
            .collect();
        assert_eq!(found[0], ["a.md"]);
        assert!(found[2].is_empty());
        assert!(found[5].is_empty());
        assert_eq!(found[6], ["d.md"]);
    }

    #[test]
    fn find_links_stays_linear_on_many_backticks() {
        let text = "`[x](a.md)` ".repeat(50_000);
        assert!(find_links(&text).is_empty());
    }

    #[test]
    fn slugify_keeps_a_link_written_as_code_literal() {
        assert_eq!(slugify("Use `[a](b)` here"), "use-ab-here");
        assert_eq!(slugify("Use [a](b) and `x`"), "use-a-and-x");
    }

    #[test]
    fn an_angle_bracket_destination_resolves_like_its_plain_form() {
        let ws = MemWorkspace::from_files([("infra/a b.md", ""), ("infra/x(1).md", "")]);
        let resolve = |text: &str| {
            let link = find_links(text).into_iter().next().expect("a link");
            resolve_link(&ws, &link.target, "infra/index.md").map(|r| r.path)
        };
        assert_eq!(resolve("[x](<a b.md>)"), Some("infra/a b.md".into()));
        assert_eq!(
            resolve("[x](<a b.md> \"title\")"),
            Some("infra/a b.md".into())
        );
        assert_eq!(resolve("[x](<a%20b.md>)"), Some("infra/a b.md".into()));
        assert_eq!(resolve("[x](<x(1).md#h>)"), Some("infra/x(1).md".into()));
        assert_eq!(resolve("[x](<https://example.com/a b>)"), None);
    }

    #[test]
    fn split_frontmatter_cases() {
        assert_eq!(
            split_frontmatter("---\na: 1\n---\nbody\n"),
            ("a: 1", "body\n", 3)
        );
        assert_eq!(split_frontmatter("---\na: 1\n---"), ("a: 1", "", 2));
        assert_eq!(split_frontmatter("no fm\n"), ("", "no fm\n", 0));
    }

    #[test]
    fn body_split_cases() {
        let (head, body) = body_split("---\na: 1\n...\nbody");
        assert_eq!(head, ["---", "a: 1", "..."]);
        assert_eq!(body, ["body"]);
        let (head, body) = body_split("plain\ntext");
        assert!(head.is_empty());
        assert_eq!(body, ["plain", "text"]);
    }

    #[test]
    fn headings_skip_fences_and_number_repeats() {
        let lines = [
            "# Intro",
            "```",
            "# not a heading",
            "~~~",
            "# still code",
            "```",
            "## Intro ##",
            "### Intro",
            "#nospace",
            "####### seven",
        ];
        let found = headings(&lines);
        let summary: Vec<_> = found
            .iter()
            .map(|h| (h.index, h.level, h.slug.as_str()))
            .collect();
        assert_eq!(
            summary,
            [(0, 1, "intro"), (6, 2, "intro-1"), (7, 3, "intro-2")]
        );
        assert_eq!(found[1].text, "Intro");
    }

    #[test]
    fn repeat_numbering_avoids_existing_slugs() {
        let found = headings(&["# x-1", "# x", "# x"]);
        let slugs: Vec<_> = found.iter().map(|h| h.slug.as_str()).collect();
        assert_eq!(slugs, ["x-1", "x", "x-1"]);
    }

    #[test]
    fn slugify_cases() {
        assert_eq!(
            slugify("Plan: CLI first, MCP later"),
            "plan-cli-first-mcp-later"
        );
        assert_eq!(slugify("<em>Tagged</em> heading"), "tagged-heading");
        assert_eq!(slugify("__dunder__ name"), "dunder-name");
        assert_eq!(slugify("Ünïcode Wörds"), "ünïcode-wörds");
    }

    #[test]
    fn slugify_strips_angle_bracket_destinations() {
        // GitHub renders each heading below as the text "See x" (CommonMark
        // angle-bracket destinations), so the slug is "see-x".
        for heading in [
            "See [x](<a)b.md>)",
            "See [x](<a b.md> \"t\")",
            "See [x](<a)b.md> \"t)\")",
            "See ![x](<a)b.png>)",
            "See ![x](<a b.png> \"t\")",
        ] {
            assert_eq!(slugify(heading), "see-x", "{heading:?}");
        }
        // Unterminated angle destinations fall back to the plain rule.
        assert_eq!(slugify("See [x](<a b) end"), "see-x-end");
    }

    #[test]
    fn anchors_include_html() {
        let anchors =
            anchors_in("---\nt: x\n---\n# Top\n<a name=\"custom\"></a>\n<A ID='Other'>\n");
        assert_eq!(
            anchors.into_iter().collect::<Vec<_>>(),
            ["Other", "custom", "top"]
        );
    }

    #[test]
    fn footnotes_with_continuations() {
        let lines = [
            "Text[^a].",
            "",
            "[^a]: first",
            "    more",
            "[^b]: other",
            "not indented",
            "\tlate",
        ];
        let defs = footnote_defs(&lines);
        assert_eq!(defs["a"], [2, 3]);
        assert_eq!(defs["b"], [4]);
        assert_eq!(defs.len(), 2);
    }

    #[test]
    fn footnote_paragraphs_run_across_blank_lines_but_defs_do_not() {
        let lines = [
            "[^a]: first",
            "",
            "    second",
            "",
            "[^b]: other",
            "",
            "not indented",
            "    code, not b's",
        ];

        let defs = footnote_defs(&lines);
        let paragraphs = footnote_paragraphs(&lines);

        assert_eq!(defs["a"], [0]);
        assert_eq!(paragraphs["a"], [0, 2]);
        // A non-indented line still ends the definition.
        assert_eq!(paragraphs["b"], [4]);
        assert_eq!(defs["b"], [4]);
    }

    #[test]
    fn resolve_link_cases() {
        let ws = MemWorkspace::from_files([("infra/index.md", ""), ("infra/x.md", "")]);
        let resolve = |link: &str, src: &str| resolve_link(&ws, link, src);
        assert_eq!(
            resolve("/infra/x.md#a", "p/q.md"),
            Some(ResolvedLink {
                path: "infra/x.md".into(),
                absolute: true
            })
        );
        assert_eq!(
            resolve("x.md", "infra/index.md"),
            Some(ResolvedLink {
                path: "infra/x.md".into(),
                absolute: false
            })
        );
        assert_eq!(
            resolve("../infra/", "people/y.md").unwrap().path,
            "infra/index.md"
        );
        assert_eq!(resolve("/infra", "y.md").unwrap().path, "infra/index.md");
        assert_eq!(resolve("https://example.com", "y.md"), None);
        assert_eq!(resolve("mailto:a@b", "y.md"), None);
        assert_eq!(resolve("#frag", "y.md"), None);
    }

    #[test]
    fn resolve_link_percent_decodes_the_path_not_the_fragment() {
        let ws = MemWorkspace::from_files([
            ("infra/a b.md", ""),
            ("infra/100%.md", ""),
            ("infra/a%20b.md", ""),
            ("infra/caf\u{e9}.md", ""),
        ]);
        let path = |link: &str| resolve_link(&ws, link, "infra/index.md").map(|r| r.path);
        assert_eq!(path("a%20b.md"), Some("infra/a b.md".into()));
        assert_eq!(path("/infra/a%20b.md#x%20y"), Some("infra/a b.md".into()));
        assert_eq!(path("caf%C3%A9.md"), Some("infra/caf\u{e9}.md".into()));
        // A file literally named `a%20b.md` is written with its `%` encoded.
        assert_eq!(path("a%2520b.md"), Some("infra/a%20b.md".into()));
        // A `%` that starts no escape stays literal.
        assert_eq!(path("100%.md"), Some("infra/100%.md".into()));
        assert_eq!(path("100%25.md"), Some("infra/100%.md".into()));
        // The scheme test reads the destination as written.
        assert_eq!(path("mailto%3Aa"), Some("infra/mailto:a".into()));
        assert_eq!(path("https://example.com/a%20b"), None);
    }

    #[test]
    fn encode_link_path_round_trips_through_resolve_link() {
        let names = [
            "a b.md",
            "100%.md",
            "a%20b.md",
            "x(1)#?.md",
            "caf\u{e9} <q>.md",
        ];
        let owned: Vec<String> = names.iter().map(|name| format!("d/{name}")).collect();
        let ws = MemWorkspace::from_files(owned.iter().map(|path| (path.as_str(), "")));
        for name in names {
            let link = encode_link_path(name);
            assert!(!link.contains(' ') && !link.contains('#'), "{link}");
            // The written destination must still parse as a link.
            let text = format!("[x]({link})");
            let found = find_links(&text);
            assert_eq!(found.len(), 1, "{link}");
            assert_eq!(found[0].target, link.as_ref());
            let back = resolve_link(&ws, &link, "d/index.md").map(|r| r.path);
            assert_eq!(back, Some(format!("d/{name}")), "{link}");
        }
        assert!(matches!(
            encode_link_path("plain/x.md"),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    /// Property: for any concept file name the server's own path check
    /// accepts, the destination `encode_link_path` writes parses as exactly
    /// one link, in both the plain and the angle-bracket form, and
    /// `resolve_link` reads it back to the same file.
    #[hegel::test(test_cases = 200, derandomize = true)]
    fn an_encoded_concept_name_round_trips_through_find_links_and_resolve_link(
        tc: hegel::TestCase,
    ) {
        use hegel::generators;
        // Pieces that broke (or could break) a destination: Unicode
        // whitespace, controls, reserved punctuation, `%` (also as a fake
        // escape), and a URI-scheme lookalike.
        let specials = vec![
            " ",
            "\t",
            "\u{a0}",
            "\u{2003}",
            "\u{3000}",
            "\u{85}",
            "\u{2028}",
            "\n",
            "\r",
            "\u{1c}",
            "\u{1f}",
            "\u{7f}",
            "\0",
            "%",
            "%20",
            "%2",
            "%zz",
            ":",
            "http:",
            "<",
            ">",
            "(",
            ")",
            "[",
            "]",
            "\\",
            "\"",
            "'",
            "#",
            "?",
            "&",
            "+",
            "..",
            ".",
            "\u{e9}",
            "\u{1f600}",
            "\u{202e}",
        ];
        let count = tc.draw(generators::integers::<usize>().max_value(8));
        let mut name = String::new();
        for _ in 0..count {
            if tc.draw(generators::booleans()) {
                name.push_str(tc.draw(generators::sampled_from(&specials)));
            } else {
                name.push_str(&tc.draw(generators::text().max_size(4)));
            }
        }
        name.push_str(".md");
        // A file name is one path segment, and the name must be one the
        // write tools accept (`rules::check_path`) in a plain folder.
        tc.assume(!name.contains('/'));
        let path = format!("d/{name}");
        tc.assume(crate::okf::rules::check_path(&path).is_ok_and(|checked| checked == path));

        let ws = MemWorkspace::from_files([(path.as_str(), "")]);
        let encoded = encode_link_path(&name);
        for destination in [encoded.to_string(), format!("<{encoded}>")] {
            let text = format!("[x]({destination})");
            let found = find_links(&text);
            assert_eq!(found.len(), 1, "{text:?}");
            assert_eq!(found[0].target, encoded.as_ref(), "{text:?}");
            let back = resolve_link(&ws, &found[0].target, "d/index.md").map(|r| r.path);
            assert_eq!(back, Some(path.clone()), "{text:?}");
        }
    }

    #[test]
    fn slugify_follows_python_word_characters() {
        // `²` is a Python word char (category No); a combining mark is not.
        assert_eq!(slugify("a²b"), "a²b");
        assert_eq!(slugify("e\u{301}x"), "ex");
        assert_eq!(slugify("\u{1f}a b\u{1f}"), "a-b");
        assert_eq!(slugify("²_x"), "²_x");
        assert_eq!(slugify("a_\u{301}"), "a");
    }

    #[test]
    fn python_whitespace_extras_count_as_space() {
        let found = headings(&[
            "# \u{1f}pad\u{1f}",
            "\u{1c}```",
            "# in",
            "\u{1c}```",
            "# out",
        ]);
        let summary: Vec<_> = found.iter().map(|h| (h.index, h.text.as_str())).collect();
        assert_eq!(summary, [(0, "pad"), (4, "out")]);
        let (head, body) = body_split("---\na: 1\n---\u{1f}\nb");
        assert_eq!(head, ["---", "a: 1", "---\u{1f}"]);
        assert_eq!(body, ["b"]);
        let links = find_links("[a](b\u{1f}c) [d](e\u{1f}\"t\")");
        let targets: Vec<_> = links.iter().map(|l| l.target.as_str()).collect();
        assert_eq!(targets, ["e"]);
        assert!(anchors_in("<a\u{1f}name=\"zz\">").contains("zz"));
        let defs = footnote_defs(&["[^a]: x", "    \u{1f}", "    more"]);
        assert_eq!(defs["a"], [0]);
    }

    #[test]
    fn pathological_inputs_neither_panic_nor_drop_later_links() {
        // Python finds exactly the trailing link here (checked at 1000 reps).
        let text = format!("{}[ok](x)", "[a](b ".repeat(100_000));
        let targets: Vec<_> = find_links(&text).into_iter().map(|l| l.target).collect();
        assert_eq!(targets, ["x"]);
        // Python: slugify("__a" * n) == "a" + "__a" * (n - 1).
        let expected = format!("a{}", "__a".repeat(99_999));
        assert_eq!(slugify(&"__a".repeat(100_000)), expected);
    }

    #[test]
    fn many_rejected_image_openers_stay_linear() {
        let text = format!("{}[ok](t.md)", "![".repeat(20_000));
        let links = find_links(&text);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "t.md");
        let adjacent = find_links("[a](b)[c](d)");
        assert_eq!(adjacent.len(), 2);
    }

    #[test]
    fn target_range_with_title_and_multibyte_prefix() {
        // Python's character spans (checked) are starts 10 and 27, targets 14..19 and 31..32;
        // `\u{e9}` and `\u{fc}` add one byte each to the byte offsets below.
        let line = "\u{e9} ![x](y) [t](/a.md \"ti\") \u{fc}[k](z)";
        let links = find_links(line);
        assert_eq!(links.len(), 2);
        assert_eq!(
            (links[0].range.start, links[0].target_range.clone()),
            (11, 15..20)
        );
        assert_eq!(&line[links[0].target_range.clone()], "/a.md");
        assert_eq!(&line[links[0].range.clone()], "[t](/a.md \"ti\")");
        assert_eq!(
            (links[1].range.start, links[1].target_range.clone()),
            (29, 33..34)
        );
    }

    #[test]
    fn unclosed_fence_hides_later_headings() {
        let found = headings(&["# a", "```", "# b", "## c"]);
        let texts: Vec<_> = found.iter().map(|h| h.text.as_str()).collect();
        assert_eq!(texts, ["a"]);
    }

    #[test]
    fn slugify_emphasis_table_matches_python() {
        // Expected values come from okf.py's slugify.
        let table = [
            ("_a_", "a"),
            ("a__b", "a__b"),
            ("__init__", "init"),
            ("*a*_b_", "ab"),
            ("a_*_b", "ab"),
            ("a*_b", "ab"),
            ("x_ y", "x-y"),
            ("_", ""),
            ("__", ""),
            ("\u{b2}_x", "\u{b2}_x"),
            ("\u{e9}_", "\u{e9}"),
            ("a_\u{301}", "a"),
            ("`c_d`", "c_d"),
            ("[l_k](u)", "l_k"),
            ("<i>_t_</i>", "t"),
            ("snake_case_", "snake_case"),
        ];
        for (input, expected) in table {
            assert_eq!(slugify(input), expected, "{input:?}");
        }
    }

    #[test]
    fn dotless_and_dotted_i_fold_like_python() {
        let ws = MemWorkspace::from_files([("a.md", "")]);
        // Python: resolve_link("\u{131}map:x") is external, as are the others.
        for link in ["\u{131}map:x", "\u{130}map:x", "\u{212a}ey:x", "\u{17f}:x"] {
            assert_eq!(resolve_link(&ws, link, "y.md"), None, "{link:?}");
        }
        // Python: both are anchors.
        assert!(anchors_in("<a \u{131}d=\"m\">").contains("m"));
        assert!(anchors_in("<a \u{130}d=\"m\">").contains("m"));
    }

    #[hegel::test]
    fn link_ranges_are_consistent(tc: hegel::TestCase) {
        use hegel::generators;
        let text: String = tc.draw(generators::text());
        let wrapped = format!(
            "[\u{e9}]({}) !{text}[a](b) \u{fc}[c](d \"t\")",
            text.replace(')', "")
        );
        for link in find_links(&wrapped) {
            assert_eq!(&wrapped[link.target_range.clone()], link.target);
            assert!(link.range.start < link.target_range.start);
            assert!(link.target_range.end < link.range.end);
        }
    }

    #[hegel::test]
    fn slugify_output_has_no_spaces_or_uppercase_ascii(tc: hegel::TestCase) {
        use hegel::generators;
        let text: String = tc.draw(generators::text());
        let slug = slugify(&text);
        assert!(!slug.contains(' '));
        assert!(!slug.chars().any(|c| c.is_ascii_uppercase()));
    }

    /// The mobile reader relies on this (src/http/read/js/route.js,
    /// `fromBrowse`): a heading slug has no '.', so it never ends in `.md`
    /// and cannot be mistaken for the last segment of a document's path.
    #[hegel::test]
    fn slugify_output_has_no_dot(tc: hegel::TestCase) {
        use hegel::generators;
        let text: String = tc.draw(generators::text());
        assert!(!slugify(&text).contains('.'));
        assert!(!slugify(&format!("{text}.md")).contains('.'));
    }
}
