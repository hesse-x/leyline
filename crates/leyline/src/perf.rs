use std::{
    array,
    fmt::Write as _,
    path::PathBuf,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::Instant,
};

const BUCKET_LIMITS_NS: [u64; 12] = [
    0,
    1_000,
    10_000,
    100_000,
    1_000_000,
    5_000_000,
    10_000_000,
    16_666_667,
    33_333_334,
    100_000_000,
    1_000_000_000,
    u64::MAX,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(usize)]
pub enum PerfStage {
    PtyDrain,
    TerminalParse,
    Snapshot,
    BidiMap,
    Shape,
    Raster,
    SceneBuild,
    AtlasPrepare,
    AtlasUpload,
    VulkanSubmit,
    Present,
    InputToPtyWrite,
}

impl PerfStage {
    const ALL: [Self; 12] = [
        Self::PtyDrain,
        Self::TerminalParse,
        Self::Snapshot,
        Self::BidiMap,
        Self::Shape,
        Self::Raster,
        Self::SceneBuild,
        Self::AtlasPrepare,
        Self::AtlasUpload,
        Self::VulkanSubmit,
        Self::Present,
        Self::InputToPtyWrite,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::PtyDrain => "PtyDrain",
            Self::TerminalParse => "TerminalParse",
            Self::Snapshot => "Snapshot",
            Self::BidiMap => "BidiMap",
            Self::Shape => "Shape",
            Self::Raster => "Raster",
            Self::SceneBuild => "SceneBuild",
            Self::AtlasPrepare => "AtlasPrepare",
            Self::AtlasUpload => "AtlasUpload",
            Self::VulkanSubmit => "VulkanSubmit",
            Self::Present => "Present",
            Self::InputToPtyWrite => "InputToPtyWrite",
        }
    }
}

struct Counters {
    count: AtomicU64,
    total_ns: AtomicU64,
    max_ns: AtomicU64,
    buckets: [AtomicU64; BUCKET_LIMITS_NS.len()],
}

