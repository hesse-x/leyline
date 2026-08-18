use std::{collections::VecDeque, time::Instant};

use super::{GridSize, SearchBuffer};

pub const MAX_SEARCH_QUERY_SCALARS: usize = 256;
pub const MAX_SEARCH_QUERY_BYTES: usize = 1_024;
pub const MAX_SEARCH_MATCHES: usize = 10_000;
pub const MAX_SEARCH_SLICE_ROWS: usize = 256;
pub const MAX_REGEX_LOGICAL_LINE_BYTES: usize = 64 * 1024;
const REGEX_ENGINE_BUDGET: usize = 384 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SearchContentId {
    pub content_revision: u64,
    pub columns: u16,
    pub lines: u16,
    pub buffer: SearchBuffer,
}

impl SearchContentId {
    pub(super) fn new(content_revision: u64, grid: GridSize, buffer: SearchBuffer) -> Self {
        Self {
            content_revision,
            columns: grid.columns.get(),
            lines: grid.lines.get(),
            buffer,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SearchAnchor {
    pub history_line: i32,
    pub column: u16,
    pub scalar_offset: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SearchMatch {
    pub start: SearchAnchor,
    pub end: SearchAnchor,
}

#[derive(Clone, Debug)]
pub struct CompiledLiteral {
    pub(super) pattern: Vec<char>,
    pub(super) prefix: Vec<usize>,
}

#[derive(Debug)]
pub struct CompiledRegex {
    pub(super) regex: regex_automata::meta::Regex,
}

impl CompiledRegex {
    #[must_use]
    pub const fn allocated_bytes(&self) -> usize {
        REGEX_ENGINE_BUDGET
    }
}

impl CompiledLiteral {
    #[must_use]
    pub fn scalar_len(&self) -> usize {
        self.pattern.len()
    }

    #[must_use]
    pub fn allocated_bytes(&self) -> usize {
        self.pattern
            .capacity()
            .saturating_mul(size_of::<char>())
            .saturating_add(self.prefix.capacity().saturating_mul(size_of::<usize>()))
    }
}

#[derive(Clone, Debug)]
pub struct SearchScanCursor {
    pub(super) content: SearchContentId,
    pub(super) line: i32,
    pub(super) column: usize,
    pub(super) scalar_offset: usize,
    pub(super) progress: usize,
    pub(super) recent: VecDeque<SearchAnchor>,
}

#[derive(Clone, Debug)]
pub struct RegexScanCursor {
    pub(super) content: SearchContentId,
    pub(super) line: i32,
    pub(super) column: usize,
    pub(super) scalar_offset: usize,
    pub(super) text: String,
    pub(super) anchors: Vec<SearchAnchor>,
    pub(super) byte_offsets: Vec<usize>,
    pub(super) matching: bool,
    pub(super) match_offset: usize,
}

impl RegexScanCursor {
    #[must_use]
    pub const fn content_id(&self) -> SearchContentId {
        self.content
    }

    #[must_use]
    pub fn allocated_bytes(&self) -> usize {
        self.text
            .capacity()
            .saturating_add(
                self.anchors
                    .capacity()
                    .saturating_mul(size_of::<SearchAnchor>()),
            )
            .saturating_add(
                self.byte_offsets
                    .capacity()
                    .saturating_mul(size_of::<usize>()),
            )
    }
}

impl SearchScanCursor {
    #[must_use]
    pub const fn content_id(&self) -> SearchContentId {
        self.content
    }

    #[must_use]
    pub fn allocated_bytes(&self) -> usize {
        self.recent
            .capacity()
            .saturating_mul(size_of::<SearchAnchor>())
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SearchBudget {
    pub deadline: Instant,
    pub max_rows: usize,
    pub max_matches: usize,
    pub clock: fn() -> Instant,
}

impl SearchBudget {
    #[must_use]
    pub fn new(deadline: Instant) -> Self {
        Self {
            deadline,
            max_rows: MAX_SEARCH_SLICE_ROWS,
            max_matches: MAX_SEARCH_MATCHES,
            clock: Instant::now,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SearchScanStep {
    pub content: SearchContentId,
    pub matches: Vec<SearchMatch>,
    pub next: Option<SearchScanCursor>,
    pub scanned_rows: usize,
}

#[derive(Clone, Debug)]
pub struct RegexScanStep {
    pub content: SearchContentId,
    pub matches: Vec<SearchMatch>,
    pub next: Option<RegexScanCursor>,
    pub scanned_rows: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SearchProjection {
    pub start: [u16; 2],
    pub end: [u16; 2],
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum SearchError {
    #[error("search query is empty")]
    EmptyQuery,
    #[error("search query exceeds its resource limit")]
    QueryTooLong,
    #[error("search query contains a control character")]
    ControlCharacter,
    #[error("search content changed while scanning")]
    StaleContent,
    #[error("search allocation failed")]
    Allocation,
    #[error("search coordinate is outside the terminal grid")]
    InvalidCoordinate,
    #[error("invalid regular expression")]
    InvalidRegex,
    #[error("a soft-wrapped logical line exceeds the regex search limit")]
    RegexLineTooLong,
}

pub(super) fn compile(query: &str) -> Result<CompiledLiteral, SearchError> {
    if query.is_empty() {
        return Err(SearchError::EmptyQuery);
    }
    if query.len() > MAX_SEARCH_QUERY_BYTES {
        return Err(SearchError::QueryTooLong);
    }
    let scalar_count = query.chars().count();
    if scalar_count > MAX_SEARCH_QUERY_SCALARS {
        return Err(SearchError::QueryTooLong);
    }
    if query.chars().any(char::is_control) {
        return Err(SearchError::ControlCharacter);
    }
    let mut pattern = Vec::new();
    pattern
        .try_reserve_exact(scalar_count)
        .map_err(|_| SearchError::Allocation)?;
    pattern.extend(query.chars());
    let mut prefix = Vec::new();
    prefix
        .try_reserve_exact(scalar_count)
        .map_err(|_| SearchError::Allocation)?;
    prefix.resize(scalar_count, 0);
    let mut matched = 0;
    for index in 1..pattern.len() {
        while matched > 0 && pattern[index] != pattern[matched] {
            matched = prefix[matched - 1];
        }
        if pattern[index] == pattern[matched] {
            matched += 1;
        }
        prefix[index] = matched;
    }
    Ok(CompiledLiteral { pattern, prefix })
}

pub(super) fn compile_regex(query: &str) -> Result<CompiledRegex, SearchError> {
    if query.is_empty() {
        return Err(SearchError::EmptyQuery);
    }
    if query.len() > MAX_SEARCH_QUERY_BYTES || query.chars().count() > MAX_SEARCH_QUERY_SCALARS {
        return Err(SearchError::QueryTooLong);
    }
    if query.chars().any(char::is_control) {
        return Err(SearchError::ControlCharacter);
    }
    let regex = regex_automata::meta::Regex::builder()
        .configure(
            regex_automata::meta::Regex::config()
                .nfa_size_limit(Some(256 * 1024))
                .dfa_size_limit(Some(64 * 1024))
                .hybrid_cache_capacity(64 * 1024)
                .pool_capacity(1),
        )
        .build(query)
        .map_err(|_| SearchError::InvalidRegex)?;
    Ok(CompiledRegex { regex })
}
