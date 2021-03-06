//! A specialized 2D grid implementation optimized for use in a terminal.

use std::cmp::{max, min};
use std::ops::{Deref, Index, IndexMut, Range, RangeFrom, RangeFull, RangeInclusive, RangeTo};

use serde::{Deserialize, Serialize};

use crate::ansi::{CharsetIndex, StandardCharset};
use crate::index::{Column, IndexRange, Line, Point};
use crate::term::cell::{Flags, ResetDiscriminant};

pub mod resize;
mod row;
mod storage;
#[cfg(test)]
mod tests;

pub use self::row::Row;
use self::storage::Storage;

/// Bidirectional iterator.
pub trait BidirectionalIterator: Iterator {
    fn prev(&mut self) -> Option<Self::Item>;
}

/// An item in the grid along with its Line and Column.
pub struct Indexed<T> {
    pub inner: T,
    pub line: Line,
    pub column: Column,
}

impl<T> Deref for Indexed<T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T: PartialEq> ::std::cmp::PartialEq for Grid<T> {
    fn eq(&self, other: &Self) -> bool {
        // Compare struct fields and check result of grid comparison.
        self.raw.eq(&other.raw)
            && self.cols.eq(&other.cols)
            && self.lines.eq(&other.lines)
            && self.display_offset.eq(&other.display_offset)
    }
}

pub trait GridCell: Sized {
    /// Check if the cell contains any content.
    fn is_empty(&self) -> bool;

    /// Perform an opinionated cell reset based on a template cell.
    fn reset(&mut self, template: &Self);

    fn flags(&self) -> &Flags;
    fn flags_mut(&mut self) -> &mut Flags;
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Cursor<T> {
    /// The location of this cursor.
    pub point: Point,

    /// Template cell when using this cursor.
    pub template: T,

    /// Currently configured graphic character sets.
    pub charsets: Charsets,

    /// Tracks if the next call to input will need to first handle wrapping.
    ///
    /// This is true after the last column is set with the input function. Any function that
    /// implicitly sets the line or column needs to set this to false to avoid wrapping twice.
    ///
    /// Tracking `input_needs_wrap` makes it possible to not store a cursor position that exceeds
    /// the number of columns, which would lead to index out of bounds when interacting with arrays
    /// without sanitization.
    pub input_needs_wrap: bool,
}

#[derive(Debug, Default, Copy, Clone, PartialEq, Eq)]
pub struct Charsets([StandardCharset; 4]);

impl Index<CharsetIndex> for Charsets {
    type Output = StandardCharset;

    fn index(&self, index: CharsetIndex) -> &StandardCharset {
        &self.0[index as usize]
    }
}

impl IndexMut<CharsetIndex> for Charsets {
    fn index_mut(&mut self, index: CharsetIndex) -> &mut StandardCharset {
        &mut self.0[index as usize]
    }
}

/// Grid based terminal content storage.
///
/// ```notrust
/// ┌─────────────────────────┐  <-- max_scroll_limit + lines
/// │                         │
/// │      UNINITIALIZED      │
/// │                         │
/// ├─────────────────────────┤  <-- self.raw.inner.len()
/// │                         │
/// │      RESIZE BUFFER      │
/// │                         │
/// ├─────────────────────────┤  <-- self.history_size() + lines
/// │                         │
/// │     SCROLLUP REGION     │
/// │                         │
/// ├─────────────────────────┤v lines
/// │                         │|
/// │     VISIBLE  REGION     │|
/// │                         │|
/// ├─────────────────────────┤^ <-- display_offset
/// │                         │
/// │    SCROLLDOWN REGION    │
/// │                         │
/// └─────────────────────────┘  <-- zero
///                           ^
///                          cols
/// ```
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Grid<T> {
    /// Current cursor for writing data.
    #[serde(skip)]
    pub cursor: Cursor<T>,

    /// Last saved cursor.
    #[serde(skip)]
    pub saved_cursor: Cursor<T>,

    /// Lines in the grid. Each row holds a list of cells corresponding to the
    /// columns in that row.
    raw: Storage<T>,

    /// Number of columns.
    cols: Column,

    /// Number of visible lines.
    lines: Line,

    /// Offset of displayed area.
    ///
    /// If the displayed region isn't at the bottom of the screen, it stays
    /// stationary while more text is emitted. The scrolling implementation
    /// updates this offset accordingly.
    display_offset: usize,

    /// Maximum number of lines in history.
    max_scroll_limit: usize,
}

#[derive(Debug, Copy, Clone)]
pub enum Scroll {
    Delta(isize),
    PageUp,
    PageDown,
    Top,
    Bottom,
}

impl<T: GridCell + Default + PartialEq + Clone> Grid<T> {
    pub fn new(lines: Line, cols: Column, max_scroll_limit: usize) -> Grid<T> {
        Grid {
            raw: Storage::with_capacity(lines, cols),
            max_scroll_limit,
            display_offset: 0,
            saved_cursor: Cursor::default(),
            cursor: Cursor::default(),
            lines,
            cols,
        }
    }

