//! Bounded, per-frame projection between terminal logical cells and bidi visual cells.

use std::{ops::Range, sync::Arc, time::Instant};

use unicode_bidi::{BidiInfo, Level};
use unicode_segmentation::UnicodeSegmentation;

use crate::{
    layout::GridLayout,
    terminal::{CellWidth, FrameSnapshot, GridSize, SelectionPoint},
};

pub const UNICODE_BIDI_VERSION: &str = "unicode-bidi 0.3.18";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnicodePolicy {
    pub bidi: bool,
    pub generation: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CellAtom {
    pub logical_start: u16,
    pub span: u8,
    pub text_range: Range<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShapeCluster {
    pub logical_start: u16,
    pub logical_end: u16,
    pub text: String,
}

/// Groups extended grapheme clusters without changing the snapshot's cell occupancy.
///
/// # Errors
/// Returns a typed error for an out-of-range line or inconsistent provenance.
pub fn shape_clusters(
    snapshot: &FrameSnapshot,
    line: usize,
) -> Result<Vec<ShapeCluster>, UnicodeLayoutError> {
    if line >= snapshot.grid.lines() {
        return Err(UnicodeLayoutError::InvalidSnapshot);
    }
    let columns = snapshot.grid.columns();
    let row = &snapshot.cells[line * columns..(line + 1) * columns];
    let mut text = String::new();
    let mut byte_to_cell = Vec::new();
    for (column, cell) in row.iter().enumerate() {
        if matches!(cell.width, CellWidth::Spacer | CellWidth::LeadingSpacer) {
            continue;
        }
        let start = text.len();
        text.push(cell.ch);
        if let Some(extra) = &cell.zerowidth {
            text.extend(extra.iter());
        }
        byte_to_cell.extend(std::iter::repeat_n(
            u16::try_from(column).map_err(|_| UnicodeLayoutError::Capacity)?,
            text.len() - start,
        ));
    }
    let mut clusters = Vec::new();
    for (offset, cluster) in text.grapheme_indices(true) {
        let end = offset + cluster.len();
        let start_cell = *byte_to_cell
            .get(offset)
            .ok_or(UnicodeLayoutError::Projection { line })?;
        let end_cell = byte_to_cell
            .get(end.saturating_sub(1))
            .copied()
            .ok_or(UnicodeLayoutError::Projection { line })?
            .saturating_add(1);
        clusters.push(ShapeCluster {
            logical_start: start_cell,
            logical_end: end_cell,
            text: cluster.to_owned(),
        });
    }
    Ok(clusters)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaretAffinity {
    Before,
    After,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VisualCaret {
    pub logical_boundary: u16,
    pub affinity: CaretAffinity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VisualLineMap {
    pub visual_to_logical_cell: Box<[u16]>,
    pub logical_to_visual_cell: Box<[u16]>,
    pub logical_cell_to_owner: Box<[u16]>,
    pub owner_span: Box<[u8]>,
    pub atom_levels: Box<[u8]>,
    pub visual_carets: Box<[VisualCaret]>,
    pub paragraph_level: u8,
}

impl VisualLineMap {
    fn identity(columns: usize, owners: &[u16], spans: &[u8]) -> Self {
        let cells = (0..columns)
            .map(|column| u16::try_from(column).expect("grid columns fit u16"))
            .collect::<Vec<_>>();
        let carets = (0..=columns)
            .map(|column| VisualCaret {
                logical_boundary: u16::try_from(column).expect("grid columns fit u16"),
                affinity: if column == columns {
                    CaretAffinity::After
                } else {
                    CaretAffinity::Before
                },
            })
            .collect();
        Self {
            visual_to_logical_cell: cells.clone().into_boxed_slice(),
            logical_to_visual_cell: cells.into_boxed_slice(),
            logical_cell_to_owner: owners.to_vec().into_boxed_slice(),
            owner_span: spans.to_vec().into_boxed_slice(),
            atom_levels: vec![0; columns].into_boxed_slice(),
            visual_carets: carets,
            paragraph_level: 0,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VisualGridMap {
    pub snapshot_generation: u64,
    pub policy_generation: u64,
    pub grid: GridSize,
    pub bidi_enabled: bool,
    pub lines: Arc<[VisualLineMap]>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VisualHit {
    Cell {
        physical_point: SelectionPoint,
        owner_point: SelectionPoint,
        caret: VisualCaret,
    },
    Outside,
}

pub struct VisualMapBuilder {
    snapshot_generation: u64,
    policy: UnicodePolicy,
    grid: GridSize,
    cells: Arc<[crate::terminal::SnapshotCell]>,
    next_line: usize,
    lines: Vec<VisualLineMap>,
}

pub enum BuildStep {
    Pending,
    Ready(VisualGridMap),
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum UnicodeLayoutError {
    #[error("snapshot cell count does not match its grid")]
    InvalidSnapshot,
    #[error("invalid wide-cell/spacer contract on line {line}")]
    InvalidCellSpan { line: usize },
    #[error("bidi result cannot be projected onto terminal atoms on line {line}")]
    Projection { line: usize },
    #[error("unicode layout capacity exceeded")]
    Capacity,
}

/// Starts bounded visual-map preparation.
///
/// # Errors
/// Returns a typed error when snapshot dimensions or storage are invalid.
pub fn begin_visual_map(
    snapshot: &FrameSnapshot,
    policy: UnicodePolicy,
) -> Result<VisualMapBuilder, UnicodeLayoutError> {
    let expected = snapshot
        .grid
        .columns()
        .checked_mul(snapshot.grid.lines())
        .ok_or(UnicodeLayoutError::Capacity)?;
    if snapshot.cells.len() != expected {
        return Err(UnicodeLayoutError::InvalidSnapshot);
    }
    Ok(VisualMapBuilder {
        snapshot_generation: snapshot.generation,
        policy,
        grid: snapshot.grid,
        cells: Arc::clone(&snapshot.cells),
        next_line: 0,
        lines: Vec::with_capacity(snapshot.grid.lines()),
    })
}

impl VisualMapBuilder {
    /// Advances at most 64 lines and stops at the supplied deadline.
    ///
    /// # Errors
    /// Returns a typed projection error when a row cannot safely map to physical cells.
    pub fn step(&mut self, deadline: Instant) -> Result<BuildStep, UnicodeLayoutError> {
        let stop = self.next_line.saturating_add(64).min(self.grid.lines());
        while self.next_line < stop && (self.next_line == 0 || Instant::now() < deadline) {
            let columns = self.grid.columns();
            let line = match build_line(&self.cells, columns, self.next_line, self.policy.bidi) {
                Ok(line) => line,
                Err(
                    UnicodeLayoutError::InvalidCellSpan { .. }
                    | UnicodeLayoutError::Projection { .. },
                ) => {
                    let row = &self.cells[self.next_line * columns..(self.next_line + 1) * columns];
                    fallback_identity(row)
                }
                Err(error) => return Err(error),
            };
            self.lines.push(line);
            self.next_line += 1;
        }
        if self.next_line != self.grid.lines() {
            return Ok(BuildStep::Pending);
        }
        Ok(BuildStep::Ready(VisualGridMap {
            snapshot_generation: self.snapshot_generation,
            policy_generation: self.policy.generation,
            grid: self.grid,
            bidi_enabled: self.policy.bidi,
            lines: Arc::from(std::mem::take(&mut self.lines)),
        }))
    }
}

fn fallback_identity(row: &[crate::terminal::SnapshotCell]) -> VisualLineMap {
    let mut owners = (0..row.len())
        .map(|column| u16::try_from(column).expect("bounded grid column"))
        .collect::<Vec<_>>();
    let mut spans = vec![1_u8; row.len()];
    for column in 0..row.len().saturating_sub(1) {
        if row[column].width == CellWidth::Wide && row[column + 1].width == CellWidth::Spacer {
            owners[column + 1] = owners[column];
            spans[column] = 2;
            spans[column + 1] = 2;
        }
    }
    VisualLineMap::identity(row.len(), &owners, &spans)
}

/// Builds a complete visual map for callers without an incremental scheduler.
///
/// # Errors
/// Returns a typed snapshot, capacity, or bidi projection error.
pub fn build_visual_map(
    snapshot: &FrameSnapshot,
    policy: UnicodePolicy,
) -> Result<VisualGridMap, UnicodeLayoutError> {
    let mut builder = begin_visual_map(snapshot, policy)?;
    loop {
        if let BuildStep::Ready(map) =
            builder.step(Instant::now() + std::time::Duration::from_secs(1))?
        {
            return Ok(map);
        }
    }
}

#[must_use]
pub fn hit_test(map: &VisualGridMap, layout: &GridLayout, pixel: [u32; 2]) -> VisualHit {
    let Some([visual, line]) = layout.cell_at_pixel(pixel) else {
        return VisualHit::Outside;
    };
    let Some(line_map) = map.lines.get(usize::from(line)) else {
        return VisualHit::Outside;
    };
    let Some(&logical) = line_map.visual_to_logical_cell.get(usize::from(visual)) else {
        return VisualHit::Outside;
    };
    let owner = line_map.logical_cell_to_owner[usize::from(logical)];
    let cell_left = layout.content_origin_px[0]
        .saturating_add(u32::from(visual) * u32::from(layout.cell_px[0].get()));
    let caret_index = usize::from(visual)
        + usize::from(
            pixel[0].saturating_sub(cell_left) >= u32::from(layout.cell_px[0].get()).div_ceil(2),
        );
    VisualHit::Cell {
        physical_point: SelectionPoint {
            column: logical,
            line,
        },
        owner_point: SelectionPoint {
            column: owner,
            line,
        },
        caret: line_map.visual_carets[caret_index],
    }
}

#[allow(clippy::too_many_lines)]
fn build_line(
    cells: &[crate::terminal::SnapshotCell],
    columns: usize,
    line: usize,
    bidi: bool,
) -> Result<VisualLineMap, UnicodeLayoutError> {
    let row = &cells[line * columns..(line + 1) * columns];
    let mut owners = vec![0_u16; columns];
    let mut spans = vec![1_u8; columns];
    let mut atoms = Vec::with_capacity(columns);
    let mut text = String::new();
    let mut column = 0;
    while column < columns {
        let cell = &row[column];
        let span = match cell.width {
            CellWidth::Wide => {
                if column + 1 >= columns || row[column + 1].width != CellWidth::Spacer {
                    return Err(UnicodeLayoutError::InvalidCellSpan { line });
                }
                2
            }
            CellWidth::Spacer => return Err(UnicodeLayoutError::InvalidCellSpan { line }),
            CellWidth::Narrow | CellWidth::LeadingSpacer => 1,
        };
        let owner = u16::try_from(column).map_err(|_| UnicodeLayoutError::Capacity)?;
        for offset in 0..span {
            owners[column + offset] = owner;
            spans[column + offset] =
                u8::try_from(span).map_err(|_| UnicodeLayoutError::Capacity)?;
        }
        let start = u32::try_from(text.len()).map_err(|_| UnicodeLayoutError::Capacity)?;
        if cell.width != CellWidth::LeadingSpacer {
            text.push(cell.ch);
            if let Some(extra) = &cell.zerowidth {
                text.extend(extra.iter());
            }
        }
        let end = u32::try_from(text.len()).map_err(|_| UnicodeLayoutError::Capacity)?;
        atoms.push(CellAtom {
            logical_start: owner,
            span: u8::try_from(span).map_err(|_| UnicodeLayoutError::Capacity)?,
            text_range: start..end,
        });
        column += span;
    }
    if !bidi || text.is_empty() {
        return Ok(VisualLineMap::identity(columns, &owners, &spans));
    }
    let info = BidiInfo::new(&text, None);
    let Some(paragraph) = info.paragraphs.first() else {
        return Ok(VisualLineMap::identity(columns, &owners, &spans));
    };
    let resolved = info.reordered_levels(paragraph, paragraph.range.clone());
    let paragraph_level = paragraph.level.number();
    let mut atom_level_values = Vec::with_capacity(atoms.len());
    for atom in &atoms {
        let range = usize::try_from(atom.text_range.start)
            .map_err(|_| UnicodeLayoutError::Capacity)?
            ..usize::try_from(atom.text_range.end).map_err(|_| UnicodeLayoutError::Capacity)?;
        let mut values = text[range.clone()]
            .char_indices()
            .map(|(offset, _)| resolved[range.start + offset]);
        let level = values
            .next()
            .unwrap_or_else(|| Level::new(paragraph_level).expect("valid paragraph level"));
        if values.any(|candidate| candidate != level) {
            return Err(UnicodeLayoutError::Projection { line });
        }
        atom_level_values.push(level);
    }
    let placeholders = atoms
        .iter()
        .map(|atom| {
            let range = usize::try_from(atom.text_range.start).unwrap_or(0)
                ..usize::try_from(atom.text_range.end).unwrap_or(0);
            !range.is_empty() && text[range].chars().all(is_x9_control)
        })
        .collect::<Vec<_>>();
    let visible = atom_level_values
        .iter()
        .enumerate()
        .filter(|(index, _)| !placeholders[*index])
        .collect::<Vec<_>>();
    let visible_levels = visible.iter().map(|(_, level)| **level).collect::<Vec<_>>();
    let mut visual_atoms = BidiInfo::reorder_visual(&visible_levels)
        .into_iter()
        .map(|index| visible[index].0)
        .collect::<Vec<_>>();
    for placeholder in 0..atoms.len() {
        if !placeholders[placeholder] {
            continue;
        }
        let insert = (0..placeholder)
            .rev()
            .find(|index| !placeholders[*index])
            .and_then(|previous| visual_atoms.iter().rposition(|index| *index == previous))
            .map_or_else(
                || {
                    (placeholder + 1..atoms.len())
                        .find(|index| !placeholders[*index])
                        .and_then(|next| visual_atoms.iter().position(|index| *index == next))
                        .unwrap_or(visual_atoms.len())
                },
                |position| {
                    let mut insert = position + 1;
                    while insert < visual_atoms.len()
                        && placeholders[visual_atoms[insert]]
                        && visual_atoms[insert] < placeholder
                    {
                        insert += 1;
                    }
                    insert
                },
            );
        visual_atoms.insert(insert, placeholder);
    }
    let mut visual_to_logical = Vec::with_capacity(columns);
    let mut atom_levels = Vec::with_capacity(columns);
    for atom_index in visual_atoms {
        let atom = &atoms[atom_index];
        for offset in 0..atom.span {
            visual_to_logical.push(atom.logical_start + u16::from(offset));
            atom_levels.push(atom_level_values[atom_index].number());
        }
    }
    if visual_to_logical.len() != columns {
        return Err(UnicodeLayoutError::Projection { line });
    }
    let mut logical_to_visual = vec![u16::MAX; columns];
    for (visual, logical) in visual_to_logical.iter().copied().enumerate() {
        let slot = logical_to_visual
            .get_mut(usize::from(logical))
            .ok_or(UnicodeLayoutError::Projection { line })?;
        if *slot != u16::MAX {
            return Err(UnicodeLayoutError::Projection { line });
        }
        *slot = u16::try_from(visual).map_err(|_| UnicodeLayoutError::Capacity)?;
    }
    let mut carets = Vec::with_capacity(columns + 1);
    for boundary in 0..=columns {
        let (logical_boundary, affinity) = if boundary == columns {
            let logical = visual_to_logical[columns - 1];
            if atom_levels[columns - 1] % 2 == 0 {
                (logical.saturating_add(1), CaretAffinity::After)
            } else {
                (owners[usize::from(logical)], CaretAffinity::Before)
            }
        } else {
            let logical = visual_to_logical[boundary];
            if atom_levels[boundary] % 2 == 0 {
                (logical, CaretAffinity::Before)
            } else if owners[usize::from(logical)] == logical {
                (
                    logical.saturating_add(u16::from(spans[usize::from(logical)])),
                    CaretAffinity::After,
                )
            } else {
                (logical, CaretAffinity::Before)
            }
        };
        carets.push(VisualCaret {
            logical_boundary,
            affinity,
        });
    }
    Ok(VisualLineMap {
        visual_to_logical_cell: visual_to_logical.into_boxed_slice(),
        logical_to_visual_cell: logical_to_visual.into_boxed_slice(),
        logical_cell_to_owner: owners.into_boxed_slice(),
        owner_span: spans.into_boxed_slice(),
        atom_levels: atom_levels.into_boxed_slice(),
        visual_carets: carets.into_boxed_slice(),
        paragraph_level,
    })
}

const fn is_x9_control(ch: char) -> bool {
    matches!(
        ch,
        '\u{00ad}' | '\u{202a}' | '\u{202b}' | '\u{202c}' | '\u{202d}' | '\u{202e}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::{CellFlags, SnapshotCell, TerminalColor, UnderlineStyle};
    use std::time::Duration;

    fn cell(ch: char, width: CellWidth) -> SnapshotCell {
        SnapshotCell {
            ch,
            zerowidth: None,
            foreground: TerminalColor::Named(256),
            background: TerminalColor::Named(257),
            underline_color: None,
            underline_style: UnderlineStyle::None,
            flags: CellFlags::default(),
            width,
            hyperlink: None,
        }
    }

    #[test]
    fn rtl_run_reorders_cells_and_preserves_inverse_permutation() {
        let cells = vec![
            cell('a', CellWidth::Narrow),
            cell('\u{05d0}', CellWidth::Narrow),
            cell('\u{05d1}', CellWidth::Narrow),
        ];
        let map = build_line(&cells, 3, 0, true).unwrap();
        assert_eq!(&*map.visual_to_logical_cell, &[0, 2, 1]);
        for (visual, logical) in map.visual_to_logical_cell.iter().enumerate() {
            assert_eq!(
                usize::from(map.logical_to_visual_cell[usize::from(*logical)]),
                visual
            );
        }
        assert_eq!(map.visual_carets.len(), 4);
    }

    #[test]
    fn wide_atom_remains_adjacent_inside_rtl_text() {
        let cells = vec![
            cell('\u{05d0}', CellWidth::Narrow),
            cell('\u{4e2d}', CellWidth::Wide),
            cell(' ', CellWidth::Spacer),
            cell('\u{05d1}', CellWidth::Narrow),
        ];
        let map = build_line(&cells, 4, 0, true).unwrap();
        let owner = map.logical_to_visual_cell[1];
        let spacer = map.logical_to_visual_cell[2];
        assert_eq!(spacer, owner + 1);
        assert_eq!(map.logical_cell_to_owner[2], 1);
    }

    #[test]
    fn malformed_spacer_is_a_typed_error() {
        let cells = vec![cell(' ', CellWidth::Spacer)];
        assert_eq!(
            build_line(&cells, 1, 0, true),
            Err(UnicodeLayoutError::InvalidCellSpan { line: 0 })
        );
    }

    #[test]
    fn x9_control_cell_is_transparent_and_attached_after_previous_atom() {
        let cells = vec![
            cell('\u{05d0}', CellWidth::Narrow),
            cell('\u{202a}', CellWidth::Narrow),
            cell('a', CellWidth::Narrow),
            cell('\u{05d1}', CellWidth::Narrow),
        ];
        let map = build_line(&cells, 4, 0, true).unwrap();
        let previous = map.logical_to_visual_cell[0];
        let placeholder = map.logical_to_visual_cell[1];
        assert_eq!(placeholder, previous + 1);
        assert_eq!(map.visual_to_logical_cell.len(), 4);
    }

    #[test]
    fn zwj_family_is_one_shape_cluster_across_logical_cells() {
        use crate::terminal::{
            CursorBlink, CursorShape, CursorSnapshot, FrameSnapshot, GridSize, TerminalModes,
        };
        let sequence = [
            '\u{1f468}',
            '\u{200d}',
            '\u{1f469}',
            '\u{200d}',
            '\u{1f467}',
            '\u{200d}',
            '\u{1f466}',
        ];
        let snapshot = FrameSnapshot {
            generation: 1,
            grid: GridSize::new(u16::try_from(sequence.len()).unwrap(), 1).unwrap(),
            cells: sequence
                .into_iter()
                .map(|ch| cell(ch, CellWidth::Narrow))
                .collect::<Vec<_>>()
                .into(),
            cursor: CursorSnapshot {
                column: 0,
                line: 0,
                visible: false,
                shape: CursorShape::Block,
                blink: CursorBlink::Steady,
            },
            modes: TerminalModes::default(),
            display_offset: 0,
            history_size: 0,
            title: None,
            hyperlinks: Arc::from([]),
        };
        let clusters = shape_clusters(&snapshot, 0).unwrap();
        assert_eq!(clusters.len(), 1);
        assert_eq!((clusters[0].logical_start, clusters[0].logical_end), (0, 7));
    }

    #[test]
    fn visual_map_builder_yields_after_sixty_four_lines() {
        use crate::terminal::{
            CursorBlink, CursorShape, CursorSnapshot, FrameSnapshot, GridSize, TerminalModes,
        };
        let snapshot = FrameSnapshot {
            generation: 7,
            grid: GridSize::new(1, 65).unwrap(),
            cells: (0..65)
                .map(|_| cell('a', CellWidth::Narrow))
                .collect::<Vec<_>>()
                .into(),
            cursor: CursorSnapshot {
                column: 0,
                line: 0,
                visible: false,
                shape: CursorShape::Block,
                blink: CursorBlink::Steady,
            },
            modes: TerminalModes::default(),
            display_offset: 0,
            history_size: 0,
            title: None,
            hyperlinks: Arc::from([]),
        };
        let mut builder = begin_visual_map(
            &snapshot,
            UnicodePolicy {
                bidi: true,
                generation: 9,
            },
        )
        .unwrap();
        assert!(matches!(
            builder
                .step(Instant::now() + Duration::from_secs(1))
                .unwrap(),
            BuildStep::Pending
        ));
        let BuildStep::Ready(map) = builder
            .step(Instant::now() + Duration::from_secs(1))
            .unwrap()
        else {
            panic!("the final line should complete the map");
        };
        assert_eq!(map.lines.len(), 65);
        assert_eq!(map.snapshot_generation, 7);
        assert_eq!(map.policy_generation, 9);
    }

    #[test]
    fn hit_test_distinguishes_both_physical_halves_of_a_wide_cell() {
        use crate::layout::GridLayout;
        use leyline_gfx::PixelSize;
        use leyline_text::CellMetrics;
        use std::num::NonZeroU16;

        let grid = GridSize::new(2, 1).unwrap();
        let line = build_line(
            &[
                cell('\u{4e2d}', CellWidth::Wide),
                cell(' ', CellWidth::Spacer),
            ],
            2,
            0,
            true,
        )
        .unwrap();
        let map = VisualGridMap {
            snapshot_generation: 1,
            policy_generation: 1,
            grid,
            bidi_enabled: true,
            lines: Arc::from([line]),
        };
        let width = NonZeroU16::new(10).unwrap();
        let height = NonZeroU16::new(20).unwrap();
        let layout = GridLayout {
            viewport_px: PixelSize {
                width: 20,
                height: 20,
            },
            content_origin_px: [0, 0],
            cell_px: [width, height],
            cell_metrics: CellMetrics {
                width_px: width,
                height_px: height,
                baseline_px: 15,
                underline_y_px: 17,
                underline_thickness_px: NonZeroU16::new(1).unwrap(),
                strike_y_px: 10,
                strike_thickness_px: NonZeroU16::new(1).unwrap(),
            },
            grid,
            font_generation: 1,
        };
        let VisualHit::Cell {
            physical_point: owner_half,
            owner_point: owner,
            ..
        } = hit_test(&map, &layout, [2, 5])
        else {
            panic!("wide owner must be hittable");
        };
        let VisualHit::Cell {
            physical_point: spacer_half,
            owner_point: spacer_owner,
            ..
        } = hit_test(&map, &layout, [12, 5])
        else {
            panic!("wide spacer must be hittable");
        };
        assert_eq!((owner_half.column, spacer_half.column), (0, 1));
        assert_eq!((owner.column, spacer_owner.column), (0, 0));
    }
}
