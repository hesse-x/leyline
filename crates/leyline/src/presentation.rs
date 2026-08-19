//! UI-thread presentation state and atomic prepared-frame publication.

use std::sync::Arc;

use crate::unicode_layout::VisualGridMap;

/// Keeps frame preparation and visual-map publication in one transaction.
#[derive(Default)]
pub struct PresentationPipeline {
    pending: Option<PreparedFrame>,
    published_visual_map: Option<Arc<VisualGridMap>>,
}

struct PreparedFrame {
    key: leyline_gfx::FrameKey,
    visual_map: Arc<VisualGridMap>,
}

impl PresentationPipeline {
    pub fn stage(&mut self, key: leyline_gfx::FrameKey, visual_map: Arc<VisualGridMap>) {
        self.pending = Some(PreparedFrame { key, visual_map });
    }

    pub fn commit(
        &mut self,
        committed: leyline_gfx::CommittedFrameKey,
    ) -> Option<Arc<VisualGridMap>> {
        let prepared = self.pending.take()?;
        if prepared.key != committed.frame {
            return None;
        }
        self.published_visual_map = Some(Arc::clone(&prepared.visual_map));
        Some(prepared.visual_map)
    }

    #[must_use]
    pub fn published_visual_map(&self) -> Option<&Arc<VisualGridMap>> {
        self.published_visual_map.as_ref()
    }

    pub fn reset(&mut self) {
        self.pending = None;
        self.published_visual_map = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::GridSize;

    fn map(generation: u64) -> Arc<VisualGridMap> {
        Arc::new(VisualGridMap {
            snapshot_generation: generation,
            policy_generation: 1,
            grid: GridSize::new(1, 1).unwrap(),
            bidi_enabled: true,
            lines: Arc::from([]),
        })
    }

    #[test]
    fn replacement_and_stale_commit_never_publish_part_of_a_frame() {
        let mut pipeline = PresentationPipeline::default();
        let first = leyline_gfx::FrameKey {
            snapshot_generation: 1,
            ..leyline_gfx::FrameKey::default()
        };
        let second = leyline_gfx::FrameKey {
            snapshot_generation: 2,
            ..leyline_gfx::FrameKey::default()
        };
        pipeline.stage(first, map(1));
        pipeline.stage(second, map(2));
        assert!(
            pipeline
                .commit(leyline_gfx::CommittedFrameKey {
                    frame: first,
                    atlas_epoch: 1,
                })
                .is_none()
        );
        assert!(pipeline.published_visual_map().is_none());
    }
}