    /// Update the size of the scrollback history.
    pub fn update_history(&mut self, history_size: usize) {
        let current_history_size = self.history_size();
        if current_history_size > history_size {
            self.raw.shrink_lines(current_history_size - history_size);
        }
        self.display_offset = min(self.display_offset, history_size);
        self.max_scroll_limit = history_size;
    }

    pub fn scroll_display(&mut self, scroll: Scroll) {
        self.display_offset = match scroll {
            Scroll::Delta(count) => min(
                max((self.display_offset as isize) + count, 0isize) as usize,
                self.history_size(),
            ),
            Scroll::PageUp => min(self.display_offset + self.lines.0, self.history_size()),
            Scroll::PageDown => self.display_offset.saturating_sub(self.lines.0),
            Scroll::Top => self.history_size(),
            Scroll::Bottom => 0,
        };
    }

    fn increase_scroll_limit(&mut self, count: usize) {
        let count = min(count, self.max_scroll_limit - self.history_size());
        if count != 0 {
            self.raw.initialize(count, self.cols);
        }
    }

    fn decrease_scroll_limit(&mut self, count: usize) {
        let count = min(count, self.history_size());
        if count != 0 {
            self.raw.shrink_lines(min(count, self.history_size()));
            self.display_offset = min(self.display_offset, self.history_size());
        }
    }

    #[inline]
    pub fn scroll_down<D>(&mut self, region: &Range<Line>, positions: Line)
    where
        T: ResetDiscriminant<D>,
        D: PartialEq,
    {
        let screen_lines = self.screen_lines().0;

        // When rotating the entire region, just reset everything.
        if positions >= region.end - region.start {
            for i in region.start.0..region.end.0 {
                let index = screen_lines - i - 1;
                self.raw[index].reset(&self.cursor.template);
            }

            return;
        }

        // Which implementation we can use depends on the existence of a scrollback history.
        //
        // Since a scrollback history prevents us from rotating the entire buffer downwards, we
        // instead have to rely on a slower, swap-based implementation.
        if self.max_scroll_limit == 0 {
            // Swap the lines fixed at the bottom to their target positions after rotation.
            //
            // Since we've made sure that the rotation will never rotate away the entire region, we
            // know that the position of the fixed lines before the rotation must already be
            // visible.
            //
            // We need to start from the top, to make sure the fixed lines aren't swapped with each
            // other.
            let fixed_lines = screen_lines - region.end.0;
            for i in (0..fixed_lines).rev() {
                self.raw.swap(i, i + positions.0);
            }

            // Rotate the entire line buffer downward.
            self.raw.rotate_down(*positions);

            // Ensure all new lines are fully cleared.
            for i in 0..positions.0 {
                let index = screen_lines - i - 1;
                self.raw[index].reset(&self.cursor.template);
            }

            // Swap the fixed lines at the top back into position.
            for i in 0..region.start.0 {
                let index = screen_lines - i - 1;
                self.raw.swap(index, index - positions.0);
            }
        } else {
            // Subregion rotation.
            for line in IndexRange((region.start + positions)..region.end).rev() {
                self.raw.swap_lines(line, line - positions);
            }

            for line in IndexRange(region.start..(region.start + positions)) {
                self.raw[line].reset(&self.cursor.template);
            }
        }
    }

