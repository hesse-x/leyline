use std::time::{Duration, Instant};

use leyline::{
    perf::{self, PerfStage},
    terminal::{GridSize, TerminalCoreAdapter},
    unicode_layout::{BuildStep, UnicodePolicy, begin_visual_map},
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _output = perf::initialize_from_env();
    let mut args = std::env::args().skip(1);
    let workload = args.next().ok_or("missing workload")?;
    let columns = args.next().ok_or("missing columns")?.parse::<u16>()?;
    let lines = args.next().ok_or("missing lines")?.parse::<u16>()?;
    let fixture = match workload.as_str() {
        "idle-blink" => b"\x1b[2J\x1b[Hidle".as_slice(),
        "throughput" => include_bytes!("../../../tests/perf/fixtures/throughput.txt").as_slice(),
        "unicode" => include_bytes!("../../../tests/perf/fixtures/unicode.txt").as_slice(),
        _ => return Err("unknown workload".into()),
    };
    let grid = GridSize::new(columns, lines)?;
    let mut terminal = TerminalCoreAdapter::new(grid, 10_000)?;

    for _ in 0..50 {
        let _drain = perf::timer(PerfStage::PtyDrain);
        terminal.advance(fixture)?;
        let snapshot = {
            let _snapshot = perf::timer(PerfStage::Snapshot);
            terminal.snapshot()?
        };
        let mut builder = {
            let _bidi = perf::timer(PerfStage::BidiMap);
            begin_visual_map(
                &snapshot,
                UnicodePolicy {
                    bidi: true,
                    generation: 1,
                },
            )?
        };
        let _scene = perf::timer(PerfStage::SceneBuild);
        loop {
            let _bidi = perf::timer(PerfStage::BidiMap);
            if let BuildStep::Ready(_) = builder.step(Instant::now() + Duration::from_secs(1))? {
                break;
            }
        }
    }
    Ok(())
}
