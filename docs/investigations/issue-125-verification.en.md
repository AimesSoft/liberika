# Issue #125: resource lifecycle fix and validation

Translation of the [Chinese record](issue-125-verification.zh.md). Validation environment: 2026-09-28, macOS / Apple Silicon, branch based on commit `ca40a95`. Issue: https://github.com/AimesSoft/liberika/issues/125

## Conclusion

The problem existed, but two behaviors must be distinguished:

- `stop()` intentionally retained the playback session and decoder for replay from the beginning; retaining some memory after stop matched existing semantics.
- The persistent HTTP read-ahead worker and asynchronous demux worker lacked deterministic ownership cleanup. The HTTP worker's `JoinHandle` was discarded and destruction only set a flag, which could not be observed while blocked on response headers or body. `AsyncDemuxer` likewise saved no thread handle and destruction only sent `Stop`. Connections, threads and media sources could therefore outlive destruction of the object. This lifecycle defect was reproduced.

The full “hundreds of MB retained” curve was not reproduced in the original author's host application, and a Swift strong-reference leak was not established. Process footprint failing to return immediately to startup levels does not by itself prove that objects remain live.

## Fix

- Foreground Range requests and persistent read-ahead use a shared reqwest/Tokio I/O runtime. Each media source has a generation-aware cancellation token; cancellation drops the request future, interrupting connect, TLS, response-header and response-body waits. DNS work already inside the system blocking resolver may finish independently; synchronous DNS termination is not claimed.
- Foreground requests wait at most 15 seconds for headers and 60 seconds without body data. Each request is also limited by the remaining overall fetch budget; retries, backoff and HEAD-to-GET fallback total at most 120 seconds. Fallback GET uses the smaller of the remaining budget and header timeout.
- The persistent stream retains the existing single-connection, stripe-handoff and window-backpressure design. Each stream session stores the worker `JoinHandle`; session `Drop` cancels I/O, wakes and waits for the worker. Every worker exit path notifies foreground threads waiting for prefetch, avoiding waits after socket closure.
- `AsyncDemuxer` stores a `JoinHandle` and media-source cancellation handle. Seek interrupts reads from the old generation; the new generation resumes only after the worker processes the seek. Destruction performs terminal cancellation and waits for the thread, draining the packet channel while waiting to avoid a worker blocked on a full queue.
- `stop()` remains replayable, but releases the HTTP cache, persistent read-ahead and empty playback queue capacity before returning. Existing `open()` replaces the old session; `close()` / destroy release media resources. No half-close API is added.

Changes are limited to source, demux lifecycle and required dependencies. reqwest/Tokio increase dependency size but allow synchronous FFmpeg reads to be canceled while network I/O blocks; saving and joining the thread alone would bring the network wait into destruction. Public lifecycle interfaces for C API, JNI, JSON bridge, Player and Presenter remain unchanged. No malloc pressure relief or forced allocator reclamation was used to conceal live resources.

## Experiments

### Real TCP control before the fix

A local server returned valid 206 headers declaring an 8 MiB body but sent no body. On then-current `main`, an `HttpRangeSource` started the persistent stream and was immediately destroyed. The server did not observe EOF within 500 ms after destruction returned, and the regression consistently failed. This demonstrated that the worker and socket outlived the source object.

After the fix the same test consistently passed; source destruction waits for socket closure and worker exit.

### Cancellation, replay and close

`crates/erika/tests/media_lifecycle.rs` uses real TCP/HTTP connections and the repository MKV fixture to check:

- Cancellation while HEAD, response headers, response body or TLS handshake is blocked; the server observes EOF/reset.
- Prefetch cancellation on source destruction or `release_buffer()`; subsequent reads still work and retain authentication headers.
- A real Player performing consecutive seeks during a blocked HTTP read, replay after stop, replacing an old session with open, and close.
- Demux destruction waits for media-source release without deadlocking even when the packet queue is full.

In this run TLS, HEAD, response-header and response-body cancellation took 0.17, 0.04, 0.06 and 0.08 ms respectively; Player stop, open replacing the old session and close took 0.01, 3.38 and 0.23 ms. The test limit was 500 ms; these local figures are not cross-device latency guarantees.

The additional `cancellation_wakes_a_reader_waiting_for_prefetch` regression failed on previous revision `9c8300c`, receiving no cancellation result within 500 ms. It passed after adding exit notifications and using the same lock for condition checking and waiting.

### Timeouts and retries

Six real TCP tests use private short timeout parameters to verify the same execution path as production:

- After HEAD returns 405 or zero length, fallback GET inherits the total budget already consumed by HEAD retries.
- A stalled first HEAD or GET triggers a per-response timeout and actually sends a second request to obtain the result.
- If a body stalls after partial delivery, the old connection is released and the transfer resumes at the received offset with `If-Range`.
- A continuously arriving body may exceed the header wait timeout, but cannot exceed the total budget even while data keeps arriving.

The first five all failed with the original timeout logic and only short test budgets injected; after the fix all six passed. Lifecycle tests treat EOF, `ConnectionReset` and `ConnectionAborted` as valid disconnections, covering the abort type Windows may return. This run was on macOS; Windows remains to be revalidated.

### Memory

A single-process macOS experiment used the repository MKV, software decoding and a 64 KiB HTTP window for 12 cycles of create player → open → play → stop → close. Frames held by the test host were released before each sample.

| Stage | Live malloc bytes | Footprint bytes |
|---|---:|---:|
| Initial baseline | 621,648 | 3,260,800 |
| First play | 2,341,024 | 7,356,848 |
| First stop | 1,675,312 | 7,979,440 |
| First close | 717,552 | 7,881,112 |
| Twelfth stop | 1,675,312 | 8,815,024 |
| Twelfth close | 717,552 | 8,700,312 |

Live malloc after all 12 closes was **717,552 bytes**, with no cycle-by-cycle accumulation. Footprint did not return to the initial baseline, consistent with the allocator, shared HTTP runtime and system frameworks retaining virtual/physical pages; footprint alone cannot establish a leak.

## Verification commands

```sh
cargo test -p erika -p erika_capi --lib
cargo test -p erika --test media_lifecycle -- --nocapture --test-threads=1
cargo test -p erika --test media_lifecycle memory_probe -- --ignored --nocapture --test-threads=1
```

Results: all 681 liberika library tests and 41 C API tests passed; four lifecycle integration tests and one manual memory experiment passed.

## Limits

The original author's host, long high-bitrate public HTTPS sessions, complete VideoToolbox/Metal memory curves and builds for every target platform were not covered. Frames already delivered to the host must still be released by the host; third-party custom `MediaSource` implementations with blocking I/O must supply their own cancellation handle. CI must still validate the new Rust dependencies on each platform.