    /// Move lines at the bottom toward the top.
    ///
    /// This is the performance-sensitive part of scrolling.
    pub fn scroll_up<D>(&mut self, region: &Range<Line>, positions: Line)
    where
        T: ResetDiscriminant<D>,
        D: PartialEq,
    {
        let screen_lines = self.screen_lines().0;

        // When rotating the entire region with fixed lines at the top, just reset everything.
        if positions >= region.end - region.start && region.start != Line(0) {
            for i in region.start.0..region.end.0 {
                let index = screen_lines - i - 1;
                self.raw[index].reset(&self.cursor.template);
            }

            return;
        }

        // Update display offset when not pinned to active area.
        if self.display_offset != 0 {
            self.display_offset = min(self.display_offset + *positions, self.max_scroll_limit);
        }

        // Create scrollback for the new lines.
        self.increase_scroll_limit(*positions);

        // Swap the lines fixed at the top to their target positions after rotation.
        //
        // Since we've made sure that the rotation will never rotate away the entire region, we
        // know that the position of the fixed lines before the rotation must already be
        // visible.
        //
        // We need to start from the bottom, to make sure the fixed lines aren't swapped with each
        // other.
        for i in (0..region.start.0).rev() {
            let index = screen_lines - i - 1;
            self.raw.swap(index, index - positions.0);
        }

        // Rotate the entire line buffer upward.
        self.raw.rotate(-(positions.0 as isize));

        // Ensure all new lines are fully cleared.
        for i in 0..positions.0 {
            self.raw[i].reset(&self.cursor.template);
        }

        // Swap the fixed lines at the bottom back into position.
        let fixed_lines = screen_lines - region.end.0;
        for i in 0..fixed_lines {
            self.raw.swap(i, i + positions.0);
        }
    }

    pub fn clear_viewport<D>(&mut self)
    where
        T: ResetDiscriminant<D>,
        D: PartialEq,
    {
        // Determine how many lines to scroll up by.
        let end = Point { line: 0, col: self.cols() };
        let mut iter = self.iter_from(end);
        while let Some(cell) = iter.prev() {
            if !cell.is_empty() || iter.cur.line >= *self.lines {
                break;
            }
        }
        debug_assert!(iter.cur.line <= *self.lines);
        let positions = self.lines - iter.cur.line;
        let region = Line(0)..self.screen_lines();

        // Reset display offset.
        self.display_offset = 0;

        // Clear the viewport.
        self.scroll_up(&region, positions);

        // Reset rotated lines.
        for i in positions.0..self.lines.0 {
            self.raw[i].reset(&self.cursor.template);
        }
    }

    /// Completely reset the grid state.
    pub fn reset<D>(&mut self)
    where
        T: ResetDiscriminant<D>,
        D: PartialEq,
    {
        self.clear_history();

        self.saved_cursor = Cursor::default();
        self.cursor = Cursor::default();
        self.display_offset = 0;

        // Reset all visible lines.
        for row in 0..self.raw.len() {
            self.raw[row].reset(&self.cursor.template);
        }
    }
}

#[allow(clippy::len_without_is_empty)]
impl<T> Grid<T> {
    /// Clamp a buffer point to the visible region.
    pub fn clamp_buffer_to_visible(&self, point: Point<usize>) -> Point {
        if point.line < self.display_offset {
            Point::new(self.lines - 1, self.cols - 1)
        } else if point.line >= self.display_offset + self.lines.0 {
            Point::new(Line(0), Column(0))
        } else {
            // Since edgecases are handled, conversion is identical as visible to buffer.
            self.visible_to_buffer(point.into()).into()
        }
    }