impl Counters {
    fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            total_ns: AtomicU64::new(0),
            max_ns: AtomicU64::new(0),
            buckets: array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

struct Registry {
    stages: [Counters; PerfStage::ALL.len()],
    bitmap_hits: AtomicU64,
    bitmap_misses: AtomicU64,
    cache_entries: AtomicU64,
    cache_bytes: AtomicU64,
    cache_evictions: AtomicU64,
    cache_pinned: AtomicU64,
    atlas_pages: AtomicU64,
    atlas_gray_pages: AtomicU64,
    atlas_color_pages: AtomicU64,
    atlas_entries: AtomicU64,
    atlas_epoch: AtomicU64,
    atlas_repacks: AtomicU64,
}

impl Registry {
    fn new() -> Self {
        Self {
            stages: array::from_fn(|_| Counters::new()),
            bitmap_hits: AtomicU64::new(0),
            bitmap_misses: AtomicU64::new(0),
            cache_entries: AtomicU64::new(0),
            cache_bytes: AtomicU64::new(0),
            cache_evictions: AtomicU64::new(0),
            cache_pinned: AtomicU64::new(0),
            atlas_pages: AtomicU64::new(0),
            atlas_gray_pages: AtomicU64::new(0),
            atlas_color_pages: AtomicU64::new(0),
            atlas_entries: AtomicU64::new(0),
            atlas_epoch: AtomicU64::new(0),
            atlas_repacks: AtomicU64::new(0),
        }
    }
}

static ENABLED: AtomicBool = AtomicBool::new(false);
static REGISTRY: std::sync::LazyLock<Registry> = std::sync::LazyLock::new(Registry::new);

pub struct PerfOutputGuard {
    path: PathBuf,
    workload: String,
    started: Instant,
}

/// Enables process metrics only when `LEYLINE_PERF_OUTPUT` names an output file.
#[must_use]
pub fn initialize_from_env() -> Option<PerfOutputGuard> {
    let path = std::env::var_os("LEYLINE_PERF_OUTPUT").map(PathBuf::from)?;
    let workload = std::env::var("LEYLINE_PERF_WORKLOAD").unwrap_or_else(|_| "interactive".into());
    ENABLED.store(true, Ordering::Release);
    Some(PerfOutputGuard {
        path,
        workload,
        started: Instant::now(),
    })
}

#[must_use]
pub fn timer(stage: PerfStage) -> PerfTimer {
    PerfTimer {
        stage,
        started: ENABLED.load(Ordering::Acquire).then(Instant::now),
    }
}

pub struct PerfTimer {
    stage: PerfStage,
    started: Option<Instant>,
}

impl Drop for PerfTimer {
    fn drop(&mut self) {
        let Some(started) = self.started else {
            return;
        };
        let elapsed = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        record(self.stage, elapsed);
    }
}

pub fn record_bitmap_cache(hits: u64, misses: u64) {
    if !ENABLED.load(Ordering::Acquire) {
        return;
    }
    saturating_add(&REGISTRY.bitmap_hits, hits);
    saturating_add(&REGISTRY.bitmap_misses, misses);
}

pub fn set_text_cache_stats(first: leyline_text::CacheStats, second: leyline_text::CacheStats) {
    if !ENABLED.load(Ordering::Acquire) {
        return;
    }
    let store_usize = |target: &AtomicU64, left: usize, right: usize| {
        target.store(
            u64::try_from(left.saturating_add(right)).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    };
    store_usize(&REGISTRY.cache_entries, first.entries, second.entries);
    store_usize(&REGISTRY.cache_bytes, first.bytes, second.bytes);
    store_usize(&REGISTRY.cache_pinned, first.pinned, second.pinned);
    REGISTRY.cache_evictions.store(
        first.evictions.saturating_add(second.evictions),
        Ordering::Relaxed,
    );
}

pub fn set_atlas_stats(stats: leyline_gfx::AtlasStats) {
    if !ENABLED.load(Ordering::Acquire) {
        return;
    }
    REGISTRY.atlas_pages.store(
        u64::try_from(stats.pages).unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );
    REGISTRY.atlas_gray_pages.store(
        u64::try_from(stats.gray_pages).unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );
    REGISTRY.atlas_color_pages.store(
        u64::try_from(stats.color_pages).unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );
    REGISTRY.atlas_entries.store(
        u64::try_from(stats.entries).unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );
    REGISTRY.atlas_epoch.store(stats.epoch, Ordering::Relaxed);
    REGISTRY
        .atlas_repacks
        .store(stats.repacks, Ordering::Relaxed);
}

pub fn record_duration(stage: PerfStage, elapsed: std::time::Duration) {
    if ENABLED.load(Ordering::Acquire) {
        record(stage, u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX));
    }
}

fn record(stage: PerfStage, elapsed_ns: u64) {
    let counters = &REGISTRY.stages[stage as usize];
    saturating_add(&counters.count, 1);
    saturating_add(&counters.total_ns, elapsed_ns);
    counters.max_ns.fetch_max(elapsed_ns, Ordering::Relaxed);
    let bucket = BUCKET_LIMITS_NS
        .iter()
        .position(|limit| elapsed_ns <= *limit)
        .unwrap_or(BUCKET_LIMITS_NS.len() - 1);
    saturating_add(&counters.buckets[bucket], 1);
}

fn saturating_add(value: &AtomicU64, increment: u64) {
    let _ = value.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(increment))
    });
}

fn percentile(counters: &Counters, numerator: u64) -> u64 {
    let count = counters.count.load(Ordering::Relaxed);
    if count == 0 {
        return 0;
    }
    let target = count.saturating_mul(numerator).saturating_add(99) / 100;
    let mut cumulative = 0_u64;
    for (index, bucket) in counters.buckets.iter().enumerate() {
        cumulative = cumulative.saturating_add(bucket.load(Ordering::Relaxed));
        if cumulative >= target {
            return BUCKET_LIMITS_NS[index];
        }
    }
    u64::MAX
}

