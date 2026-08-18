use std::time::{Duration, Instant};

use crate::tab::PixelRect;
use crate::terminal::{
    CompiledLiteral, CompiledRegex, MAX_SEARCH_MATCHES, RegexScanCursor, SearchBudget,
    SearchContentId, SearchError, SearchMatch, SearchScanCursor, TerminalCoreAdapter,
};

const SEARCH_DEBOUNCE: Duration = Duration::from_millis(50);
const SEARCH_SLICE: Duration = Duration::from_millis(2);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchDialogPresentation {
    pub panel: PixelRect,
    pub input: PixelRect,
    pub previous: PixelRect,
    pub next: PixelRect,
    pub query_text: String,
}

impl SearchDialogPresentation {
    #[must_use]
    pub fn layout(viewport: [u32; 2], scale_120: u32, query_text: String) -> Self {
        let scale = |value: u32| value.saturating_mul(scale_120).div_ceil(120);
        let margin = scale(12).min(viewport[0] / 2).min(viewport[1] / 2);
        let width = scale(180).min(viewport[0].saturating_sub(margin.saturating_mul(2)));
        let height = scale(44).min(viewport[1].saturating_sub(margin.saturating_mul(2)));
        let panel = PixelRect {
            x: viewport[0].saturating_sub(width) / 2,
            y: viewport[1].saturating_sub(height) / 4,
            width,
            height,
        };
        let inner = scale(6).min(width / 4).min(height / 4);
        let row = height.saturating_sub(inner.saturating_mul(2));
        let button = scale(28).min(row);
        let input_y = panel.y.saturating_add(inner);
        let input = PixelRect {
            x: panel.x.saturating_add(inner),
            y: input_y,
            width: panel
                .width
                .saturating_sub(inner.saturating_mul(2))
                .saturating_sub(button.saturating_mul(2)),
            height: row,
        };
        let previous = PixelRect {
            x: input.x.saturating_add(input.width),
            y: input_y,
            width: button,
            height: row,
        };
        let next = PixelRect {
            x: previous.x.saturating_add(previous.width),
            y: input_y,
            width: button,
            height: row,
        };
        Self {
            panel,
            input,
            previous,
            next,
            query_text,
        }
    }

    pub fn move_to(&mut self, origin: [u32; 2], viewport: [u32; 2]) {
        let target = [
            origin[0].min(viewport[0].saturating_sub(self.panel.width)),
            origin[1].min(viewport[1].saturating_sub(self.panel.height)),
        ];
        let offset = [
            i64::from(target[0]) - i64::from(self.panel.x),
            i64::from(target[1]) - i64::from(self.panel.y),
        ];
        for rect in [
            &mut self.panel,
            &mut self.input,
            &mut self.previous,
            &mut self.next,
        ] {
            rect.x = u32::try_from(i64::from(rect.x) + offset[0]).unwrap_or(0);
            rect.y = u32::try_from(i64::from(rect.y) + offset[1]).unwrap_or(0);
        }
    }