    // Clamp a buffer point based range to the viewport.
    //
    // This will make sure the content within the range is visible and return `None` whenever the
    // entire range is outside the visible region.
    pub fn clamp_buffer_range_to_visible(
        &self,
        range: &RangeInclusive<Point<usize>>,
    ) -> Option<RangeInclusive<Point>> {
        let start = range.start();
        let end = range.end();

        // Check if the range is completely offscreen
        let viewport_end = self.display_offset;
        let viewport_start = viewport_end + self.lines.0 - 1;
        if end.line > viewport_start || start.line < viewport_end {
            return None;
        }

        let start = self.clamp_buffer_to_visible(*start);
        let end = self.clamp_buffer_to_visible(*end);

        Some(start..=end)
    }

    /// Convert viewport relative point to global buffer indexing.
    #[inline]
    pub fn visible_to_buffer(&self, point: Point) -> Point<usize> {
        Point { line: self.lines.0 + self.display_offset - point.line.0 - 1, col: point.col }
    }

    #[inline]
    pub fn display_iter(&self) -> DisplayIter<'_, T> {
        DisplayIter::new(self)
    }

    #[inline]
    pub fn clear_history(&mut self) {
        // Explicitly purge all lines from history.
        self.raw.shrink_lines(self.history_size());
    }

    /// This is used only for initializing after loading ref-tests.
    #[inline]
    pub fn initialize_all(&mut self)
    where
        T: GridCell + Clone + Default,
    {
        // Remove all cached lines to clear them of any content.
        self.truncate();

        // Initialize everything with empty new lines.
        self.raw.initialize(self.max_scroll_limit - self.history_size(), self.cols);
    }

    /// This is used only for truncating before saving ref-tests.
    #[inline]
    pub fn truncate(&mut self) {
        self.raw.truncate();
    }

    #[inline]
    pub fn iter_from(&self, point: Point<usize>) -> GridIterator<'_, T> {
        GridIterator { grid: self, cur: point }
    }

    #[inline]
    pub fn display_offset(&self) -> usize {
        self.display_offset
    }

    #[inline]
    pub fn cursor_cell(&mut self) -> &mut T {
        let point = self.cursor.point;
        &mut self[&point]
    }
}

/// Grid dimensions.
pub trait Dimensions {
    /// Total number of lines in the buffer, this includes scrollback and visible lines.
    fn total_lines(&self) -> usize;

    /// Height of the viewport in lines.
    fn screen_lines(&self) -> Line;

    /// Width of the terminal in columns.
    fn cols(&self) -> Column;

    /// Number of invisible lines part of the scrollback history.
    #[inline]
    fn history_size(&self) -> usize {
        self.total_lines() - self.screen_lines().0
    }
}

impl<G> Dimensions for Grid<G> {
    #[inline]
    fn total_lines(&self) -> usize {
        self.raw.len()
    }

    #[inline]
    fn screen_lines(&self) -> Line {
        self.lines
    }

    #[inline]
    fn cols(&self) -> Column {
        self.cols
    }
}

#[cfg(test)]
impl Dimensions for (Line, Column) {
    fn total_lines(&self) -> usize {
        *self.0
    }

    fn screen_lines(&self) -> Line {
        self.0
    }

    fn cols(&self) -> Column {
        self.1
    }
}

pub struct GridIterator<'a, T> {
    /// Immutable grid reference.
    grid: &'a Grid<T>,

    /// Current position of the iterator within the grid.
    cur: Point<usize>,
}

impl<'a, T> GridIterator<'a, T> {
    pub fn point(&self) -> Point<usize> {
        self.cur
    }

    pub fn cell(&self) -> &'a T {
        &self.grid[self.cur]
    }
}

impl<'a, T> Iterator for GridIterator<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        let last_col = self.grid.cols() - 1;

        match self.cur {
            Point { line, col } if line == 0 && col == last_col => return None,
            Point { col, .. } if (col == last_col) => {
                self.cur.line -= 1;
                self.cur.col = Column(0);
            },
            _ => self.cur.col += Column(1),
        }

        Some(&self.grid[self.cur])
    }
}

