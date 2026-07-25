# What does reaching calamine over gRPC cost?

A library call is free. A socket is not. This harness measures the difference
on a real workbook, in a way built to be hard to accuse of stacking the deck.

Captured runs live in [RESULTS.md](RESULTS.md), dated and hardware-stamped,
with addresses rewritten to documentation ranges. They describe one machine,
one file and one day. When the code changes enough for them to drift, rerun
and replace them; don't quote them against different hardware.

```bash
cargo build --release        # from the repository root, builds the server
cd bench
cargo run --release -- /path/to/workbook.xlsx [sheet] [iterations]
```

The harness starts its own `grpc-calamine` on port 50077, uploads the workbook
once, runs every arm, and kills the server on the way out. Nothing else needs
to be running. Without a sheet name it picks the largest sheet, since sheet 0
of a big workbook is often a small cover sheet.

## The four arms

Each arm adds exactly one layer, so the marginal cost of each is the answer to
"where does the time go".

| Arm | Does |
|---|---|
| 0 | calamine alone: walk the sheet's populated cells and touch every value |
| 1 | + densify into the canonical grid (dense rows, explicit empties, gap rows) |
| 2 | + convert to protobuf and prost-encode each row, exactly as the server does |
| 3 | + push it through a socket and decode it in a separate client process |

Arm 2 is everything the server does, in one thread, with no transport. Arm 3 is
the same work delivered over gRPC. The gap between them is the surface's price.

## Why you can trust that the arms did the same work

The obvious way to win a benchmark like this is to quietly let one side do
less. So arms 1, 2 and 3 each feed an order-sensitive digest of the cell
stream, and **the harness aborts instead of printing numbers if any two
disagree**:

```
same-work proof (digest of the canonical cell stream)
  1 native dense                     <digest>/<rows>r/<cells>c
  2 native dense + protobuf encode   <digest>/<rows>r/<cells>c
  3 gRPC end to end                  <digest>/<rows>r/<cells>c
  arms 1-3 identical: yes
```

The digest covers row indices and every cell's tag and payload, so skipping a
cell, dropping an empty, reordering rows or truncating the grid all change it.
It has earned its keep: it caught an early version of arm 1 that collapsed
`shared_string` into `string`, and it caught the server streaming tens of
thousands of trailing styled-blank rows that calamine itself trims. That
second catch became a server fix and a regression test
(`tests/streaming.rs`), which is the point of the gate.

Arm 0 is deliberately **not** gated. It walks only populated cells and hands
back borrowed `&str`, so it is doing different and strictly less work than the
others. It is reported as a floor, never as a peer.

## What is controlled

- **Arms are interleaved** inside each iteration, so clock drift, boost and
  thermals hit every arm equally instead of accumulating in whichever ran
  last.
- **Reported as min / median / p95**, never a bare mean.
- **CPU seconds are printed beside wall clock.** This matters more than
  anything else here: gRPC spreads work over a parser thread, tokio workers
  and a separate client process, so wall clock alone would credit it for
  using more cores. Read the two columns together.
- **Wire size is counted at the socket.** The client connects through a
  byte-counting stream, so the reported wire bytes are what actually crossed
  the TCP connection, HTTP/2 framing included. The encoded protobuf payload
  (from arm 2) is printed beside it; with compression on, the two diverge and
  the socket is the honest one.
- **Identical calamine build.** The harness is a path dependency on the
  server crate, so both arms link the same calamine version, the same feature
  set and the same conversion code. It is not in the workspace, so it never
  affects CI.
- **Same I/O path.** Both arms parse from an in-memory `Cursor<Arc<[u8]>>`.
  The native arm never gets file or `BufReader` semantics the server does
  not.
- **The upload leg is reported separately**, because folding it in always
  flatters the native arm and leaving it out always flatters gRPC.
- **Dead code cannot be eliminated**, because every arm's values flow into
  the digest that is printed.

## What is not controlled, and you should know it

- **Client and server share a machine** in the default mode. They contend for
  the same cores and talk over loopback, which makes wire bytes look free; a
  real network is what makes the expansion matter. Use `BENCH_ADDR` for that.
- **No core pinning.** Results move a few percent between runs, more on CPUs
  with asymmetric cache domains.
- **One workbook, one machine per run.** The shape of the file — above all
  how much of it is shared strings — moves the results more than its size
  does.
- **The digest costs real time**, and every arm pays it. It compresses the
  ratios slightly toward each other.

## Knobs