    #[must_use]
    pub fn drag_hit_test(&self, point: [u32; 2], viewport: [u32; 2], outset: u32) -> bool {
        let x = self.panel.x.saturating_sub(outset);
        let y = self.panel.y.saturating_sub(outset);
        let right = self
            .panel
            .x
            .saturating_add(self.panel.width)
            .saturating_add(outset)
            .min(viewport[0]);
        let bottom = self
            .panel
            .y
            .saturating_add(self.panel.height)
            .saturating_add(outset)
            .min(viewport[1]);
        PixelRect {
            x,
            y,
            width: right.saturating_sub(x),
            height: bottom.saturating_sub(y),
        }
        .contains(point)
            && !self.input.contains(point)
            && !self.previous.contains(point)
            && !self.next.contains(point)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SearchPhase {
    #[default]
    Empty,
    Debouncing,
    Scanning,
    Ready,
    Truncated,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchDirection {
    Next,
    Previous,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchEdit<'a> {
    Insert(&'a str),
    Backspace,
    Delete,
    Left,
    Right,
    Home,
    End,
    DeleteSurrounding { before_bytes: u32, after_bytes: u32 },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SearchEffect {
    pub needs_frame: bool,
    pub needs_deadline: bool,
    pub scroll_target: Option<usize>,
    pub ime_changed: bool,
}

#[derive(Debug, Default)]
pub struct SearchController {
    open: bool,
    query: String,
    regex_enabled: bool,
    cursor_byte: usize,
    revision: u64,
    phase: SearchPhase,
    editor_notice: Option<SearchError>,
    bound_content: Option<SearchContentId>,
    compiled: Option<CompiledSearch>,
    matches: Vec<SearchMatch>,
    current: Option<SearchMatch>,
    current_saved_index: Option<usize>,
    scan: Option<ControllerScanCursor>,
    deadline: Option<Instant>,
    pending_navigation: Option<SearchDirection>,
    navigation_scan: Option<NavigationScan>,
}

#[derive(Debug)]
struct NavigationScan {
    direction: SearchDirection,
    origin: SearchAnchorKey,
    cursor: ControllerScanCursor,
    first: Option<SearchMatch>,
    last: Option<SearchMatch>,
    directional: Option<SearchMatch>,
}

#[derive(Debug)]
enum CompiledSearch {
    Literal(CompiledLiteral),
    Regex(CompiledRegex),
}

impl CompiledSearch {
    fn allocated_bytes(&self) -> usize {
        match self {
            Self::Literal(value) => value.allocated_bytes(),
            Self::Regex(value) => value.allocated_bytes(),
        }
    }
}

#[derive(Clone, Debug)]
enum ControllerScanCursor {
    Literal(SearchScanCursor),
    Regex(RegexScanCursor),
}

impl ControllerScanCursor {
    fn allocated_bytes(&self) -> usize {
        match self {
            Self::Literal(value) => value.allocated_bytes(),
            Self::Regex(value) => value.allocated_bytes(),
        }
    }
}

struct ControllerScanStep {
    matches: Vec<SearchMatch>,
    next: Option<ControllerScanCursor>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct SearchAnchorKey(i32, u16, u8);

impl From<crate::terminal::SearchAnchor> for SearchAnchorKey {
    fn from(value: crate::terminal::SearchAnchor) -> Self {
        Self(value.history_line, value.column, value.scalar_offset)
    }
}

impl SearchController {
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open
    }

    #[must_use]
    pub fn query(&self) -> &str {
        &self.query
    }

    #[must_use]
    pub const fn cursor_byte(&self) -> usize {
        self.cursor_byte
    }

    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    #[must_use]
    pub const fn phase(&self) -> SearchPhase {
        self.phase
    }

    #[must_use]
    pub fn matches(&self) -> &[SearchMatch] {
        &self.matches
    }

    #[must_use]
    pub fn allocated_bytes(&self) -> usize {
        let compiled = self
            .compiled
            .as_ref()
            .map_or(0, CompiledSearch::allocated_bytes);
        let cursors = self
            .scan
            .as_ref()
            .map_or(0, ControllerScanCursor::allocated_bytes)
            .saturating_add(
                self.navigation_scan
                    .as_ref()
                    .map_or(0, |value| value.cursor.allocated_bytes()),
            );
        self.query
            .capacity()
            .saturating_add(
                self.matches
                    .capacity()
                    .saturating_mul(size_of::<SearchMatch>()),
            )
            .saturating_add(compiled)
            .saturating_add(cursors)
    }

    #[must_use]
    pub const fn current(&self) -> Option<SearchMatch> {
        self.current
    }

    #[must_use]
    pub const fn current_saved_index(&self) -> Option<usize> {
        self.current_saved_index
    }

    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> {
        if self.phase == SearchPhase::Scanning || self.navigation_scan.is_some() {
            Some(Instant::now())
        } else {
            self.deadline
        }
    }

    #[must_use]
    pub fn status(&self) -> String {
        match self.phase {
            SearchPhase::Empty => String::new(),
            SearchPhase::Debouncing | SearchPhase::Scanning => "Searching...".into(),
            SearchPhase::Failed => match self.editor_notice {
                Some(SearchError::InvalidRegex) => "Invalid regular expression".into(),
                Some(SearchError::RegexLineTooLong) => "Logical line is too long".into(),
                _ => "Search failed".into(),
            },
            SearchPhase::Truncated => format!(">={MAX_SEARCH_MATCHES}"),
            SearchPhase::Ready if self.matches.is_empty() => "Not found".into(),
            SearchPhase::Ready => self.current_saved_index.map_or_else(
                || self.matches.len().to_string(),
                |index| format!("{}/{}", index + 1, self.matches.len()),
            ),
        }
    }

    pub fn open(&mut self) -> SearchEffect {
        self.bump_revision();
        let reopening = !self.open && !self.query.is_empty();
        self.open = true;
        self.regex_enabled = true;
        self.cursor_byte = self.query.len();
        if reopening {
            self.phase = SearchPhase::Debouncing;
            self.deadline = Some(Instant::now());
        }
        SearchEffect {
            needs_frame: true,
            needs_deadline: reopening,
            ime_changed: true,
            ..SearchEffect::default()
        }
    }

    pub fn cancel(&mut self) -> SearchEffect {
        self.bump_revision();
        self.open = false;
        self.phase = SearchPhase::Empty;
        self.editor_notice = None;
        self.bound_content = None;
        self.compiled = None;
        self.matches.clear();
        self.current = None;
        self.current_saved_index = None;
        self.scan = None;
        self.deadline = None;
        self.pending_navigation = None;
        self.navigation_scan = None;
        SearchEffect {
            needs_frame: true,
            ime_changed: true,
            ..SearchEffect::default()
        }
    }

    pub fn edit(&mut self, edit: SearchEdit<'_>, now: Instant) -> SearchEffect {
        if !self.open {
            return SearchEffect::default();
        }
        let old_query = self.query.clone();
        let old_cursor = self.cursor_byte;
        let query_changed = match edit {
            SearchEdit::Insert(value) => {
                let mut candidate = self.query.clone();
                candidate.insert_str(self.cursor_byte, value);
                match validate_query(&candidate) {
                    Ok(()) => {
                        self.query = candidate;
                        self.cursor_byte += value.len();
                        true
                    }
                    Err(error) => {
                        self.editor_notice = Some(error);
                        false
                    }
                }
            }
            SearchEdit::Backspace => self.previous_boundary().is_some_and(|start| {
                self.query.drain(start..self.cursor_byte);
                self.cursor_byte = start;
                true
            }),
            SearchEdit::Delete => self.next_boundary().is_some_and(|end| {
                self.query.drain(self.cursor_byte..end);
                true
            }),
            SearchEdit::Left => {
                if let Some(value) = self.previous_boundary() {
                    self.cursor_byte = value;
                }
                false
            }
            SearchEdit::Right => {
                if let Some(value) = self.next_boundary() {
                    self.cursor_byte = value;
                }
                false
            }
            SearchEdit::Home => {
                self.cursor_byte = 0;
                false
            }
            SearchEdit::End => {
                self.cursor_byte = self.query.len();
                false
            }
            SearchEdit::DeleteSurrounding {
                before_bytes,
                after_bytes,
            } => {
                let range = usize::try_from(before_bytes)
                    .ok()
                    .and_then(|before| self.cursor_byte.checked_sub(before))
                    .zip(
                        usize::try_from(after_bytes)
                            .ok()
                            .and_then(|after| self.cursor_byte.checked_add(after)),
                    );
                if let Some((start, end)) = range
                    && end <= self.query.len()
                    && self.query.is_char_boundary(start)
                    && self.query.is_char_boundary(end)
                {
                    self.query.drain(start..end);
                    self.cursor_byte = start;
                    true
                } else {
                    self.editor_notice = Some(SearchError::InvalidCoordinate);
                    false
                }
            }
        };
        if self.query != old_query || self.cursor_byte != old_cursor {
            self.bump_revision();
        }
        if query_changed {
            self.editor_notice = None;
            self.reset_results();
            if self.query.is_empty() {
                self.phase = SearchPhase::Empty;
            } else {
                self.phase = SearchPhase::Debouncing;
                self.deadline = Some(now + SEARCH_DEBOUNCE);
            }
        }
        SearchEffect {
            needs_frame: self.query != old_query
                || self.cursor_byte != old_cursor
                || self.editor_notice.is_some(),
            needs_deadline: query_changed && !self.query.is_empty(),
            ime_changed: self.query != old_query || self.cursor_byte != old_cursor,
            ..SearchEffect::default()
        }
    }

    pub fn invalidate(&mut self, now: Instant) {
        if !self.open || self.query.is_empty() {
            return;
        }
        self.reset_results();
        self.phase = SearchPhase::Debouncing;
        self.deadline = Some(now + SEARCH_DEBOUNCE);
    }

    pub fn navigate(
        &mut self,
        direction: SearchDirection,
        core: &TerminalCoreAdapter,
        now: Instant,
    ) -> SearchEffect {
        if !self.open || self.query.is_empty() {
            return SearchEffect::default();
        }
        if matches!(self.phase, SearchPhase::Debouncing | SearchPhase::Empty) {
            self.deadline = Some(now);
            self.pending_navigation = Some(direction);
            return SearchEffect {
                needs_frame: true,
                needs_deadline: true,
                ..SearchEffect::default()
            };
        }
        if self.matches.is_empty() {
            self.pending_navigation = Some(direction);
            return SearchEffect {
                needs_deadline: self.scan.is_some(),
                ..SearchEffect::default()
            };
        }
        if matches!(self.phase, SearchPhase::Scanning | SearchPhase::Truncated) {
            let origin = self
                .current
                .map_or(SearchAnchorKey(i32::MIN, 0, 0), |value| value.start.into());
            self.navigation_scan = Some(NavigationScan {
                direction,
                origin,
                cursor: if self.regex_enabled {
                    ControllerScanCursor::Regex(core.regex_scan_cursor())
                } else {
                    ControllerScanCursor::Literal(core.search_scan_cursor())
                },
                first: None,
                last: None,
                directional: None,
            });
            return SearchEffect {
                needs_frame: true,
                needs_deadline: true,
                ..SearchEffect::default()
            };
        }
        let current = self.current_saved_index.unwrap_or_else(|| match direction {
            SearchDirection::Next => self.matches.len() - 1,
            SearchDirection::Previous => 0,
        });
        let next = match direction {
            SearchDirection::Next => (current + 1) % self.matches.len(),
            SearchDirection::Previous => (current + self.matches.len() - 1) % self.matches.len(),
        };
        self.set_current(next, core)
    }

    #[allow(clippy::too_many_lines)]
    pub fn advance(&mut self, core: &TerminalCoreAdapter, now: Instant) -> SearchEffect {
        if !self.open || self.query.is_empty() {
            return SearchEffect::default();
        }
        if self.phase == SearchPhase::Debouncing {
            if self.deadline.is_some_and(|deadline| now < deadline) {
                return SearchEffect {
                    needs_deadline: true,
                    ..SearchEffect::default()
                };
            }
            let compiled = if self.regex_enabled {
                core.compile_regex_search(&self.query)
                    .map(CompiledSearch::Regex)
            } else {
                core.compile_literal_search(&self.query)
                    .map(CompiledSearch::Literal)
            };
            match compiled {
                Ok(compiled) => {
                    self.compiled = Some(compiled);
                    self.bound_content = Some(core.search_content_id());
                    self.scan = Some(if self.regex_enabled {
                        ControllerScanCursor::Regex(core.regex_scan_cursor())
                    } else {
                        ControllerScanCursor::Literal(core.search_scan_cursor())
                    });
                    self.phase = SearchPhase::Scanning;
                    self.deadline = None;
                }
                Err(error) => {
                    self.editor_notice = Some(error);
                    self.phase = SearchPhase::Failed;
                    return SearchEffect {
                        needs_frame: true,
                        ..SearchEffect::default()
                    };
                }
            }
        }
        if self.bound_content != Some(core.search_content_id()) {
            self.invalidate(now);
            return SearchEffect {
                needs_frame: true,
                needs_deadline: true,
                ..SearchEffect::default()
            };
        }
        if self.navigation_scan.is_some() {
            return self.advance_navigation(core, now);
        }
        let (Some(compiled), Some(cursor)) = (self.compiled.as_ref(), self.scan.take()) else {
            return SearchEffect::default();
        };
        let remaining = MAX_SEARCH_MATCHES.saturating_sub(self.matches.len());
        let step = match scan_controller_slice(
            core,
            compiled,
            cursor,
            SearchBudget {
                deadline: now + SEARCH_SLICE,
                max_rows: crate::terminal::MAX_SEARCH_SLICE_ROWS,
                max_matches: remaining.max(1),
                clock: Instant::now,
            },
        ) {
            Ok(step) => step,
            Err(SearchError::StaleContent) => {
                self.invalidate(now);
                return SearchEffect {
                    needs_frame: true,
                    needs_deadline: true,
                    ..SearchEffect::default()
                };
            }
            Err(error) => {
                self.editor_notice = Some(error);
                self.phase = SearchPhase::Failed;
                return SearchEffect {
                    needs_frame: true,
                    ..SearchEffect::default()
                };
            }
        };
        if self.matches.try_reserve(step.matches.len()).is_err() {
            self.phase = SearchPhase::Truncated;
            return SearchEffect {
                needs_frame: true,
                ..SearchEffect::default()
            };
        }
        self.matches.extend(step.matches);
        if self.current_saved_index.is_none()
            && let Some(current) = self.current
        {
            self.current_saved_index = self
                .matches
                .binary_search_by_key(&SearchAnchorKey::from(current.start), |candidate| {
                    SearchAnchorKey::from(candidate.start)
                })
                .ok();
        }
        if self.matches.len() >= MAX_SEARCH_MATCHES {
            self.matches.truncate(MAX_SEARCH_MATCHES);
            self.scan = None;
            self.phase = SearchPhase::Truncated;
        } else {
            self.scan = step.next;
            self.phase = if self.scan.is_some() {
                SearchPhase::Scanning
            } else {
                SearchPhase::Ready
            };
        }
        let mut effect = SearchEffect {
            needs_frame: true,
            needs_deadline: self.scan.is_some(),
            ..SearchEffect::default()
        };
        if self.current.is_none()
            && !self.matches.is_empty()
            && (self.pending_navigation.is_some() || self.scan.is_none())
        {
            let direction = self
                .pending_navigation
                .take()
                .unwrap_or(SearchDirection::Next);
            let visible = self
                .matches
                .iter()
                .position(|value| core.project_search_match(*value).is_ok());
            let index = visible.unwrap_or_else(|| match direction {
                SearchDirection::Next => 0,
                SearchDirection::Previous => self.matches.len() - 1,
            });
            effect = merge_effect(effect, self.set_current(index, core));
        }
        effect
    }

    fn set_current(&mut self, index: usize, core: &TerminalCoreAdapter) -> SearchEffect {
        let value = self.matches[index];
        self.current = Some(value);
        self.current_saved_index = Some(index);
        SearchEffect {
            needs_frame: true,
            scroll_target: core.search_display_offset(value).ok(),
            ..SearchEffect::default()
        }
    }

    fn reset_results(&mut self) {
        self.bound_content = None;
        self.compiled = None;
        self.matches.clear();
        self.current = None;
        self.current_saved_index = None;
        self.scan = None;
        self.pending_navigation = None;
        self.navigation_scan = None;
    }

    fn previous_boundary(&self) -> Option<usize> {
        self.query[..self.cursor_byte]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
    }

    fn next_boundary(&self) -> Option<usize> {
        self.query[self.cursor_byte..]
            .chars()
            .next()
            .map(|value| self.cursor_byte + value.len_utf8())
    }

    fn bump_revision(&mut self) {
        match self.revision.checked_add(1) {
            Some(revision) => self.revision = revision,
            None => self.phase = SearchPhase::Failed,
        }
    }

    fn advance_navigation(&mut self, core: &TerminalCoreAdapter, now: Instant) -> SearchEffect {
        let Some(mut navigation) = self.navigation_scan.take() else {
            return SearchEffect::default();
        };
        let Some(compiled) = self.compiled.as_ref() else {
            return SearchEffect::default();
        };
        let step = match scan_controller_slice(
            core,
            compiled,
            navigation.cursor,
            SearchBudget {
                deadline: now + SEARCH_SLICE,
                max_rows: crate::terminal::MAX_SEARCH_SLICE_ROWS,
                max_matches: 1_024,
                clock: Instant::now,
            },
        ) {
            Ok(step) => step,
            Err(SearchError::StaleContent) => {
                self.invalidate(now);
                return SearchEffect {
                    needs_frame: true,
                    needs_deadline: true,
                    ..SearchEffect::default()
                };
            }
            Err(error) => {
                self.editor_notice = Some(error);
                self.phase = SearchPhase::Failed;
                return SearchEffect {
                    needs_frame: true,
                    ..SearchEffect::default()
                };
            }
        };
        for value in step.matches {
            navigation.first.get_or_insert(value);
            navigation.last = Some(value);
            let key = SearchAnchorKey::from(value.start);
            match navigation.direction {
                SearchDirection::Next if key > navigation.origin => {
                    navigation.directional.get_or_insert(value);
                }
                SearchDirection::Previous if key < navigation.origin => {
                    navigation.directional = Some(value);
                }
                SearchDirection::Next | SearchDirection::Previous => {}
            }
        }
        if let Some(cursor) = step.next {
            navigation.cursor = cursor;
            self.navigation_scan = Some(navigation);
            return SearchEffect {
                needs_frame: true,
                needs_deadline: true,
                ..SearchEffect::default()
            };
        }
        let value = navigation.directional.or(match navigation.direction {
            SearchDirection::Next => navigation.first,
            SearchDirection::Previous => navigation.last,
        });
        let Some(value) = value else {
            return SearchEffect {
                needs_frame: true,
                ..SearchEffect::default()
            };
        };
        self.current = Some(value);
        self.current_saved_index = self
            .matches
            .binary_search_by_key(&SearchAnchorKey::from(value.start), |candidate| {
                SearchAnchorKey::from(candidate.start)
            })
            .ok();
        SearchEffect {
            needs_frame: true,
            scroll_target: core.search_display_offset(value).ok(),
            ..SearchEffect::default()
        }
    }
}

fn validate_query(query: &str) -> Result<(), SearchError> {
    if query.len() > crate::terminal::MAX_SEARCH_QUERY_BYTES
        || query.chars().count() > crate::terminal::MAX_SEARCH_QUERY_SCALARS
    {
        return Err(SearchError::QueryTooLong);
    }
    if query.chars().any(char::is_control) {
        return Err(SearchError::ControlCharacter);
    }
    Ok(())
}

fn scan_controller_slice(
    core: &TerminalCoreAdapter,
    compiled: &CompiledSearch,
    cursor: ControllerScanCursor,
    budget: SearchBudget,
) -> Result<ControllerScanStep, SearchError> {
    match (compiled, cursor) {
        (CompiledSearch::Literal(compiled), ControllerScanCursor::Literal(cursor)) => {
            let step = core.scan_search_slice(compiled, cursor, budget)?;
            Ok(ControllerScanStep {
                matches: step.matches,
                next: step.next.map(ControllerScanCursor::Literal),
            })
        }
        (CompiledSearch::Regex(compiled), ControllerScanCursor::Regex(cursor)) => {
            let step = core.scan_regex_slice(compiled, cursor, budget)?;
            Ok(ControllerScanStep {
                matches: step.matches,
                next: step.next.map(ControllerScanCursor::Regex),
            })
        }
        (CompiledSearch::Literal(_), ControllerScanCursor::Regex(_))
        | (CompiledSearch::Regex(_), ControllerScanCursor::Literal(_)) => {
            Err(SearchError::StaleContent)
        }
    }
}

fn merge_effect(left: SearchEffect, right: SearchEffect) -> SearchEffect {
    SearchEffect {
        needs_frame: left.needs_frame || right.needs_frame,
        needs_deadline: left.needs_deadline || right.needs_deadline,
        scroll_target: right.scroll_target.or(left.scroll_target),
        ime_changed: left.ime_changed || right.ime_changed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::GridSize;

    #[test]
    fn editor_moves_on_utf8_boundaries_and_rejects_atomic_invalid_edits() {
        let mut search = SearchController::default();
        let now = Instant::now();
        search.open();
        search.edit(SearchEdit::Insert("a中"), now);
        assert_eq!(search.cursor_byte(), 4);
        search.edit(SearchEdit::Left, now);
        assert_eq!(search.cursor_byte(), 1);
        search.edit(SearchEdit::Backspace, now);
        assert_eq!(search.query(), "中");
        search.edit(SearchEdit::Insert("\nno"), now);
        assert_eq!(search.query(), "中");
    }

    #[test]
    fn controller_scans_navigates_and_invalidates_per_content_revision() {
        let mut core = TerminalCoreAdapter::new(GridSize::new(8, 2).unwrap(), 10).unwrap();
        core.advance(b"one one").unwrap();
        let mut search = SearchController::default();
        let now = Instant::now();
        search.open();
        search.edit(SearchEdit::Insert("one"), now);
        let effect = search.advance(&core, now + SEARCH_DEBOUNCE);
        assert_eq!(search.phase(), SearchPhase::Ready);
        assert_eq!(search.matches().len(), 2);
        assert_eq!(effect.scroll_target, Some(0));
        search.navigate(SearchDirection::Next, &core, now);
        assert_eq!(search.current_saved_index(), Some(1));
        core.advance(b"x").unwrap();
        search.advance(&core, now);
        assert_eq!(search.phase(), SearchPhase::Debouncing);
        assert!(search.matches().is_empty());
    }

    #[test]
    fn truncated_navigation_can_move_beyond_the_saved_match_set() {
        let mut core = TerminalCoreAdapter::new(GridSize::new(12, 2).unwrap(), 10).unwrap();
        core.advance(b"x x x x").unwrap();
        let mut search = SearchController::default();
        let now = Instant::now();
        search.open();
        search.edit(SearchEdit::Insert("x"), now);
        search.advance(&core, now + SEARCH_DEBOUNCE);
        let first = search.matches[0];
        search.matches.truncate(1);
        search.current = Some(first);
        search.current_saved_index = Some(0);
        search.phase = SearchPhase::Truncated;
        search.navigate(SearchDirection::Next, &core, now);
        while search.navigation_scan.is_some() {
            search.advance(&core, Instant::now());
        }
        assert!(SearchAnchorKey::from(search.current.unwrap().start) > first.start.into());
        assert_eq!(search.current_saved_index, None);
    }

    #[test]
    fn dense_matches_stop_at_the_result_and_memory_limits() {
        let core = TerminalCoreAdapter::new(GridSize::new(512, 256).unwrap(), 0).unwrap();
        let mut search = SearchController::default();
        let now = Instant::now();
        search.open();
        search.edit(SearchEdit::Insert(" "), now);
        search.advance(&core, now + SEARCH_DEBOUNCE);
        assert_eq!(search.phase(), SearchPhase::Truncated);
        assert_eq!(search.matches().len(), MAX_SEARCH_MATCHES);
        assert!(search.allocated_bytes() <= 1024 * 1024);
    }

    #[test]
    fn dialog_search_compiles_regex_reports_errors_and_matches_history() {
        let mut core = TerminalCoreAdapter::new(GridSize::new(16, 2).unwrap(), 10).unwrap();
        core.advance(b"foo1 fooA foo22").unwrap();
        let mut search = SearchController::default();
        let now = Instant::now();
        search.open();
        search.edit(SearchEdit::Insert(r"foo\d+"), now);
        for _ in 0..8 {
            search.advance(&core, now + SEARCH_DEBOUNCE);
            if search.phase() == SearchPhase::Ready {
                break;
            }
        }
        assert_eq!(search.phase(), SearchPhase::Ready);
        assert_eq!(search.matches().len(), 2);
        assert!(search.allocated_bytes() <= 1024 * 1024);

        let mut invalid = SearchController::default();
        invalid.open();
        invalid.edit(SearchEdit::Insert("["), now);
        invalid.advance(&core, now + SEARCH_DEBOUNCE);
        assert_eq!(invalid.phase(), SearchPhase::Failed);
        assert_eq!(invalid.status(), "Invalid regular expression");
    }

    #[test]
    fn dialog_layout_stays_inside_narrow_viewports() {
        let mut dialog = SearchDialogPresentation::layout([240, 120], 150, "query".into());
        assert!(dialog.panel.x + dialog.panel.width <= 240);
        assert!(dialog.panel.y + dialog.panel.height <= 120);
        assert!(dialog.panel.contains([dialog.input.x, dialog.input.y]));
        assert!(
            dialog
                .panel
                .contains([dialog.previous.x, dialog.previous.y])
        );
        assert!(dialog.panel.contains([dialog.next.x, dialog.next.y]));
        assert!(dialog.panel.height < 100);
        dialog.move_to([u32::MAX, u32::MAX], [240, 120]);
        assert_eq!(dialog.panel.x + dialog.panel.width, 240);
        assert_eq!(dialog.panel.y + dialog.panel.height, 120);
        assert!(dialog.panel.contains([dialog.input.x, dialog.input.y]));

        let regular = SearchDialogPresentation::layout([800, 500], 120, String::new());
        assert_eq!(regular.panel.width, 180);
        assert_eq!(regular.panel.x, 310);
        assert_eq!(regular.panel.y, 114);
        assert!(regular.drag_hit_test([306, 120], [800, 500], 8));
        assert!(!regular.drag_hit_test([300, 120], [800, 500], 8));
        assert!(!regular.drag_hit_test([regular.input.x, regular.input.y], [800, 500], 8));
    }
}