impl<'a, T> BidirectionalIterator for GridIterator<'a, T> {
    fn prev(&mut self) -> Option<Self::Item> {
        let last_col = self.grid.cols() - 1;

        match self.cur {
            Point { line, col: Column(0) } if line == self.grid.total_lines() - 1 => return None,
            Point { col: Column(0), .. } => {
                self.cur.line += 1;
                self.cur.col = last_col;
            },
            _ => self.cur.col -= Column(1),
        }

        Some(&self.grid[self.cur])
    }
}

/// Index active region by line.
impl<T> Index<Line> for Grid<T> {
    type Output = Row<T>;

    #[inline]
    fn index(&self, index: Line) -> &Row<T> {
        &self.raw[index]
    }
}

/// Index with buffer offset.
impl<T> Index<usize> for Grid<T> {
    type Output = Row<T>;

    #[inline]
    fn index(&self, index: usize) -> &Row<T> {
        &self.raw[index]
    }
}

impl<T> IndexMut<Line> for Grid<T> {
    #[inline]
    fn index_mut(&mut self, index: Line) -> &mut Row<T> {
        &mut self.raw[index]
    }
}

impl<T> IndexMut<usize> for Grid<T> {
    #[inline]
    fn index_mut(&mut self, index: usize) -> &mut Row<T> {
        &mut self.raw[index]
    }
}

impl<'point, T> Index<&'point Point> for Grid<T> {
    type Output = T;

    #[inline]
    fn index<'a>(&'a self, point: &Point) -> &'a T {
        &self[point.line][point.col]
    }
}

impl<'point, T> IndexMut<&'point Point> for Grid<T> {
    #[inline]
    fn index_mut<'a, 'b>(&'a mut self, point: &'b Point) -> &'a mut T {
        &mut self[point.line][point.col]
    }
}

impl<T> Index<Point<usize>> for Grid<T> {
    type Output = T;

    #[inline]
    fn index(&self, point: Point<usize>) -> &T {
        &self[point.line][point.col]
    }
}

impl<T> IndexMut<Point<usize>> for Grid<T> {
    #[inline]
    fn index_mut(&mut self, point: Point<usize>) -> &mut T {
        &mut self[point.line][point.col]
    }
}

/// A subset of lines in the grid.
///
/// May be constructed using Grid::region(..).
pub struct Region<'a, T> {
    start: Line,
    end: Line,
    raw: &'a Storage<T>,
}

/// A mutable subset of lines in the grid.
///
/// May be constructed using Grid::region_mut(..).
pub struct RegionMut<'a, T> {
    start: Line,
    end: Line,
    raw: &'a mut Storage<T>,
}

impl<'a, T> RegionMut<'a, T> {
    /// Call the provided function for every item in this region.
    pub fn each<F: Fn(&mut T)>(self, func: F) {
        for row in self {
            for item in row {
                func(item)
            }
        }
    }
}

pub trait IndexRegion<I, T> {
    /// Get an immutable region of Self.
    fn region(&self, _: I) -> Region<'_, T>;

    /// Get a mutable region of Self.
    fn region_mut(&mut self, _: I) -> RegionMut<'_, T>;
}

impl<T> IndexRegion<Range<Line>, T> for Grid<T> {
    fn region(&self, index: Range<Line>) -> Region<'_, T> {
        assert!(index.start < self.screen_lines());
        assert!(index.end <= self.screen_lines());
        assert!(index.start <= index.end);
        Region { start: index.start, end: index.end, raw: &self.raw }
    }

    fn region_mut(&mut self, index: Range<Line>) -> RegionMut<'_, T> {
        assert!(index.start < self.screen_lines());
        assert!(index.end <= self.screen_lines());
        assert!(index.start <= index.end);
        RegionMut { start: index.start, end: index.end, raw: &mut self.raw }
    }
}

impl<T> IndexRegion<RangeTo<Line>, T> for Grid<T> {
    fn region(&self, index: RangeTo<Line>) -> Region<'_, T> {
        assert!(index.end <= self.screen_lines());
        Region { start: Line(0), end: index.end, raw: &self.raw }
    }

    fn region_mut(&mut self, index: RangeTo<Line>) -> RegionMut<'_, T> {
        assert!(index.end <= self.screen_lines());
        RegionMut { start: Line(0), end: index.end, raw: &mut self.raw }
    }
}