| Variable | Default | Meaning |
|---|---|---|
| `WINDOW_BYTES` | 50 MiB | Client HTTP/2 stream and connection window. `0` keeps hyper's 1 MiB default, so the two can be compared. |
| `BATCH` | `0` (server default) | `max_rows_per_message` for arm 3. `1` disables batching — one row per message — which is how the batching decision in the contract was measured. |
| `BENCH_COMPRESSION` | `none` | `grpc-encoding` the client requests for the row stream: `none`, `gzip` or `zstd`. The upload stays uncompressed either way; a workbook is already deflated. |
| `BENCH_DICT` | off | `1` opts arm 3 into the contract's `use_string_table` mode: shared strings arrive once in table chunks and as ids per cell, resolved by the client. The digest gate holds the resolved stream to the same canonical cells as every other arm. Composes with `BENCH_COMPRESSION`. |
| `BENCH_ADDR` | unset | `host:port` of an already-running server. Arms 0–2 stay local, arm 3 crosses the real network. |

Compression and the dictionary attack the same expansion from different
levels: generic compression captures string repetition without touching the
contract, at a CPU price, while `use_string_table` removes the repetition at
the schema level. Run `none`, `zstd`, `BENCH_DICT=1`, and the combination
back to back, and read the wall clock, the CPU column and the socket bytes
together before concluding anything.

### Over a real network

Loopback is the weakest part of the default setup. `BENCH_ADDR=host:port`
points arm 3 at a remote server while arms 0–2 stay local, so the comparison
becomes "parse it here versus have that machine parse it". In remote mode the
CPU column for arm 3 counts the client process only — the server's `/proc` is
not readable from another machine — so do not compare it against local runs,
where that column is client plus server.

To find where the wire overtakes the parse as the bottleneck, shape the
server's egress and sweep the rate:

```bash
# on the server host, e.g. 1 Gbit/s
sudo tc qdisc add dev <nic> root tbf rate 1gbit burst 4mb latency 300ms
# ... run the bench from the client host ...
sudo tc qdisc del dev <nic> root
```

Below the crossover, wall clock is simply bytes divided by bandwidth, which is
what makes the wire expansion — and anything that reduces it — matter.

## The packed-buffer experiment

```bash
cargo run --release --bin packed -- <workbook.xlsx> [sheet] [rows-per-batch]
```

Answers a different question: how much of the wire expansion is the per-cell
protobuf framing, and how much is the string payload itself? It encodes the
identical cell stream three ways and requires all three to decode back to a
digest matching the in-memory grid, so no format can post a number by quietly
dropping something.

- **A. contract** — `WorksheetRowBatch` exactly as the server sends it.
- **B. packed** — the same cells as flat little-endian arrays in one `bytes`
  blob. No per-cell submessage, no per-cell length prefix.
- **C. packed + dictionary** — B, except shared strings become `u32` indices
  into a table sent once per stream. This is the deduplication XLSX itself
  performs and the wire format currently undoes.

The dictionary table is built during the parse walk by pointer identity
(`DataRef::SharedString` borrows into the workbook's own shared-strings
table), so the encode column prices the dictionary's steady state. The
one-time cost of deriving the table is measured and printed separately, both
by pointer identity and by content hashing, next to what would make it free:
calamine already holds the resolved table in memory and exposes no accessor
for it.

B and C are hand-rolled buffers and stop being self-describing — protobuf can
no longer evolve them field by field, byte order becomes part of the
specification, and every client needs a hand-written decoder. That price is
the reason this is an experiment and not the contract.

## The other-language arms

The question a user actually faces is "my data is on that server: do I pull it
over gRPC, pull it as JSON, or just parse the file myself?" These reproduce
the comparison from the consumer's side, each feeding a digest compatible with
the harness so cross-implementation agreement is checkable:

- **Python** — `demos/python-client/_pybench.py` runs python-calamine on a
  local file, grpc-calamine over the wire, and the same rows as NDJSON over
  HTTP. One hard-won caveat is written into it: the verification digest must
  be cheaper than the parsers under test, or it becomes the thing being
  measured.
- **Rust NDJSON** — `cargo run --release --bin jsonclient` pulls the NDJSON
  arm with a fast client, for when the interpreter is no longer the
  bottleneck and the format difference can show.
- **Java** — `demos/java-client`'s `demo.Timing` measures client-side
  dispatch configurations (blocking vs async stub, executor choice, batched
  vs row-per-message), which is where grpc-java clients win or lose the most.

## Captured results

[RESULTS.md](RESULTS.md), with the hardware, link speed and workbook shape
stated next to every table. The charts the README embeds are rendered from
those captures by [`charts.py`](charts.py) into [`charts/`](charts);
regenerate them whenever the captures change. Machine-specific settings
for reruns (addresses, workbook paths) go in `bench/.env`, which is
gitignored; copy [`.env.example`](.env.example) and edit. Real environment
variables override the file.