impl Drop for PerfOutputGuard {
    fn drop(&mut self) {
        let mut stages = String::new();
        for (index, stage) in PerfStage::ALL.iter().copied().enumerate() {
            if index != 0 {
                stages.push(',');
            }
            let counters = &REGISTRY.stages[stage as usize];
            let buckets = counters
                .buckets
                .iter()
                .map(|bucket| bucket.load(Ordering::Relaxed).to_string())
                .collect::<Vec<_>>()
                .join(",");
            let _ = write!(
                stages,
                "\"{}\":{{\"count\":{},\"total_ns\":{},\"max_ns\":{},\"p50_ns\":{},\"p95_ns\":{},\"p99_ns\":{},\"buckets\":[{}]}}",
                stage.name(),
                counters.count.load(Ordering::Relaxed),
                counters.total_ns.load(Ordering::Relaxed),
                counters.max_ns.load(Ordering::Relaxed),
                percentile(counters, 50),
                percentile(counters, 95),
                percentile(counters, 99),
                buckets
            );
        }
        let rss_kib = std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|status| {
                status.lines().find_map(|line| {
                    line.strip_prefix("VmRSS:")?
                        .split_whitespace()
                        .next()?
                        .parse::<u64>()
                        .ok()
                })
            })
            .unwrap_or(0);
        let cpu = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
        let elapsed_ns = u64::try_from(self.started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let flag = |name: &str| std::env::var(name).unwrap_or_else(|_| "unknown".into());
        let report = format!(
            "{{\"workload\":\"{}\",\"environment\":{{\"cpu_count\":{},\"rss_kib\":{},\"elapsed_ns\":{}}},\"configuration\":{{\"bidi\":\"{}\",\"ligatures\":\"{}\",\"color_glyphs\":\"{}\"}},\"samples\":{},\"p50_ns\":{},\"p95_ns\":{},\"p99_ns\":{},\"bitmap_cache\":{{\"hits\":{},\"misses\":{},\"entries\":{},\"bytes\":{},\"evictions\":{},\"pinned\":{}}},\"atlas\":{{\"pages\":{},\"gray_pages\":{},\"color_pages\":{},\"entries\":{},\"epoch\":{},\"repacks\":{}}},\"stages\":{{{}}}}}\n",
            self.workload.replace(['\\', '\"'], "_"),
            cpu,
            rss_kib,
            elapsed_ns,
            flag("LEYLINE_PERF_BIDI").replace(['\\', '\"'], "_"),
            flag("LEYLINE_PERF_LIGATURES").replace(['\\', '\"'], "_"),
            flag("LEYLINE_PERF_COLOR_GLYPHS").replace(['\\', '\"'], "_"),
            REGISTRY.stages[PerfStage::SceneBuild as usize]
                .count
                .load(Ordering::Relaxed),
            percentile(&REGISTRY.stages[PerfStage::SceneBuild as usize], 50),
            percentile(&REGISTRY.stages[PerfStage::SceneBuild as usize], 95),
            percentile(&REGISTRY.stages[PerfStage::SceneBuild as usize], 99),
            REGISTRY.bitmap_hits.load(Ordering::Relaxed),
            REGISTRY.bitmap_misses.load(Ordering::Relaxed),
            REGISTRY.cache_entries.load(Ordering::Relaxed),
            REGISTRY.cache_bytes.load(Ordering::Relaxed),
            REGISTRY.cache_evictions.load(Ordering::Relaxed),
            REGISTRY.cache_pinned.load(Ordering::Relaxed),
            REGISTRY.atlas_pages.load(Ordering::Relaxed),
            REGISTRY.atlas_gray_pages.load(Ordering::Relaxed),
            REGISTRY.atlas_color_pages.load(Ordering::Relaxed),
            REGISTRY.atlas_entries.load(Ordering::Relaxed),
            REGISTRY.atlas_epoch.load(Ordering::Relaxed),
            REGISTRY.atlas_repacks.load(Ordering::Relaxed),
            stages,
        );
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(error) = std::fs::write(&self.path, report) {
            tracing::warn!(%error, path = %self.path.display(), "failed to write performance report");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn records_zero_boundaries_and_saturates_overflow() {
        let _lock = TEST_LOCK.lock().unwrap();
        ENABLED.store(true, Ordering::Release);
        let counters = &REGISTRY.stages[PerfStage::InputToPtyWrite as usize];
        record(PerfStage::InputToPtyWrite, 0);
        record(PerfStage::InputToPtyWrite, 1_000);
        record(PerfStage::InputToPtyWrite, 1_001);
        assert_eq!(counters.buckets[0].load(Ordering::Relaxed), 1);
        assert_eq!(counters.buckets[1].load(Ordering::Relaxed), 1);
        assert_eq!(counters.buckets[2].load(Ordering::Relaxed), 1);
        counters.total_ns.store(u64::MAX - 1, Ordering::Relaxed);
        record(PerfStage::InputToPtyWrite, 10);
        assert_eq!(counters.total_ns.load(Ordering::Relaxed), u64::MAX);
        ENABLED.store(false, Ordering::Release);
    }

    #[test]
    fn disabled_timer_has_no_timestamp_or_registry_access() {
        let _lock = TEST_LOCK.lock().unwrap();
        ENABLED.store(false, Ordering::Release);
        let timer = timer(PerfStage::PtyDrain);
        assert!(timer.started.is_none());
    }
}