impl<T> IndexRegion<RangeFrom<Line>, T> for Grid<T> {
    fn region(&self, index: RangeFrom<Line>) -> Region<'_, T> {
        assert!(index.start < self.screen_lines());
        Region { start: index.start, end: self.screen_lines(), raw: &self.raw }
    }

    fn region_mut(&mut self, index: RangeFrom<Line>) -> RegionMut<'_, T> {
        assert!(index.start < self.screen_lines());
        RegionMut { start: index.start, end: self.screen_lines(), raw: &mut self.raw }
    }
}

impl<T> IndexRegion<RangeFull, T> for Grid<T> {
    fn region(&self, _: RangeFull) -> Region<'_, T> {
        Region { start: Line(0), end: self.screen_lines(), raw: &self.raw }
    }

    fn region_mut(&mut self, _: RangeFull) -> RegionMut<'_, T> {
        RegionMut { start: Line(0), end: self.screen_lines(), raw: &mut self.raw }
    }
}

pub struct RegionIter<'a, T> {
    end: Line,
    cur: Line,
    raw: &'a Storage<T>,
}

pub struct RegionIterMut<'a, T> {
    end: Line,
    cur: Line,
    raw: &'a mut Storage<T>,
}

impl<'a, T> IntoIterator for Region<'a, T> {
    type IntoIter = RegionIter<'a, T>;
    type Item = &'a Row<T>;

    fn into_iter(self) -> Self::IntoIter {
        RegionIter { end: self.end, cur: self.start, raw: self.raw }
    }
}

impl<'a, T> IntoIterator for RegionMut<'a, T> {
    type IntoIter = RegionIterMut<'a, T>;
    type Item = &'a mut Row<T>;

    fn into_iter(self) -> Self::IntoIter {
        RegionIterMut { end: self.end, cur: self.start, raw: self.raw }
    }
}

impl<'a, T> Iterator for RegionIter<'a, T> {
    type Item = &'a Row<T>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.cur < self.end {
            let index = self.cur;
            self.cur += 1;
            Some(&self.raw[index])
        } else {
            None
        }
    }
}

impl<'a, T> Iterator for RegionIterMut<'a, T> {
    type Item = &'a mut Row<T>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.cur < self.end {
            let index = self.cur;
            self.cur += 1;
            unsafe { Some(&mut *(&mut self.raw[index] as *mut _)) }
        } else {
            None
        }
    }
}

/// Iterates over the visible area accounting for buffer transform.
pub struct DisplayIter<'a, T> {
    grid: &'a Grid<T>,
    offset: usize,
    limit: usize,
    col: Column,
    line: Line,
}

impl<'a, T: 'a> DisplayIter<'a, T> {
    pub fn new(grid: &'a Grid<T>) -> DisplayIter<'a, T> {
        let offset = grid.display_offset + *grid.screen_lines() - 1;
        let limit = grid.display_offset;
        let col = Column(0);
        let line = Line(0);

        DisplayIter { grid, offset, col, limit, line }
    }

    pub fn offset(&self) -> usize {
        self.offset
    }

    pub fn point(&self) -> Point {
        Point::new(self.line, self.col)
    }
}

impl<'a, T: 'a> Iterator for DisplayIter<'a, T> {
    type Item = Indexed<&'a T>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        // Return None if we've reached the end.
        if self.offset == self.limit && self.grid.cols() == self.col {
            return None;
        }

        // Get the next item.
        let item = Some(Indexed {
            inner: &self.grid.raw[self.offset][self.col],
            line: self.line,
            column: self.col,
        });

        // Update line/col to point to next item.
        self.col += 1;
        if self.col == self.grid.cols() && self.offset != self.limit {
            self.offset -= 1;

            self.col = Column(0);
            self.line = Line(*self.grid.lines - 1 - (self.offset - self.limit));
        }

        item
    }
}
