# Performance workloads

Run `tests/perf/run.sh all [output-directory]` to execute the headless idle,
throughput, and Unicode workloads at 80x24 and 240x80. Throughput additionally
covers current-tab, background-tab, and multi-window scheduling labels.

Each case writes one JSON document. The runner validates every document with
the standard Python JSON parser. Metrics contain no terminal text or input
bytes; fixtures only select the work performed by the local process.
