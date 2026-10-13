# smithyfull variants

Patches against `scenarios/rust/smithyfull` behind the `smithyfull` rows on
`site/src/tuning.md`. Each variant was built as a copy of the scenario (with the
server SDK generated as for the matrix) and the patch applied, then deployed with
`../probe.py` using the `@payload=smithyfull-event.json` config key.

| Patch                     | What it adds                                                                                   |
| ------------------------- | ---------------------------------------------------------------------------------------------- |
| `slim-prewarm-read.patch` | SDK default features off; `SF_PREWARM=1` opens all three connections at init, concurrently; `SF_PREFAULT=1` reads the executable at init |
| `ring-prewarm-read.patch` | The same, with rustls on ring passed to the loader as the HTTP client                          |
| `counters-read.patch`     | `SF_PREFAULT=1`, plus `probe_counters.rs`: one `PROBE {json}` line per request on stderr (CPU, page faults, bytes read, resident executable), and with `SF_PAGES=1` the executable's mapped pages after Init and after the first request |

`counters-read.patch` also needs `probe_counters.rs` copied into `src/`. To compare
dependency sets, build it once with the scenario's `Cargo.lock` and once with a
lock refreshed by `cargo update`; the patch itself adds only `libc`.

To attribute pages to crates, build the same copy a second time with
`CARGO_PROFILE_LAMBDA_STRIP=false` and deploy that build: stripping changes the
code layout slightly, so page lists only map onto the symbols of the binary that
produced them.
