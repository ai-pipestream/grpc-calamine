# Captured results

Captured 2026-07-23/24. These numbers describe one machine, one workbook and
one day; rerun the harness (see [README.md](README.md)) rather than quoting
them against different hardware. IP addresses in the captures are rewritten
to documentation ranges; the network runs used a real 10 GbE LAN and a real
WireGuard (Tailscale) path between two machines.

Host: AMD Ryzen 9 9950X3D, 32 logical cores, Linux 7.0.0-28-generic.
Second machine (network runs): AMD Ryzen 9 9950X, 32 logical cores, 10 GbE.
Build: release profile, cargo defaults (no `[profile.release]` overrides).
Workbook: 105,709,047 bytes, 4 sheets; `Worksheet` is the largest.

The grid is 985,351 rows x 8 cols. That is what calamine's own
`worksheet_range` reports for this sheet: the declared `<dimension>` claims
1,043,928 rows, but the last 58,577 hold only blanks and `Range::from_sparse`
trims them.

```
grpc-calamine: in-process calamine vs the same work over gRPC

workbook    : 100mb.xlsx
size        : 105.7 MB
sheet       : Worksheet   (of 4: Worksheet (2), Worksheet (3), Tablo3, Worksheet)
iterations  : 5, arms interleaved
host        : 32 logical cores
profile     : release (cargo defaults; no [profile.release] overrides)

client http2 window: 52428800 bytes


  iteration 1/5
  iteration 2/5
  iteration 3/5
  iteration 4/5
  iteration 5/5
                        
same-work proof (digest of the canonical cell stream)
  0 calamine sparse walk             7b7ae3deea46e84e/7888232r/7888232c
  1 native dense                     b1ec698bf30cde49/985351r/7882808c
  2 native dense + protobuf encode   b1ec698bf30cde49/985351r/7882808c
  3 gRPC end to end                  b1ec698bf30cde49/985351r/7882808c

  arms 1-3 identical: yes
  arm 0 walks only populated cells (7888232 vs 7882808), so it is not comparable and is shown as a floor.

wall clock ms (min / median / p95), and CPU seconds burned per run
                                               min      med      p95     CPU s
  0  calamine alone, populated cells only    2082.7   2097.1   2106.7      2.12
  1  + dense canonical grid                  2310.6   2314.1   2327.8      2.34
  2  + protobuf convert and encode           2417.2   2428.0   2441.2      2.45
  3  + gRPC socket, decoded by the client    1799.5   1836.9   1878.1      4.03

where the in-process time goes (arms 0-2 are one serial thread)
  calamine parse, the floor                  2097.1 ms   86.4%
  densify into the contract's grid            217.0 ms    8.9%
  protobuf convert + encode                   114.0 ms    4.7%
  total the server must do per read          2428.0 ms

what the gRPC surface actually costs
  same work, one thread, in process (2)      2428.0 ms wall    2.45 s CPU
  same work, over the wire (3)               1836.9 ms wall    4.03 s CPU
  wall clock                                 -591.1 ms  (-24.3%)
  CPU                                         +1.57 s   (1.64x)
  parallelism used by arm 3                    2.19 cores (CPU / wall)

  Read the two columns together. gRPC finishes sooner in wall clock because
  the server parses while the client decodes, on different cores; it is not
  doing less work. The honest cost of the surface is the CPU column and the
  bytes below, not latency.

  versus plain calamine in your own process (arm 0):
    wall -260.2 ms (0.88x), CPU +1.90 s (1.90x)
  server peak RSS                             296.1 MiB

shape and wire
  rows                                     985351
  cells (dense)                            7882808
  protobuf on the wire                        788.9 MB
  expansion over the source file               7.46x
  messages on the stream                   3850  (255.9 rows each)
  throughput (arm 3, median)                 536415 rows/s, 4291323 cells/s

latency to the first row
  gRPC stream (min / median / p95)              1.3      1.5      1.8 ms
  a batch API cannot answer before           2428.0 ms  (arm 2 completes)

upload leg, counted separately
  OpenWorkbook for 105.7 MB                  492.5 ms  (215 MB/s)
  paid once per workbook, then any number of reads reuse the handle.

```

## `max_rows_per_message = 1`

```
grpc-calamine: in-process calamine vs the same work over gRPC

workbook    : 100mb.xlsx
size        : 105.7 MB
sheet       : Worksheet   (of 4: Worksheet (2), Worksheet (3), Tablo3, Worksheet)
iterations  : 5, arms interleaved
host        : 32 logical cores
profile     : release (cargo defaults; no [profile.release] overrides)

client http2 window: 52428800 bytes


  iteration 1/5
  iteration 2/5
  iteration 3/5
  iteration 4/5
  iteration 5/5
                        
same-work proof (digest of the canonical cell stream)
  0 calamine sparse walk             7b7ae3deea46e84e/7888232r/7888232c
  1 native dense                     b1ec698bf30cde49/985351r/7882808c
  2 native dense + protobuf encode   b1ec698bf30cde49/985351r/7882808c
  3 gRPC end to end                  b1ec698bf30cde49/985351r/7882808c

  arms 1-3 identical: yes
  arm 0 walks only populated cells (7888232 vs 7882808), so it is not comparable and is shown as a floor.

wall clock ms (min / median / p95), and CPU seconds burned per run
                                               min      med      p95     CPU s
  0  calamine alone, populated cells only    2089.1   2109.1   2124.1      2.13
  1  + dense canonical grid                  2313.5   2342.2   2350.0      2.36
  2  + protobuf convert and encode           2409.9   2428.9   2449.8      2.45
  3  + gRPC socket, decoded by the client    2018.7   2059.2   2194.1      7.05

where the in-process time goes (arms 0-2 are one serial thread)
  calamine parse, the floor                  2109.1 ms   86.8%
  densify into the contract's grid            233.0 ms    9.6%
  protobuf convert + encode                    86.7 ms    3.6%
  total the server must do per read          2428.9 ms

what the gRPC surface actually costs
  same work, one thread, in process (2)      2428.9 ms wall    2.45 s CPU
  same work, over the wire (3)               2059.2 ms wall    7.05 s CPU
  wall clock                                 -369.7 ms  (-15.2%)
  CPU                                         +4.60 s   (2.87x)
  parallelism used by arm 3                    3.43 cores (CPU / wall)

  Read the two columns together. gRPC finishes sooner in wall clock because
  the server parses while the client decodes, on different cores; it is not
  doing less work. The honest cost of the surface is the CPU column and the
  bytes below, not latency.

  versus plain calamine in your own process (arm 0):
    wall -49.9 ms (0.98x), CPU +4.92 s (3.31x)
  server peak RSS                             291.8 MiB

shape and wire
  rows                                     985351
  cells (dense)                            7882808
  protobuf on the wire                        788.9 MB
  expansion over the source file               7.46x
  messages on the stream                   985351  (1.0 rows each)
  throughput (arm 3, median)                 478509 rows/s, 3828074 cells/s

latency to the first row
  gRPC stream (min / median / p95)              0.3      0.4      0.4 ms
  a batch API cannot answer before           2428.9 ms  (arm 2 completes)

upload leg, counted separately
  OpenWorkbook for 105.7 MB                  499.2 ms  (212 MB/s)
  paid once per workbook, then any number of reads reuse the handle.

```

## Packed-buffer experiment

Updated 2026-07-24: the walk now mirrors the server's canonical grid (trimmed
to 985,351 rows, digest-identical to the main harness), and the dictionary is
built during the walk by pointer identity, so the encode column prices the
dictionary's steady state and the table-derivation cost is reported
separately.

```
packed-buffer experiment: what does per-cell framing cost?

workbook   : 100mb.xlsx
sheet      : Worksheet
batch      : 256 rows per message

grid       : 985351 rows x 8 cols = 7882808 cells
messages   : 3850

lossless proof (digest of the decoded cell stream)
  source grid             b1ec698bf30cde49/985351r/7882808c
  A contract              b1ec698bf30cde49/985351r/7882808c
  B packed                b1ec698bf30cde49/985351r/7882808c
  C packed + dictionary   b1ec698bf30cde49/985351r/7882808c
  all match the source grid: yes

wire size
  source .xlsx                          105.7 MB
  A contract (CellData per cell)        788.9 MB    7.46x source    100.1 B/cell
  B packed arrays                       786.3 MB    7.44x source     99.8 B/cell    -0.3% vs A
  C packed + dictionary                 192.7 MB    1.82x source     24.4 B/cell   -75.6% vs A
      of which the table, sent once     121.7 MB   (1047814 unique strings)

CPU, ms for the whole sheet
                                        encode    decode
  A contract                             402.8    1017.2
  B packed                               141.5     545.3   (-65% / -46%)
  C packed + dictionary                   23.9     592.5   (-94% / -42%)

shared-string table derivation, ms (not part of the encode column)
  by pointer identity, inside the walk     805.8   (walk 2854.6 ms with it, 2048.8 ms without)
  by content hash, after the fact         1022.7   (1047814 unique strings; table itself 1047814 entries)
  Pointer identity (the `DataRef::SharedString` borrows all point into the
  workbook's own table) hashes two machine words per cell; content hashing
  walks every string body. Measured, the two cost about the same, because
  the bill is materializing the table -- one allocation and copy per unique
  string -- not the hashing that finds it. calamine already holds this exact
  table in memory and resolves indices into it during the parse; an upstream
  accessor exposing it would make the derivation cost zero.

note: B and C are hand-rolled little-endian buffers, so they carry no
type information and no field numbers. That is the whole point of the
experiment, and also the whole cost: the contract stops being
self-describing, protobuf cannot evolve it field by field, and every
client needs a hand-written decoder that agrees on byte order.
```

## Over a network (second machine, 10 GbE LAN)

Server on a second machine; arms 0-2 remain local. The CPU column for arm 3
is client-only here, since the server is on another host.

```
### LAN 10GbE (192.0.2.10), batched
grpc-calamine: in-process calamine vs the same work over gRPC

workbook    : 100mb.xlsx
size        : 105.7 MB
sheet       : Worksheet   (of 4: Worksheet (2), Worksheet (3), Tablo3, Worksheet)
iterations  : 3, arms interleaved
host        : 32 logical cores
profile     : release (cargo defaults; no [profile.release] overrides)

client http2 window: 52428800 bytes

server     : remote at 192.0.2.10:50055 (arms 0-2 remain local)


  iteration 1/3
  iteration 2/3
  iteration 3/3
                        
same-work proof (digest of the canonical cell stream)
  0 calamine sparse walk             7b7ae3deea46e84e/7888232r/7888232c
  1 native dense                     b1ec698bf30cde49/985351r/7882808c
  2 native dense + protobuf encode   b1ec698bf30cde49/985351r/7882808c
  3 gRPC end to end                  b1ec698bf30cde49/985351r/7882808c

  arms 1-3 identical: yes
  arm 0 walks only populated cells (7888232 vs 7882808), so it is not comparable and is shown as a floor.

wall clock ms (min / median / p95), and CPU seconds burned per run
                                               min      med      p95     CPU s
  0  calamine alone, populated cells only    2135.6   2153.9   2179.6      2.18
  1  + dense canonical grid                  2390.1   2393.9   2399.9      2.42
  2  + protobuf convert and encode           2493.0   2495.0   2523.1      2.53
  3  + gRPC socket, decoded by the client    1726.3   1729.9   1758.7      1.32

where the in-process time goes (arms 0-2 are one serial thread)
  calamine parse, the floor                  2153.9 ms   86.3%
  densify into the contract's grid            240.0 ms    9.6%
  protobuf convert + encode                   101.2 ms    4.1%
  total the server must do per read          2495.0 ms

what the gRPC surface actually costs
  same work, one thread, in process (2)      2495.0 ms wall    2.53 s CPU
  same work, over the wire (3)               1729.9 ms wall    1.32 s CPU
  wall clock                                 -765.1 ms  (-30.7%)
  CPU                                         -1.21 s   (0.52x)
  parallelism used by arm 3                    0.76 cores (CPU / wall)

  Read the two columns together. gRPC finishes sooner in wall clock because
  the server parses while the client decodes, on different cores; it is not
  doing less work. The honest cost of the surface is the CPU column and the
  bytes below, not latency.

  versus plain calamine in your own process (arm 0):
    wall -424.0 ms (0.80x), CPU -0.86 s (0.61x)
  server peak RSS                               0.0 MiB

shape and wire
  rows                                     985351
  cells (dense)                            7882808
  protobuf on the wire                        788.9 MB
  expansion over the source file               7.46x
  messages on the stream                   3850  (255.9 rows each)
  throughput (arm 3, median)                 569605 rows/s, 4556841 cells/s

latency to the first row
  gRPC stream (min / median / p95)              2.3      2.3      2.4 ms
  a batch API cannot answer before           2495.0 ms  (arm 2 completes)

upload leg, counted separately
  OpenWorkbook for 105.7 MB                  631.3 ms  (167 MB/s)
  paid once per workbook, then any number of reads reuse the handle.

### LAN, max_rows_per_message=1
grpc-calamine: in-process calamine vs the same work over gRPC

workbook    : 100mb.xlsx
size        : 105.7 MB
sheet       : Worksheet   (of 4: Worksheet (2), Worksheet (3), Tablo3, Worksheet)
iterations  : 3, arms interleaved
host        : 32 logical cores
profile     : release (cargo defaults; no [profile.release] overrides)

client http2 window: 52428800 bytes

server     : remote at 192.0.2.10:50055 (arms 0-2 remain local)


  iteration 1/3
  iteration 2/3
  iteration 3/3
                        
same-work proof (digest of the canonical cell stream)
  0 calamine sparse walk             7b7ae3deea46e84e/7888232r/7888232c
  1 native dense                     b1ec698bf30cde49/985351r/7882808c
  2 native dense + protobuf encode   b1ec698bf30cde49/985351r/7882808c
  3 gRPC end to end                  b1ec698bf30cde49/985351r/7882808c

  arms 1-3 identical: yes
  arm 0 walks only populated cells (7888232 vs 7882808), so it is not comparable and is shown as a floor.

wall clock ms (min / median / p95), and CPU seconds burned per run
                                               min      med      p95     CPU s
  0  calamine alone, populated cells only    2152.1   2165.7   2174.6      2.19
  1  + dense canonical grid                  2397.0   2400.0   2449.9      2.45
  2  + protobuf convert and encode           2488.8   2526.0   2708.2      2.61
  3  + gRPC socket, decoded by the client    1922.3   1960.6   1999.4      1.61

where the in-process time goes (arms 0-2 are one serial thread)
  calamine parse, the floor                  2165.7 ms   85.7%
  densify into the contract's grid            234.3 ms    9.3%
  protobuf convert + encode                   126.0 ms    5.0%
  total the server must do per read          2526.0 ms

what the gRPC surface actually costs
  same work, one thread, in process (2)      2526.0 ms wall    2.61 s CPU
  same work, over the wire (3)               1960.6 ms wall    1.61 s CPU
  wall clock                                 -565.3 ms  (-22.4%)
  CPU                                         -1.00 s   (0.62x)
  parallelism used by arm 3                    0.82 cores (CPU / wall)

  Read the two columns together. gRPC finishes sooner in wall clock because
  the server parses while the client decodes, on different cores; it is not
  doing less work. The honest cost of the surface is the CPU column and the
  bytes below, not latency.

  versus plain calamine in your own process (arm 0):
    wall -205.0 ms (0.91x), CPU -0.58 s (0.74x)
  server peak RSS                               0.0 MiB

shape and wire
  rows                                     985351
  cells (dense)                            7882808
  protobuf on the wire                        788.9 MB
  expansion over the source file               7.46x
  messages on the stream                   985351  (1.0 rows each)
  throughput (arm 3, median)                 502564 rows/s, 4020512 cells/s

latency to the first row
  gRPC stream (min / median / p95)              0.5      0.7      0.7 ms
  a batch API cannot answer before           2526.0 ms  (arm 2 completes)

upload leg, counted separately
  OpenWorkbook for 105.7 MB                  627.8 ms  (168 MB/s)
  paid once per workbook, then any number of reads reuse the handle.

### Tailscale (100.64.0.10), batched
grpc-calamine: in-process calamine vs the same work over gRPC

workbook    : 100mb.xlsx
size        : 105.7 MB
sheet       : Worksheet   (of 4: Worksheet (2), Worksheet (3), Tablo3, Worksheet)
iterations  : 3, arms interleaved
host        : 32 logical cores
profile     : release (cargo defaults; no [profile.release] overrides)

client http2 window: 52428800 bytes

server     : remote at 100.64.0.10:50055 (arms 0-2 remain local)


  iteration 1/3
  iteration 2/3
  iteration 3/3
                        
same-work proof (digest of the canonical cell stream)
  0 calamine sparse walk             7b7ae3deea46e84e/7888232r/7888232c
  1 native dense                     b1ec698bf30cde49/985351r/7882808c
  2 native dense + protobuf encode   b1ec698bf30cde49/985351r/7882808c
  3 gRPC end to end                  b1ec698bf30cde49/985351r/7882808c

  arms 1-3 identical: yes
  arm 0 walks only populated cells (7888232 vs 7882808), so it is not comparable and is shown as a floor.

wall clock ms (min / median / p95), and CPU seconds burned per run
                                               min      med      p95     CPU s
  0  calamine alone, populated cells only    2181.9   2194.8   2264.7      2.24
  1  + dense canonical grid                  2370.5   2385.4   2393.6      2.41
  2  + protobuf convert and encode           2458.2   2474.1   2478.8      2.49
  3  + gRPC socket, decoded by the client    1915.5   1919.6   1930.6      1.41

where the in-process time goes (arms 0-2 are one serial thread)
  calamine parse, the floor                  2194.8 ms   88.7%
  densify into the contract's grid            190.7 ms    7.7%
  protobuf convert + encode                    88.6 ms    3.6%
  total the server must do per read          2474.1 ms

what the gRPC surface actually costs
  same work, one thread, in process (2)      2474.1 ms wall    2.49 s CPU
  same work, over the wire (3)               1919.6 ms wall    1.41 s CPU
  wall clock                                 -554.5 ms  (-22.4%)
  CPU                                         -1.08 s   (0.57x)
  parallelism used by arm 3                    0.73 cores (CPU / wall)

  Read the two columns together. gRPC finishes sooner in wall clock because
  the server parses while the client decodes, on different cores; it is not
  doing less work. The honest cost of the surface is the CPU column and the
  bytes below, not latency.

  versus plain calamine in your own process (arm 0):
    wall -275.2 ms (0.87x), CPU -0.83 s (0.63x)
  server peak RSS                               0.0 MiB

shape and wire
  rows                                     985351
  cells (dense)                            7882808
  protobuf on the wire                        788.9 MB
  expansion over the source file               7.46x
  messages on the stream                   3850  (255.9 rows each)
  throughput (arm 3, median)                 513311 rows/s, 4106487 cells/s

latency to the first row
  gRPC stream (min / median / p95)              3.3      3.6      3.8 ms
  a batch API cannot answer before           2474.1 ms  (arm 2 completes)

upload leg, counted separately
  OpenWorkbook for 105.7 MB                  795.7 ms  (133 MB/s)
  paid once per workbook, then any number of reads reuse the handle.
```

### Shaped links

Same setup with `tc tbf` on the server egress, to find where bytes overtake
the parse as the bottleneck.

```
### shaped to 2500mbit
qdisc tbf 8001: root refcnt 33 rate 2500Mbit burst 4Mb lat 300ms 
  arms 1-3 identical: yes
  3  + gRPC socket, decoded by the client    2629.7   2629.8   2629.9      1.40
  OpenWorkbook for 105.7 MB                  630.4 ms  (168 MB/s)

### shaped to 1000mbit
qdisc tbf 8001: root refcnt 33 rate 1Gbit burst 4Mb lat 300ms 
  arms 1-3 identical: yes
  3  + gRPC socket, decoded by the client    6571.1   6571.1   6571.1      1.31
  OpenWorkbook for 105.7 MB                  626.3 ms  (169 MB/s)

### shaped to 250mbit
qdisc tbf 8001: root refcnt 33 rate 250Mbit burst 4Mb lat 300ms 
  arms 1-3 identical: yes
  3  + gRPC socket, decoded by the client   26277.8  26277.8  26277.8      1.50
  OpenWorkbook for 105.7 MB                  619.3 ms  (171 MB/s)

```

## Client on the far machine (server here, client on the second machine)

### Python client
```
client on the second machine, server 192.0.2.20

L2 python-calamine (local file)         5283 ms     186526 rows/s  000000002203c957/985351r/7882808c
N1 grpc-calamine over gRPC              4730 ms     208320 rows/s  000000002203c957/985351r/7882808c  upload 652 ms
N2 same rows as NDJSON/HTTP             4925 ms     200057 rows/s  000000002203c957/985351r/7882808c  983 MB downloaded
```

### Rust gRPC client
```
  arms 1-3 identical: yes
  0  calamine alone, populated cells only    2104.7   2105.0   2109.9      2.13
  1  + dense canonical grid                  2317.0   2332.4   2333.5      2.35
  2  + protobuf convert and encode           2433.9   2435.9   2455.7      2.47
  3  + gRPC socket, decoded by the client    2081.0   2126.4   2207.4      1.36
  wall clock                                 -309.6 ms  (-12.7%)
  CPU                                         -1.11 s   (0.55x)
  protobuf on the wire                        788.9 MB
  messages on the stream                   3850  (255.9 rows each)
  OpenWorkbook for 105.7 MB                  607.0 ms  (174 MB/s)
```

### Rust NDJSON client (same rows, no gRPC in the path)
```
rust NDJSON/HTTP       2828 ms     348396 rows/s  000000002203c957/985351r/7882808c  983 MB
rust NDJSON/HTTP       2829 ms     348334 rows/s  000000002203c957/985351r/7882808c  983 MB
rust NDJSON/HTTP       2830 ms     348222 rows/s  000000002203c957/985351r/7882808c  983 MB
```

## Compression (loopback, 2026-07-24)

Same workbook and sheet, 3 iterations, server and client on the one box.
`BENCH_COMPRESSION` sets the grpc-encoding the client requests for the row
stream; wire size is counted at the socket.

```
### BENCH_COMPRESSION=none
grpc-calamine: in-process calamine vs the same work over gRPC

workbook    : 100mb.xlsx
size        : 105.7 MB
sheet       : Worksheet   (of 4: Worksheet (2), Worksheet (3), Tablo3, Worksheet)
iterations  : 3, arms interleaved
host        : 32 logical cores
profile     : release (cargo defaults; no [profile.release] overrides)

client http2 window: 52428800 bytes
compression : none (grpc-encoding for the row stream)


  iteration 1/3
  iteration 2/3
  iteration 3/3
                        
same-work proof (digest of the canonical cell stream)
  0 calamine sparse walk             7b7ae3deea46e84e/7888232r/7888232c
  1 native dense                     b1ec698bf30cde49/985351r/7882808c
  2 native dense + protobuf encode   b1ec698bf30cde49/985351r/7882808c
  3 gRPC end to end                  b1ec698bf30cde49/985351r/7882808c

  arms 1-3 identical: yes
  arm 0 walks only populated cells (7888232 vs 7882808), so it is not comparable and is shown as a floor.

wall clock ms (min / median / p95), and CPU seconds burned per run
                                               min      med      p95     CPU s
  0  calamine alone, populated cells only    2096.0   2119.9   2129.1      2.14
  1  + dense canonical grid                  2331.1   2341.6   2353.9      2.37
  2  + protobuf convert and encode           2445.0   2459.3   2461.8      2.48
  3  + gRPC socket, decoded by the client    1839.7   1863.4   1864.2      4.03

where the in-process time goes (arms 0-2 are one serial thread)
  calamine parse, the floor                  2119.9 ms   86.2%
  densify into the contract's grid            221.7 ms    9.0%
  protobuf convert + encode                   117.7 ms    4.8%
  total the server must do per read          2459.3 ms

what the gRPC surface actually costs
  same work, one thread, in process (2)      2459.3 ms wall    2.48 s CPU
  same work, over the wire (3)               1863.4 ms wall    4.03 s CPU
  wall clock                                 -595.9 ms  (-24.2%)
  CPU                                         +1.55 s   (1.62x)
  parallelism used by arm 3                    2.16 cores (CPU / wall)

  Read the two columns together. gRPC finishes sooner in wall clock because
  the server parses while the client decodes, on different cores; it is not
  doing less work. The honest cost of the surface is the CPU column and the
  bytes below, not latency.

  versus plain calamine in your own process (arm 0):
    wall -256.5 ms (0.88x), CPU +1.89 s (1.88x)
  server peak RSS                             303.4 MiB

shape and wire
  rows                                     985351
  cells (dense)                            7882808
  protobuf payload (arm 2 encode)             788.9 MB
  bytes on the socket (arm 3)                 789.3 MB  (compression: none)
  expansion over the source file               7.47x  (socket / source)
  messages on the stream                   3850  (255.9 rows each)
  throughput (arm 3, median)                 528795 rows/s, 4230358 cells/s

latency to the first row
  gRPC stream (min / median / p95)              1.3      1.3      1.5 ms
  a batch API cannot answer before           2459.3 ms  (arm 2 completes)

upload leg, counted separately
  OpenWorkbook for 105.7 MB                  496.1 ms  (213 MB/s)
  bytes written on the socket                 105.8 MB
  paid once per workbook, then any number of reads reuse the handle.

### BENCH_COMPRESSION=zstd
grpc-calamine: in-process calamine vs the same work over gRPC

workbook    : 100mb.xlsx
size        : 105.7 MB
sheet       : Worksheet   (of 4: Worksheet (2), Worksheet (3), Tablo3, Worksheet)
iterations  : 3, arms interleaved
host        : 32 logical cores
profile     : release (cargo defaults; no [profile.release] overrides)

client http2 window: 52428800 bytes
compression : zstd (grpc-encoding for the row stream)


  iteration 1/3
  iteration 2/3
  iteration 3/3
                        
same-work proof (digest of the canonical cell stream)
  0 calamine sparse walk             7b7ae3deea46e84e/7888232r/7888232c
  1 native dense                     b1ec698bf30cde49/985351r/7882808c
  2 native dense + protobuf encode   b1ec698bf30cde49/985351r/7882808c
  3 gRPC end to end                  b1ec698bf30cde49/985351r/7882808c

  arms 1-3 identical: yes
  arm 0 walks only populated cells (7888232 vs 7882808), so it is not comparable and is shown as a floor.

wall clock ms (min / median / p95), and CPU seconds burned per run
                                               min      med      p95     CPU s
  0  calamine alone, populated cells only    2097.0   2114.2   2126.0      2.14
  1  + dense canonical grid                  2328.3   2331.7   2339.1      2.36
  2  + protobuf convert and encode           2434.6   2441.6   2483.3      2.48
  3  + gRPC socket, decoded by the client    3427.0   3500.7   3592.0      7.17

where the in-process time goes (arms 0-2 are one serial thread)
  calamine parse, the floor                  2114.2 ms   86.6%
  densify into the contract's grid            217.6 ms    8.9%
  protobuf convert + encode                   109.8 ms    4.5%
  total the server must do per read          2441.6 ms

what the gRPC surface actually costs
  same work, one thread, in process (2)      2441.6 ms wall    2.48 s CPU
  same work, over the wire (3)               3500.7 ms wall    7.17 s CPU
  wall clock                                +1059.1 ms  (+43.4%)
  CPU                                         +4.69 s   (2.89x)
  parallelism used by arm 3                    2.05 cores (CPU / wall)

  Read the two columns together. gRPC finishes sooner in wall clock because
  the server parses while the client decodes, on different cores; it is not
  doing less work. The honest cost of the surface is the CPU column and the
  bytes below, not latency.

  versus plain calamine in your own process (arm 0):
    wall +1386.5 ms (1.66x), CPU +5.03 s (3.35x)
  server peak RSS                             321.4 MiB

shape and wire
  rows                                     985351
  cells (dense)                            7882808
  protobuf payload (arm 2 encode)             788.9 MB
  bytes on the socket (arm 3)                 290.1 MB  (compression: zstd)
  expansion over the source file               2.74x  (socket / source)
  messages on the stream                   3850  (255.9 rows each)
  throughput (arm 3, median)                 281476 rows/s, 2251805 cells/s

latency to the first row
  gRPC stream (min / median / p95)              7.8      7.8      9.0 ms
  a batch API cannot answer before           2441.6 ms  (arm 2 completes)

upload leg, counted separately
  OpenWorkbook for 105.7 MB                  493.6 ms  (214 MB/s)
  bytes written on the socket                 105.8 MB
  paid once per workbook, then any number of reads reuse the handle.

### BENCH_COMPRESSION=gzip
grpc-calamine: in-process calamine vs the same work over gRPC

workbook    : 100mb.xlsx
size        : 105.7 MB
sheet       : Worksheet   (of 4: Worksheet (2), Worksheet (3), Tablo3, Worksheet)
iterations  : 3, arms interleaved
host        : 32 logical cores
profile     : release (cargo defaults; no [profile.release] overrides)

client http2 window: 52428800 bytes
compression : gzip (grpc-encoding for the row stream)


  iteration 1/3
  iteration 2/3
  iteration 3/3
                        
same-work proof (digest of the canonical cell stream)
  0 calamine sparse walk             7b7ae3deea46e84e/7888232r/7888232c
  1 native dense                     b1ec698bf30cde49/985351r/7882808c
  2 native dense + protobuf encode   b1ec698bf30cde49/985351r/7882808c
  3 gRPC end to end                  b1ec698bf30cde49/985351r/7882808c

  arms 1-3 identical: yes
  arm 0 walks only populated cells (7888232 vs 7882808), so it is not comparable and is shown as a floor.

wall clock ms (min / median / p95), and CPU seconds burned per run
                                               min      med      p95     CPU s
  0  calamine alone, populated cells only    2105.6   2118.0   2483.1      2.26
  1  + dense canonical grid                  2328.1   2336.3   2528.0      2.42
  2  + protobuf convert and encode           2450.3   2460.4   2527.0      2.51
  3  + gRPC socket, decoded by the client   10114.8  10229.2  10315.8     14.67

where the in-process time goes (arms 0-2 are one serial thread)
  calamine parse, the floor                  2118.0 ms   86.1%
  densify into the contract's grid            218.3 ms    8.9%
  protobuf convert + encode                   124.1 ms    5.0%
  total the server must do per read          2460.4 ms

what the gRPC surface actually costs
  same work, one thread, in process (2)      2460.4 ms wall    2.51 s CPU
  same work, over the wire (3)              10229.2 ms wall   14.67 s CPU
  wall clock                                +7768.8 ms  (+315.8%)
  CPU                                        +12.17 s   (5.85x)
  parallelism used by arm 3                    1.43 cores (CPU / wall)

  Read the two columns together. gRPC finishes sooner in wall clock because
  the server parses while the client decodes, on different cores; it is not
  doing less work. The honest cost of the surface is the CPU column and the
  bytes below, not latency.

  versus plain calamine in your own process (arm 0):
    wall +8111.2 ms (4.83x), CPU +12.41 s (6.48x)
  server peak RSS                             315.6 MiB

shape and wire
  rows                                     985351
  cells (dense)                            7882808
  protobuf payload (arm 2 encode)             788.9 MB
  bytes on the socket (arm 3)                 295.8 MB  (compression: gzip)
  expansion over the source file               2.80x  (socket / source)
  messages on the stream                   3850  (255.9 rows each)
  throughput (arm 3, median)                  96328 rows/s, 770620 cells/s

latency to the first row
  gRPC stream (min / median / p95)             19.1     19.6     20.1 ms
  a batch API cannot answer before           2460.4 ms  (arm 2 completes)

upload leg, counted separately
  OpenWorkbook for 105.7 MB                  492.1 ms  (215 MB/s)
  bytes written on the socket                 105.8 MB
  paid once per workbook, then any number of reads reuse the handle.
```

## Dictionary mode (`use_string_table`, loopback, 2026-07-24)

The contract's opt-in dictionary encoding, end to end: shared strings arrive
once in `string_table` chunks and as u32 ids per cell; the client resolves
them and the digest gate holds the resolved stream to the same canonical
cells as every other arm.

Summary against the runs above (same workbook, sheet, machine):

| mode                | wall, med | CPU    | on the socket | vs source |
|---------------------|-----------|--------|---------------|-----------|
| plain contract      | 1863 ms   | 4.03 s | 789.3 MB      | 7.47x     |
| zstd                | 3501 ms   | 7.17 s | 290.1 MB      | 2.74x     |
| dictionary          | 2101 ms   | 3.47 s | 172.5 MB      | 1.63x     |
| dictionary + zstd   | 2176 ms   | 4.07 s | 61.6 MB       | 0.58x     |

Dictionary mode beats zstd on every axis at once, and combined they put the
stream at barely half the size of the source .xlsx.

```
### BENCH_DICT=1
grpc-calamine: in-process calamine vs the same work over gRPC

workbook    : 100mb.xlsx
size        : 105.7 MB
sheet       : Worksheet   (of 4: Worksheet (2), Worksheet (3), Tablo3, Worksheet)
iterations  : 3, arms interleaved
host        : 32 logical cores
profile     : release (cargo defaults; no [profile.release] overrides)

client http2 window: 52428800 bytes
compression : none (grpc-encoding for the row stream)
string dict : on (use_string_table)


  iteration 1/3
  iteration 2/3
  iteration 3/3
                        
same-work proof (digest of the canonical cell stream)
  0 calamine sparse walk             7b7ae3deea46e84e/7888232r/7888232c
  1 native dense                     b1ec698bf30cde49/985351r/7882808c
  2 native dense + protobuf encode   b1ec698bf30cde49/985351r/7882808c
  3 gRPC end to end                  b1ec698bf30cde49/985351r/7882808c

  arms 1-3 identical: yes
  arm 0 walks only populated cells (7888232 vs 7882808), so it is not comparable and is shown as a floor.

wall clock ms (min / median / p95), and CPU seconds burned per run
                                               min      med      p95     CPU s
  0  calamine alone, populated cells only    2113.7   2129.0   2132.2      2.15
  1  + dense canonical grid                  2373.7   2374.9   2375.3      2.40
  2  + protobuf convert and encode           2476.5   2480.8   2483.3      2.51
  3  + gRPC socket, decoded by the client    2083.5   2100.9   2223.1      3.47

where the in-process time goes (arms 0-2 are one serial thread)
  calamine parse, the floor                  2129.0 ms   85.8%
  densify into the contract's grid            245.9 ms    9.9%
  protobuf convert + encode                   105.9 ms    4.3%
  total the server must do per read          2480.8 ms

what the gRPC surface actually costs
  same work, one thread, in process (2)      2480.8 ms wall    2.51 s CPU
  same work, over the wire (3)               2100.9 ms wall    3.47 s CPU
  wall clock                                 -379.8 ms  (-15.3%)
  CPU                                         +0.97 s   (1.39x)
  parallelism used by arm 3                    1.65 cores (CPU / wall)

  Read the two columns together. gRPC finishes sooner in wall clock because
  the server parses while the client decodes, on different cores; it is not
  doing less work. The honest cost of the surface is the CPU column and the
  bytes below, not latency.

  versus plain calamine in your own process (arm 0):
    wall -28.0 ms (0.99x), CPU +1.32 s (1.61x)
  server peak RSS                             399.8 MiB

shape and wire
  rows                                     985351
  cells (dense)                            7882808
  protobuf payload (arm 2 encode)             788.9 MB
  bytes on the socket (arm 3)                 172.5 MB  (compression: none, dict: on)
  string table                             1047814 entries, sent once in-stream
  expansion over the source file               1.63x  (socket / source)
  messages on the stream                   3851  (255.9 rows each)
  throughput (arm 3, median)                 469005 rows/s, 3752040 cells/s

latency to the first row
  gRPC stream (min / median / p95)              1.4      1.6      1.7 ms
  a batch API cannot answer before           2480.8 ms  (arm 2 completes)

upload leg, counted separately
  OpenWorkbook for 105.7 MB                  488.2 ms  (217 MB/s)
  bytes written on the socket                 105.8 MB
  paid once per workbook, then any number of reads reuse the handle.

### BENCH_DICT=1 BENCH_COMPRESSION=zstd
grpc-calamine: in-process calamine vs the same work over gRPC

workbook    : 100mb.xlsx
size        : 105.7 MB
sheet       : Worksheet   (of 4: Worksheet (2), Worksheet (3), Tablo3, Worksheet)
iterations  : 3, arms interleaved
host        : 32 logical cores
profile     : release (cargo defaults; no [profile.release] overrides)

client http2 window: 52428800 bytes
compression : zstd (grpc-encoding for the row stream)
string dict : on (use_string_table)


  iteration 1/3
  iteration 2/3
  iteration 3/3
                        
same-work proof (digest of the canonical cell stream)
  0 calamine sparse walk             7b7ae3deea46e84e/7888232r/7888232c
  1 native dense                     b1ec698bf30cde49/985351r/7882808c
  2 native dense + protobuf encode   b1ec698bf30cde49/985351r/7882808c
  3 gRPC end to end                  b1ec698bf30cde49/985351r/7882808c

  arms 1-3 identical: yes
  arm 0 walks only populated cells (7888232 vs 7882808), so it is not comparable and is shown as a floor.

wall clock ms (min / median / p95), and CPU seconds burned per run
                                               min      med      p95     CPU s
  0  calamine alone, populated cells only    2124.3   2140.2   2141.0      2.16
  1  + dense canonical grid                  2355.8   2358.6   2384.6      2.39
  2  + protobuf convert and encode           2444.5   2460.2   2473.2      2.49
  3  + gRPC socket, decoded by the client    2155.5   2175.8   2218.7      4.07

where the in-process time goes (arms 0-2 are one serial thread)
  calamine parse, the floor                  2140.2 ms   87.0%
  densify into the contract's grid            218.4 ms    8.9%
  protobuf convert + encode                   101.5 ms    4.1%
  total the server must do per read          2460.2 ms

what the gRPC surface actually costs
  same work, one thread, in process (2)      2460.2 ms wall    2.49 s CPU
  same work, over the wire (3)               2175.8 ms wall    4.07 s CPU
  wall clock                                 -284.4 ms  (-11.6%)
  CPU                                         +1.59 s   (1.64x)
  parallelism used by arm 3                    1.87 cores (CPU / wall)

  Read the two columns together. gRPC finishes sooner in wall clock because
  the server parses while the client decodes, on different cores; it is not
  doing less work. The honest cost of the surface is the CPU column and the
  bytes below, not latency.

  versus plain calamine in your own process (arm 0):
    wall +35.6 ms (1.02x), CPU +1.91 s (1.89x)
  server peak RSS                             410.4 MiB

shape and wire
  rows                                     985351
  cells (dense)                            7882808
  protobuf payload (arm 2 encode)             788.9 MB
  bytes on the socket (arm 3)                  61.6 MB  (compression: zstd, dict: on)
  string table                             1047814 entries, sent once in-stream
  expansion over the source file               0.58x  (socket / source)
  messages on the stream                   3851  (255.9 rows each)
  throughput (arm 3, median)                 452872 rows/s, 3622976 cells/s

latency to the first row
  gRPC stream (min / median / p95)              6.6      6.8      7.0 ms
  a batch API cannot answer before           2460.2 ms  (arm 2 completes)

upload leg, counted separately
  OpenWorkbook for 105.7 MB                  491.5 ms  (215 MB/s)
  bytes written on the socket                 105.8 MB
  paid once per workbook, then any number of reads reuse the handle.
```

## On calamine's own benchmark dataset (NYC 311 1M, 2026-07-24)

calamine's README benchmarks against the NYC 311 Service Requests 1M-row
sample (41 columns, 41,000,041 dense cells, 28,056,975 populated),
distributed as CSV and converted to XLSX before measuring; the conversion
method is not stated there. We measured two conversions of the same CSV,
because the choice turns out to matter:

- **LibreOffice** (`soffice --headless --convert-to xlsx`): 185,977,936
  bytes, matching the "186MB" in calamine's README, with a
  `sharedStrings.xml` of 2,819,870 entries. This is the faithful file.
- **openpyxl write-only** (`bench/csv_to_xlsx.py`): 250,889,291 bytes,
  inline strings, no sst. On this file `use_string_table` correctly interns
  nothing, and the wire is identical with the flag on or off. Real
  Excel/LibreOffice output has an sst; streamed-writer output often does not.

Same host as above. calamine's published table (calamine 0.22.1, Ryzen 9
5900X, Windows, hyperfine whole-process timing) is not directly comparable
to these in-process numbers; openpyxl, measured both there (238.6 s) and
here (55.8 s on the LibreOffice file), anchors how much of the difference
is hardware and versions.

### LibreOffice conversion (sst, 186 MB)

Summary, dense-cell throughput on arm 3:

| arm | wall, med | on the socket | vs source |
|---|---|---|---|
| calamine alone, in process | 6.02 s | | |
| grpc-calamine | 6.60 s | 662.2 MB | 3.56x |
| + zstd alone | 6.53 s | 158.4 MB | 0.85x |
| + use_string_table | 6.50 s | 300.8 MB | 1.62x |
| + use_string_table + zstd | 6.47 s | 103.9 MB | 0.56x |

The dictionary also cuts arm 3 CPU from 11.5 s to 9.7 s: 2.8 million table
entries replace 28 million per-cell string copies. On this file zstd alone
out-compresses the dictionary alone (the 311 text is highly repetitive),
but pays for it in CPU: 12.5 s against the dictionary's 9.7 s. On the
105.7 MB workbook above the two ranked the other way around; which single
fix wins on bytes depends on the data, and the combination is smallest on
both.

```
### plain
grpc-calamine: in-process calamine vs the same work over gRPC

workbook    : NYC_311_SR_2010-2020-sample-1M.xlsx
size        : 186.0 MB
sheet       : NYC_311_SR_2010-2020-sample-1M   (of 1: NYC_311_SR_2010-2020-sample-1M)
iterations  : 3, arms interleaved
host        : 32 logical cores
profile     : release (cargo defaults; no [profile.release] overrides)

client http2 window: 52428800 bytes
compression : none (grpc-encoding for the row stream)
string dict : off


  iteration 1/3
  iteration 2/3
  iteration 3/3
                        
same-work proof (digest of the canonical cell stream)
  0 calamine sparse walk             d403ab267fbefeac/28103666r/28103666c
  1 native dense                     20dd91d5215cbcd4/1000001r/41000041c
  2 native dense + protobuf encode   20dd91d5215cbcd4/1000001r/41000041c
  3 gRPC end to end                  20dd91d5215cbcd4/1000001r/41000041c

  arms 1-3 identical: yes
  arm 0 walks only populated cells (28103666 vs 41000041), so it is not comparable and is shown as a floor.

wall clock ms (min / median / p95), and CPU seconds burned per run
                                               min      med      p95     CPU s
  0  calamine alone, populated cells only    5969.7   6018.3   6073.2      6.05
  1  + dense canonical grid                  6584.5   6605.9   6612.6      6.63
  2  + protobuf convert and encode           7097.6   7109.8   7143.7      7.15
  3  + gRPC socket, decoded by the client    6593.2   6602.3   6639.3     11.47

where the in-process time goes (arms 0-2 are one serial thread)
  calamine parse, the floor                  6018.3 ms   84.6%
  densify into the contract's grid            587.5 ms    8.3%
  protobuf convert + encode                   504.0 ms    7.1%
  total the server must do per read          7109.8 ms

what the gRPC surface actually costs
  same work, one thread, in process (2)      7109.8 ms wall    7.15 s CPU
  same work, over the wire (3)               6602.3 ms wall   11.47 s CPU
  wall clock                                 -507.6 ms  (-7.1%)
  CPU                                         +4.32 s   (1.60x)
  parallelism used by arm 3                    1.74 cores (CPU / wall)

  Read the two columns together. gRPC finishes sooner in wall clock because
  the server parses while the client decodes, on different cores; it is not
  doing less work. The honest cost of the surface is the CPU column and the
  bytes below, not latency.

  versus plain calamine in your own process (arm 0):
    wall +583.9 ms (1.10x), CPU +5.42 s (1.90x)
  server peak RSS                             380.6 MiB

shape and wire
  rows                                     1000001
  cells (dense)                            41000041
  protobuf payload (arm 2 encode)             661.8 MB
  bytes on the socket (arm 3)                 662.2 MB  (compression: none, dict: off)
  expansion over the source file               3.56x  (socket / source)
  messages on the stream                   3907  (256.0 rows each)
  throughput (arm 3, median)                 151463 rows/s, 6210003 cells/s

latency to the first row
  gRPC stream (min / median / p95)              3.5      3.5      3.7 ms
  a batch API cannot answer before           7109.8 ms  (arm 2 completes)

upload leg, counted separately
  OpenWorkbook for 186.0 MB                  640.3 ms  (290 MB/s)
  bytes written on the socket                 186.1 MB
  paid once per workbook, then any number of reads reuse the handle.

### BENCH_DICT=1
grpc-calamine: in-process calamine vs the same work over gRPC

workbook    : NYC_311_SR_2010-2020-sample-1M.xlsx
size        : 186.0 MB
sheet       : NYC_311_SR_2010-2020-sample-1M   (of 1: NYC_311_SR_2010-2020-sample-1M)
iterations  : 3, arms interleaved
host        : 32 logical cores
profile     : release (cargo defaults; no [profile.release] overrides)

client http2 window: 52428800 bytes
compression : none (grpc-encoding for the row stream)
string dict : on (use_string_table)


  iteration 1/3
  iteration 2/3
  iteration 3/3
                        
same-work proof (digest of the canonical cell stream)
  0 calamine sparse walk             d403ab267fbefeac/28103666r/28103666c
  1 native dense                     20dd91d5215cbcd4/1000001r/41000041c
  2 native dense + protobuf encode   20dd91d5215cbcd4/1000001r/41000041c
  3 gRPC end to end                  20dd91d5215cbcd4/1000001r/41000041c

  arms 1-3 identical: yes
  arm 0 walks only populated cells (28103666 vs 41000041), so it is not comparable and is shown as a floor.

wall clock ms (min / median / p95), and CPU seconds burned per run
                                               min      med      p95     CPU s
  0  calamine alone, populated cells only    5985.5   6014.8   6047.1      6.05
  1  + dense canonical grid                  6588.8   6594.2   6617.6      6.64
  2  + protobuf convert and encode           7101.3   7109.6   7126.3      7.14
  3  + gRPC socket, decoded by the client    6457.0   6499.1   6583.4      9.71

where the in-process time goes (arms 0-2 are one serial thread)
  calamine parse, the floor                  6014.8 ms   84.6%
  densify into the contract's grid            579.3 ms    8.1%
  protobuf convert + encode                   515.4 ms    7.2%
  total the server must do per read          7109.6 ms

what the gRPC surface actually costs
  same work, one thread, in process (2)      7109.6 ms wall    7.14 s CPU
  same work, over the wire (3)               6499.1 ms wall    9.71 s CPU
  wall clock                                 -610.5 ms  (-8.6%)
  CPU                                         +2.57 s   (1.36x)
  parallelism used by arm 3                    1.49 cores (CPU / wall)

  Read the two columns together. gRPC finishes sooner in wall clock because
  the server parses while the client decodes, on different cores; it is not
  doing less work. The honest cost of the surface is the CPU column and the
  bytes below, not latency.

  versus plain calamine in your own process (arm 0):
    wall +484.2 ms (1.08x), CPU +3.66 s (1.61x)
  server peak RSS                             525.0 MiB

shape and wire
  rows                                     1000001
  cells (dense)                            41000041
  protobuf payload (arm 2 encode)             661.8 MB
  bytes on the socket (arm 3)                 300.8 MB  (compression: none, dict: on)
  string table                             2819870 entries, sent once in-stream
  expansion over the source file               1.62x  (socket / source)
  messages on the stream                   3908  (255.9 rows each)
  throughput (arm 3, median)                 153869 rows/s, 6308619 cells/s

latency to the first row
  gRPC stream (min / median / p95)              2.9      3.0      3.1 ms
  a batch API cannot answer before           7109.6 ms  (arm 2 completes)

upload leg, counted separately
  OpenWorkbook for 186.0 MB                  638.9 ms  (291 MB/s)
  bytes written on the socket                 186.1 MB
  paid once per workbook, then any number of reads reuse the handle.

### BENCH_DICT=1 BENCH_COMPRESSION=zstd
grpc-calamine: in-process calamine vs the same work over gRPC

workbook    : NYC_311_SR_2010-2020-sample-1M.xlsx
size        : 186.0 MB
sheet       : NYC_311_SR_2010-2020-sample-1M   (of 1: NYC_311_SR_2010-2020-sample-1M)
iterations  : 3, arms interleaved
host        : 32 logical cores
profile     : release (cargo defaults; no [profile.release] overrides)

client http2 window: 52428800 bytes
compression : zstd (grpc-encoding for the row stream)
string dict : on (use_string_table)


  iteration 1/3
  iteration 2/3
  iteration 3/3
                        
same-work proof (digest of the canonical cell stream)
  0 calamine sparse walk             d403ab267fbefeac/28103666r/28103666c
  1 native dense                     20dd91d5215cbcd4/1000001r/41000041c
  2 native dense + protobuf encode   20dd91d5215cbcd4/1000001r/41000041c
  3 gRPC end to end                  20dd91d5215cbcd4/1000001r/41000041c

  arms 1-3 identical: yes
  arm 0 walks only populated cells (28103666 vs 41000041), so it is not comparable and is shown as a floor.

wall clock ms (min / median / p95), and CPU seconds burned per run
                                               min      med      p95     CPU s
  0  calamine alone, populated cells only    5961.3   6092.4   6112.1      6.09
  1  + dense canonical grid                  6608.3   6672.2   6677.8      6.69
  2  + protobuf convert and encode           7141.2   7149.6   7201.7      7.20
  3  + gRPC socket, decoded by the client    6466.3   6471.7   6473.1     10.50

where the in-process time goes (arms 0-2 are one serial thread)
  calamine parse, the floor                  6092.4 ms   85.2%
  densify into the contract's grid            579.8 ms    8.1%
  protobuf convert + encode                   477.4 ms    6.7%
  total the server must do per read          7149.6 ms

what the gRPC surface actually costs
  same work, one thread, in process (2)      7149.6 ms wall    7.20 s CPU
  same work, over the wire (3)               6471.7 ms wall   10.50 s CPU
  wall clock                                 -677.9 ms  (-9.5%)
  CPU                                         +3.30 s   (1.46x)
  parallelism used by arm 3                    1.62 cores (CPU / wall)

  Read the two columns together. gRPC finishes sooner in wall clock because
  the server parses while the client decodes, on different cores; it is not
  doing less work. The honest cost of the surface is the CPU column and the
  bytes below, not latency.

  versus plain calamine in your own process (arm 0):
    wall +379.3 ms (1.06x), CPU +4.41 s (1.72x)
  server peak RSS                             527.6 MiB

shape and wire
  rows                                     1000001
  cells (dense)                            41000041
  protobuf payload (arm 2 encode)             661.8 MB
  bytes on the socket (arm 3)                 103.9 MB  (compression: zstd, dict: on)
  string table                             2819870 entries, sent once in-stream
  expansion over the source file               0.56x  (socket / source)
  messages on the stream                   3908  (255.9 rows each)
  throughput (arm 3, median)                 154519 rows/s, 6335269 cells/s

latency to the first row
  gRPC stream (min / median / p95)              3.5      3.7      3.9 ms
  a batch API cannot answer before           7149.6 ms  (arm 2 completes)

upload leg, counted separately
  OpenWorkbook for 186.0 MB                  661.6 ms  (281 MB/s)
  bytes written on the socket                 186.1 MB
  paid once per workbook, then any number of reads reuse the handle.
```

### BENCH_COMPRESSION=zstd (no dict)

```
grpc-calamine: in-process calamine vs the same work over gRPC

workbook    : NYC_311_SR_2010-2020-sample-1M.xlsx
size        : 186.0 MB
sheet       : NYC_311_SR_2010-2020-sample-1M   (of 1: NYC_311_SR_2010-2020-sample-1M)
iterations  : 3, arms interleaved
host        : 32 logical cores
profile     : release (cargo defaults; no [profile.release] overrides)

client http2 window: 52428800 bytes
compression : zstd (grpc-encoding for the row stream)
string dict : off


  iteration 1/3
  iteration 2/3
  iteration 3/3
                        
same-work proof (digest of the canonical cell stream)
  0 calamine sparse walk             d403ab267fbefeac/28103666r/28103666c
  1 native dense                     20dd91d5215cbcd4/1000001r/41000041c
  2 native dense + protobuf encode   20dd91d5215cbcd4/1000001r/41000041c
  3 gRPC end to end                  20dd91d5215cbcd4/1000001r/41000041c

  arms 1-3 identical: yes
  arm 0 walks only populated cells (28103666 vs 41000041), so it is not comparable and is shown as a floor.

wall clock ms (min / median / p95), and CPU seconds burned per run
                                               min      med      p95     CPU s
  0  calamine alone, populated cells only    5737.2   5759.5   5793.5      5.80
  1  + dense canonical grid                  6412.8   6412.8   6436.0      6.46
  2  + protobuf convert and encode           6920.6   6963.9   6995.7      6.99
  3  + gRPC socket, decoded by the client    6500.1   6525.5   6564.9     12.50

where the in-process time goes (arms 0-2 are one serial thread)
  calamine parse, the floor                  5759.5 ms   82.7%
  densify into the contract's grid            653.4 ms    9.4%
  protobuf convert + encode                   551.1 ms    7.9%
  total the server must do per read          6963.9 ms

what the gRPC surface actually costs
  same work, one thread, in process (2)      6963.9 ms wall    6.99 s CPU
  same work, over the wire (3)               6525.5 ms wall   12.50 s CPU
  wall clock                                 -438.4 ms  (-6.3%)
  CPU                                         +5.51 s   (1.79x)
  parallelism used by arm 3                    1.92 cores (CPU / wall)

  Read the two columns together. gRPC finishes sooner in wall clock because
  the server parses while the client decodes, on different cores; it is not
  doing less work. The honest cost of the surface is the CPU column and the
  bytes below, not latency.

  versus plain calamine in your own process (arm 0):
    wall +766.0 ms (1.13x), CPU +6.70 s (2.16x)
  server peak RSS                             371.7 MiB

shape and wire
  rows                                     1000001
  cells (dense)                            41000041
  protobuf payload (arm 2 encode)             661.8 MB
  bytes on the socket (arm 3)                 158.4 MB  (compression: zstd, dict: off)
  expansion over the source file               0.85x  (socket / source)
  messages on the stream                   3907  (256.0 rows each)
  throughput (arm 3, median)                 153245 rows/s, 6283030 cells/s

latency to the first row
  gRPC stream (min / median / p95)              3.8      3.9      4.0 ms
  a batch API cannot answer before           6963.9 ms  (arm 2 completes)

upload leg, counted separately
  OpenWorkbook for 186.0 MB                  641.4 ms  (290 MB/s)
  bytes written on the socket                 186.1 MB
  paid once per workbook, then any number of reads reuse the handle.
```

Python, same file:

```
workbook : NYC_311_SR_2010-2020-sample-1M.xlsx
sheet    : NYC_311_SR_2010-2020-sample-1M

  upload once: 602 ms, 3907 messages
  upload once: 611 ms, 3907 messages (dict)

same-work proof
  P3 grpc-calamine over gRPC         00000000c2848a72/1000001r/41000041c
  P4 gRPC + use_string_table         00000000c2848a72/1000001r/41000041c
  P2 python-calamine (in-process)    00000000c2848a72/1000001r/41000041c
  P1 openpyxl read_only              0000000022057755/1001001r/41041041c
  identical: NO

wall clock
  P2 python-calamine (in-process)        17787 ms       56220 rows/s    1.00x
  P4 gRPC + use_string_table             18215 ms       54900 rows/s    1.02x
  P3 grpc-calamine over gRPC             18281 ms       54702 rows/s    1.03x
  P1 openpyxl read_only                  55796 ms       17940 rows/s    3.14x
```

python-calamine and the gRPC arms tie (the interpreter's per-cell loop is
the bottleneck either way) and agree byte-for-byte on 41,000,041 cells.
openpyxl reads 1,001,001 rows: LibreOffice pads the declared dimension
with 1,000 trailing empty rows and openpyxl trusts the declaration, the
same class of disagreement the synthetic fixtures in `tests/` pin down.

### openpyxl write-only conversion (inline strings, 251 MB)

```
### plain
grpc-calamine: in-process calamine vs the same work over gRPC

workbook    : NYC_311_SR_2010-2020-sample-1M.xlsx
size        : 250.9 MB
sheet       : NYC_311_SR_2010-2020-sample-1M   (of 1: NYC_311_SR_2010-2020-sample-1M)
iterations  : 3, arms interleaved
host        : 32 logical cores
profile     : release (cargo defaults; no [profile.release] overrides)

client http2 window: 52428800 bytes
compression : none (grpc-encoding for the row stream)
string dict : off


  iteration 1/3
  iteration 2/3
  iteration 3/3
                        
same-work proof (digest of the canonical cell stream)
  0 calamine sparse walk             323747fbdf384e28/28056975r/28056975c
  1 native dense                     b51656973c49a3e9/1000001r/41000041c
  2 native dense + protobuf encode   b51656973c49a3e9/1000001r/41000041c
  3 gRPC end to end                  b51656973c49a3e9/1000001r/41000041c

  arms 1-3 identical: yes
  arm 0 walks only populated cells (28056975 vs 41000041), so it is not comparable and is shown as a floor.

wall clock ms (min / median / p95), and CPU seconds burned per run
                                               min      med      p95     CPU s
  0  calamine alone, populated cells only    7013.0   7027.5   7097.7      7.05
  1  + dense canonical grid                  7626.9   7650.3   7680.3      7.65
  2  + protobuf convert and encode           8251.1   8261.7   8309.5      8.27
  3  + gRPC socket, decoded by the client    7842.2   8048.2   8078.5     12.81

where the in-process time goes (arms 0-2 are one serial thread)
  calamine parse, the floor                  7027.5 ms   85.1%
  densify into the contract's grid            622.8 ms    7.5%
  protobuf convert + encode                   611.4 ms    7.4%
  total the server must do per read          8261.7 ms

what the gRPC surface actually costs
  same work, one thread, in process (2)      8261.7 ms wall    8.27 s CPU
  same work, over the wire (3)               8048.2 ms wall   12.81 s CPU
  wall clock                                 -213.4 ms  (-2.6%)
  CPU                                         +4.54 s   (1.55x)
  parallelism used by arm 3                    1.59 cores (CPU / wall)

  Read the two columns together. gRPC finishes sooner in wall clock because
  the server parses while the client decodes, on different cores; it is not
  doing less work. The honest cost of the surface is the CPU column and the
  bytes below, not latency.

  versus plain calamine in your own process (arm 0):
    wall +1020.8 ms (1.15x), CPU +5.76 s (1.82x)
  server peak RSS                             490.6 MiB

shape and wire
  rows                                     1000001
  cells (dense)                            41000041
  protobuf payload (arm 2 encode)             661.8 MB
  bytes on the socket (arm 3)                 662.2 MB  (compression: none, dict: off)
  expansion over the source file               2.64x  (socket / source)
  messages on the stream                   3907  (256.0 rows each)
  throughput (arm 3, median)                 124251 rows/s, 5094285 cells/s

latency to the first row
  gRPC stream (min / median / p95)              3.8      3.9      3.9 ms
  a batch API cannot answer before           8261.7 ms  (arm 2 completes)

upload leg, counted separately
  OpenWorkbook for 250.9 MB                  149.2 ms  (1682 MB/s)
  bytes written on the socket                 251.0 MB
  paid once per workbook, then any number of reads reuse the handle.

### BENCH_DICT=1 (0 table entries: no sst to intern)
grpc-calamine: in-process calamine vs the same work over gRPC

workbook    : NYC_311_SR_2010-2020-sample-1M.xlsx
size        : 250.9 MB
sheet       : NYC_311_SR_2010-2020-sample-1M   (of 1: NYC_311_SR_2010-2020-sample-1M)
iterations  : 3, arms interleaved
host        : 32 logical cores
profile     : release (cargo defaults; no [profile.release] overrides)

client http2 window: 52428800 bytes
compression : none (grpc-encoding for the row stream)
string dict : on (use_string_table)


  iteration 1/3
  iteration 2/3
  iteration 3/3
                        
same-work proof (digest of the canonical cell stream)
  0 calamine sparse walk             323747fbdf384e28/28056975r/28056975c
  1 native dense                     b51656973c49a3e9/1000001r/41000041c
  2 native dense + protobuf encode   b51656973c49a3e9/1000001r/41000041c
  3 gRPC end to end                  b51656973c49a3e9/1000001r/41000041c

  arms 1-3 identical: yes
  arm 0 walks only populated cells (28056975 vs 41000041), so it is not comparable and is shown as a floor.

wall clock ms (min / median / p95), and CPU seconds burned per run
                                               min      med      p95     CPU s
  0  calamine alone, populated cells only    6954.6   6989.6   6991.9      6.98
  1  + dense canonical grid                  7615.3   7635.6   7636.0      7.63
  2  + protobuf convert and encode           8237.4   8245.6   8306.6      8.26
  3  + gRPC socket, decoded by the client    7931.2   7972.3   8022.5     12.82

where the in-process time goes (arms 0-2 are one serial thread)
  calamine parse, the floor                  6989.6 ms   84.8%
  densify into the contract's grid            646.0 ms    7.8%
  protobuf convert + encode                   610.0 ms    7.4%
  total the server must do per read          8245.6 ms

what the gRPC surface actually costs
  same work, one thread, in process (2)      8245.6 ms wall    8.26 s CPU
  same work, over the wire (3)               7972.3 ms wall   12.82 s CPU
  wall clock                                 -273.3 ms  (-3.3%)
  CPU                                         +4.55 s   (1.55x)
  parallelism used by arm 3                    1.61 cores (CPU / wall)

  Read the two columns together. gRPC finishes sooner in wall clock because
  the server parses while the client decodes, on different cores; it is not
  doing less work. The honest cost of the surface is the CPU column and the
  bytes below, not latency.

  versus plain calamine in your own process (arm 0):
    wall +982.7 ms (1.14x), CPU +5.84 s (1.84x)
  server peak RSS                             492.9 MiB

shape and wire
  rows                                     1000001
  cells (dense)                            41000041
  protobuf payload (arm 2 encode)             661.8 MB
  bytes on the socket (arm 3)                 662.2 MB  (compression: none, dict: on)
  string table                             0 entries, sent once in-stream
  expansion over the source file               2.64x  (socket / source)
  messages on the stream                   3907  (256.0 rows each)
  throughput (arm 3, median)                 125434 rows/s, 5142784 cells/s

latency to the first row
  gRPC stream (min / median / p95)              4.0      4.2      4.4 ms
  a batch API cannot answer before           8245.6 ms  (arm 2 completes)

upload leg, counted separately
  OpenWorkbook for 250.9 MB                  148.4 ms  (1690 MB/s)
  bytes written on the socket                 251.0 MB
  paid once per workbook, then any number of reads reuse the handle.
```

Python, same file:

```
workbook : NYC_311_SR_2010-2020-sample-1M.xlsx
sheet    : NYC_311_SR_2010-2020-sample-1M

  upload once: 167 ms, 3907 messages
  upload once: 159 ms, 3907 messages (dict)

same-work proof
  P3 grpc-calamine over gRPC         000000003b52e7de/1000001r/41000041c
  P4 gRPC + use_string_table         000000003b52e7de/1000001r/41000041c
  P2 python-calamine (in-process)    000000003b52e7de/1000001r/41000041c
  P1 openpyxl read_only              00000000b7c83c1e/1000001r/38460119c
  identical: NO

wall clock
  P3 grpc-calamine over gRPC             17780 ms       56242 rows/s    1.00x
  P4 gRPC + use_string_table             17801 ms       56178 rows/s    1.00x
  P2 python-calamine (in-process)        20011 ms       49972 rows/s    1.13x
  P1 openpyxl read_only                 122542 ms        8160 rows/s    6.89x
```

## Over a real network, with the dictionary (2026-07-25)

The runs that motivated the dictionary. Server on the second machine
(9950X, `tc tbf` shaping on its 10 GbE egress), client here; arms 0-2 stay
local, so the arm 3 CPU column is client-only. 105.7 MB workbook, all runs
digest-identical to the local arms.

| link | plain | use_string_table | + zstd |
|---|---|---|---|
| 10 GbE LAN | 2.01 s | 2.06 s | 2.20 s |
| shaped 1 Gbit/s | 8.64 s | 2.33 s | 1.83 s |
| shaped 250 Mbit/s | 26.28 s | 5.64 s | 2.53 s |

On a saturated link the stream is bytes divided by bandwidth, so the wire
sizes (789.3 / 172.5 / 61.6 MB) become the wall clock. At 250 Mbit/s the
dictionary alone is 4.7x faster than plain and with zstd 10.4x; at
1 Gbit/s the combined stream beats plain mode running on the 10 GbE LAN,
because 61.6 MB fits under the parse time even at gigabit rates.

### 10 GbE LAN, unshaped

```
#### plain
grpc-calamine: in-process calamine vs the same work over gRPC

workbook    : 100mb.xlsx
size        : 105.7 MB
sheet       : Worksheet   (of 4: Worksheet (2), Worksheet (3), Tablo3, Worksheet)
iterations  : 3, arms interleaved
host        : 32 logical cores
profile     : release (cargo defaults; no [profile.release] overrides)

client http2 window: 52428800 bytes
compression : none (grpc-encoding for the row stream)
string dict : off

server     : remote at 192.0.2.10:50055 (arms 0-2 remain local)


  iteration 1/3
  iteration 2/3
  iteration 3/3
                        
same-work proof (digest of the canonical cell stream)
  0 calamine sparse walk             7b7ae3deea46e84e/7888232r/7888232c
  1 native dense                     b1ec698bf30cde49/985351r/7882808c
  2 native dense + protobuf encode   b1ec698bf30cde49/985351r/7882808c
  3 gRPC end to end                  b1ec698bf30cde49/985351r/7882808c

  arms 1-3 identical: yes
  arm 0 walks only populated cells (7888232 vs 7882808), so it is not comparable and is shown as a floor.

wall clock ms (min / median / p95), and CPU seconds burned per run
                                               min      med      p95     CPU s
  0  calamine alone, populated cells only    2141.8   2147.3   2162.9      2.17
  1  + dense canonical grid                  2355.2   2375.0   2385.4      2.40
  2  + protobuf convert and encode           2424.4   2458.9   2464.9      2.48
  3  + gRPC socket, decoded by the client    1878.4   2013.6   2029.1      1.32

where the in-process time goes (arms 0-2 are one serial thread)
  calamine parse, the floor                  2147.3 ms   87.3%
  densify into the contract's grid            227.6 ms    9.3%
  protobuf convert + encode                    84.0 ms    3.4%
  total the server must do per read          2458.9 ms

what the gRPC surface actually costs
  same work, one thread, in process (2)      2458.9 ms wall    2.48 s CPU
  same work, over the wire (3)               2013.6 ms wall    1.32 s CPU
  wall clock                                 -445.3 ms  (-18.1%)
  CPU                                         -1.16 s   (0.53x)
  parallelism used by arm 3                    0.66 cores (CPU / wall)

  Read the two columns together. gRPC finishes sooner in wall clock because
  the server parses while the client decodes, on different cores; it is not
  doing less work. The honest cost of the surface is the CPU column and the
  bytes below, not latency.

  versus plain calamine in your own process (arm 0):
    wall -133.7 ms (0.94x), CPU -0.85 s (0.61x)
  server peak RSS                               0.0 MiB

shape and wire
  rows                                     985351
  cells (dense)                            7882808
  protobuf payload (arm 2 encode)             788.9 MB
  bytes on the socket (arm 3)                 789.3 MB  (compression: none, dict: off)
  expansion over the source file               7.47x  (socket / source)
  messages on the stream                   3850  (255.9 rows each)
  throughput (arm 3, median)                 489346 rows/s, 3914769 cells/s

latency to the first row
  gRPC stream (min / median / p95)              1.9      1.9      2.1 ms
  a batch API cannot answer before           2458.9 ms  (arm 2 completes)

upload leg, counted separately
  OpenWorkbook for 105.7 MB                 1038.8 ms  (102 MB/s)
  bytes written on the socket                 105.8 MB
  paid once per workbook, then any number of reads reuse the handle.

#### BENCH_DICT=1
grpc-calamine: in-process calamine vs the same work over gRPC

workbook    : 100mb.xlsx
size        : 105.7 MB
sheet       : Worksheet   (of 4: Worksheet (2), Worksheet (3), Tablo3, Worksheet)
iterations  : 3, arms interleaved
host        : 32 logical cores
profile     : release (cargo defaults; no [profile.release] overrides)

client http2 window: 52428800 bytes
compression : none (grpc-encoding for the row stream)
string dict : on (use_string_table)

server     : remote at 192.0.2.10:50055 (arms 0-2 remain local)


  iteration 1/3
  iteration 2/3
  iteration 3/3
                        
same-work proof (digest of the canonical cell stream)
  0 calamine sparse walk             7b7ae3deea46e84e/7888232r/7888232c
  1 native dense                     b1ec698bf30cde49/985351r/7882808c
  2 native dense + protobuf encode   b1ec698bf30cde49/985351r/7882808c
  3 gRPC end to end                  b1ec698bf30cde49/985351r/7882808c

  arms 1-3 identical: yes
  arm 0 walks only populated cells (7888232 vs 7882808), so it is not comparable and is shown as a floor.

wall clock ms (min / median / p95), and CPU seconds burned per run
                                               min      med      p95     CPU s
  0  calamine alone, populated cells only    2126.5   2174.6   2211.2      2.20
  1  + dense canonical grid                  2358.4   2365.0   2416.4      2.40
  2  + protobuf convert and encode           2436.7   2467.8   2487.1      2.49
  3  + gRPC socket, decoded by the client    1876.3   2062.2   2067.5      0.91

where the in-process time goes (arms 0-2 are one serial thread)
  calamine parse, the floor                  2174.6 ms   88.1%
  densify into the contract's grid            190.5 ms    7.7%
  protobuf convert + encode                   102.8 ms    4.2%
  total the server must do per read          2467.8 ms

what the gRPC surface actually costs
  same work, one thread, in process (2)      2467.8 ms wall    2.49 s CPU
  same work, over the wire (3)               2062.2 ms wall    0.91 s CPU
  wall clock                                 -405.6 ms  (-16.4%)
  CPU                                         -1.58 s   (0.36x)
  parallelism used by arm 3                    0.44 cores (CPU / wall)

  Read the two columns together. gRPC finishes sooner in wall clock because
  the server parses while the client decodes, on different cores; it is not
  doing less work. The honest cost of the surface is the CPU column and the
  bytes below, not latency.

  versus plain calamine in your own process (arm 0):
    wall -112.3 ms (0.95x), CPU -1.29 s (0.41x)
  server peak RSS                               0.0 MiB

shape and wire
  rows                                     985351
  cells (dense)                            7882808
  protobuf payload (arm 2 encode)             788.9 MB
  bytes on the socket (arm 3)                 172.5 MB  (compression: none, dict: on)
  string table                             1047814 entries, sent once in-stream
  expansion over the source file               1.63x  (socket / source)
  messages on the stream                   3851  (255.9 rows each)
  throughput (arm 3, median)                 477805 rows/s, 3822442 cells/s

latency to the first row
  gRPC stream (min / median / p95)              2.1      2.2      2.3 ms
  a batch API cannot answer before           2467.8 ms  (arm 2 completes)

upload leg, counted separately
  OpenWorkbook for 105.7 MB                  629.9 ms  (168 MB/s)
  bytes written on the socket                 105.8 MB
  paid once per workbook, then any number of reads reuse the handle.

#### BENCH_DICT=1 BENCH_COMPRESSION=zstd
grpc-calamine: in-process calamine vs the same work over gRPC

workbook    : 100mb.xlsx
size        : 105.7 MB
sheet       : Worksheet   (of 4: Worksheet (2), Worksheet (3), Tablo3, Worksheet)
iterations  : 3, arms interleaved
host        : 32 logical cores
profile     : release (cargo defaults; no [profile.release] overrides)

client http2 window: 52428800 bytes
compression : zstd (grpc-encoding for the row stream)
string dict : on (use_string_table)

server     : remote at 192.0.2.10:50055 (arms 0-2 remain local)


  iteration 1/3
  iteration 2/3
  iteration 3/3
                        
same-work proof (digest of the canonical cell stream)
  0 calamine sparse walk             7b7ae3deea46e84e/7888232r/7888232c
  1 native dense                     b1ec698bf30cde49/985351r/7882808c
  2 native dense + protobuf encode   b1ec698bf30cde49/985351r/7882808c
  3 gRPC end to end                  b1ec698bf30cde49/985351r/7882808c

  arms 1-3 identical: yes
  arm 0 walks only populated cells (7888232 vs 7882808), so it is not comparable and is shown as a floor.

wall clock ms (min / median / p95), and CPU seconds burned per run
                                               min      med      p95     CPU s
  0  calamine alone, populated cells only    2109.1   2142.2   2160.7      2.16
  1  + dense canonical grid                  2335.7   2342.1   2358.9      2.37
  2  + protobuf convert and encode           2433.5   2442.2   2460.9      2.47
  3  + gRPC socket, decoded by the client    1969.2   2203.5   2213.7      0.98

where the in-process time goes (arms 0-2 are one serial thread)
  calamine parse, the floor                  2142.2 ms   87.7%
  densify into the contract's grid            199.9 ms    8.2%
  protobuf convert + encode                   100.1 ms    4.1%
  total the server must do per read          2442.2 ms

what the gRPC surface actually costs
  same work, one thread, in process (2)      2442.2 ms wall    2.47 s CPU
  same work, over the wire (3)               2203.5 ms wall    0.98 s CPU
  wall clock                                 -238.7 ms  (-9.8%)
  CPU                                         -1.49 s   (0.40x)
  parallelism used by arm 3                    0.44 cores (CPU / wall)

  Read the two columns together. gRPC finishes sooner in wall clock because
  the server parses while the client decodes, on different cores; it is not
  doing less work. The honest cost of the surface is the CPU column and the
  bytes below, not latency.

  versus plain calamine in your own process (arm 0):
    wall +61.3 ms (1.03x), CPU -1.18 s (0.45x)
  server peak RSS                               0.0 MiB

shape and wire
  rows                                     985351
  cells (dense)                            7882808
  protobuf payload (arm 2 encode)             788.9 MB
  bytes on the socket (arm 3)                  61.6 MB  (compression: zstd, dict: on)
  string table                             1047814 entries, sent once in-stream
  expansion over the source file               0.58x  (socket / source)
  messages on the stream                   3851  (255.9 rows each)
  throughput (arm 3, median)                 447185 rows/s, 3577483 cells/s

latency to the first row
  gRPC stream (min / median / p95)              2.9      6.8      7.8 ms
  a batch API cannot answer before           2442.2 ms  (arm 2 completes)

upload leg, counted separately
  OpenWorkbook for 105.7 MB                  616.0 ms  (172 MB/s)
  bytes written on the socket                 105.8 MB
  paid once per workbook, then any number of reads reuse the handle.
```
### shaped to 1 Gbit/s (tc tbf, burst 4mb, latency 300ms)

```
#### plain
grpc-calamine: in-process calamine vs the same work over gRPC

workbook    : 100mb.xlsx
size        : 105.7 MB
sheet       : Worksheet   (of 4: Worksheet (2), Worksheet (3), Tablo3, Worksheet)
iterations  : 2, arms interleaved
host        : 32 logical cores
profile     : release (cargo defaults; no [profile.release] overrides)

client http2 window: 52428800 bytes
compression : none (grpc-encoding for the row stream)
string dict : off

server     : remote at 192.0.2.10:50055 (arms 0-2 remain local)


  iteration 1/2
  iteration 2/2
                        
same-work proof (digest of the canonical cell stream)
  0 calamine sparse walk             7b7ae3deea46e84e/7888232r/7888232c
  1 native dense                     b1ec698bf30cde49/985351r/7882808c
  2 native dense + protobuf encode   b1ec698bf30cde49/985351r/7882808c
  3 gRPC end to end                  b1ec698bf30cde49/985351r/7882808c

  arms 1-3 identical: yes
  arm 0 walks only populated cells (7888232 vs 7882808), so it is not comparable and is shown as a floor.

wall clock ms (min / median / p95), and CPU seconds burned per run
                                               min      med      p95     CPU s
  0  calamine alone, populated cells only    2111.6   2144.8   2144.8      2.16
  1  + dense canonical grid                  2352.2   2363.8   2363.8      2.39
  2  + protobuf convert and encode           2439.5   2452.2   2452.2      2.47
  3  + gRPC socket, decoded by the client    6573.0   8639.9   8639.9      1.29

where the in-process time goes (arms 0-2 are one serial thread)
  calamine parse, the floor                  2144.8 ms   87.5%
  densify into the contract's grid            219.0 ms    8.9%
  protobuf convert + encode                    88.4 ms    3.6%
  total the server must do per read          2452.2 ms

what the gRPC surface actually costs
  same work, one thread, in process (2)      2452.2 ms wall    2.47 s CPU
  same work, over the wire (3)               8639.9 ms wall    1.29 s CPU
  wall clock                                +6187.7 ms  (+252.3%)
  CPU                                         -1.18 s   (0.52x)
  parallelism used by arm 3                    0.15 cores (CPU / wall)

  Read the two columns together. gRPC finishes sooner in wall clock because
  the server parses while the client decodes, on different cores; it is not
  doing less work. The honest cost of the surface is the CPU column and the
  bytes below, not latency.

  versus plain calamine in your own process (arm 0):
    wall +6495.1 ms (4.03x), CPU -0.87 s (0.60x)
  server peak RSS                               0.0 MiB

shape and wire
  rows                                     985351
  cells (dense)                            7882808
  protobuf payload (arm 2 encode)             788.9 MB
  bytes on the socket (arm 3)                 789.3 MB  (compression: none, dict: off)
  expansion over the source file               7.47x  (socket / source)
  messages on the stream                   3850  (255.9 rows each)
  throughput (arm 3, median)                 114047 rows/s, 912374 cells/s

latency to the first row
  gRPC stream (min / median / p95)              1.9      2.3      2.3 ms
  a batch API cannot answer before           2452.2 ms  (arm 2 completes)

upload leg, counted separately
  OpenWorkbook for 105.7 MB                  611.8 ms  (173 MB/s)
  bytes written on the socket                 105.8 MB
  paid once per workbook, then any number of reads reuse the handle.

#### BENCH_DICT=1
grpc-calamine: in-process calamine vs the same work over gRPC

workbook    : 100mb.xlsx
size        : 105.7 MB
sheet       : Worksheet   (of 4: Worksheet (2), Worksheet (3), Tablo3, Worksheet)
iterations  : 2, arms interleaved
host        : 32 logical cores
profile     : release (cargo defaults; no [profile.release] overrides)

client http2 window: 52428800 bytes
compression : none (grpc-encoding for the row stream)
string dict : on (use_string_table)

server     : remote at 192.0.2.10:50055 (arms 0-2 remain local)


  iteration 1/2
  iteration 2/2
                        
same-work proof (digest of the canonical cell stream)
  0 calamine sparse walk             7b7ae3deea46e84e/7888232r/7888232c
  1 native dense                     b1ec698bf30cde49/985351r/7882808c
  2 native dense + protobuf encode   b1ec698bf30cde49/985351r/7882808c
  3 gRPC end to end                  b1ec698bf30cde49/985351r/7882808c

  arms 1-3 identical: yes
  arm 0 walks only populated cells (7888232 vs 7882808), so it is not comparable and is shown as a floor.

wall clock ms (min / median / p95), and CPU seconds burned per run
                                               min      med      p95     CPU s
  0  calamine alone, populated cells only    2119.0   2128.8   2128.8      2.16
  1  + dense canonical grid                  2339.6   2342.5   2342.5      2.37
  2  + protobuf convert and encode           2457.0   2478.0   2478.0      2.49
  3  + gRPC socket, decoded by the client    2320.5   2332.1   2332.1      0.89

where the in-process time goes (arms 0-2 are one serial thread)
  calamine parse, the floor                  2128.8 ms   85.9%
  densify into the contract's grid            213.7 ms    8.6%
  protobuf convert + encode                   135.5 ms    5.5%
  total the server must do per read          2478.0 ms

what the gRPC surface actually costs
  same work, one thread, in process (2)      2478.0 ms wall    2.49 s CPU
  same work, over the wire (3)               2332.1 ms wall    0.89 s CPU
  wall clock                                 -145.9 ms  (-5.9%)
  CPU                                         -1.60 s   (0.36x)
  parallelism used by arm 3                    0.38 cores (CPU / wall)

  Read the two columns together. gRPC finishes sooner in wall clock because
  the server parses while the client decodes, on different cores; it is not
  doing less work. The honest cost of the surface is the CPU column and the
  bytes below, not latency.

  versus plain calamine in your own process (arm 0):
    wall +203.3 ms (1.10x), CPU -1.26 s (0.42x)
  server peak RSS                               0.0 MiB

shape and wire
  rows                                     985351
  cells (dense)                            7882808
  protobuf payload (arm 2 encode)             788.9 MB
  bytes on the socket (arm 3)                 172.5 MB  (compression: none, dict: on)
  string table                             1047814 entries, sent once in-stream
  expansion over the source file               1.63x  (socket / source)
  messages on the stream                   3850  (255.9 rows each)
  throughput (arm 3, median)                 422524 rows/s, 3380191 cells/s

latency to the first row
  gRPC stream (min / median / p95)              2.1      2.3      2.3 ms
  a batch API cannot answer before           2478.0 ms  (arm 2 completes)

upload leg, counted separately
  OpenWorkbook for 105.7 MB                  612.8 ms  (173 MB/s)
  bytes written on the socket                 105.8 MB
  paid once per workbook, then any number of reads reuse the handle.

#### BENCH_DICT=1 BENCH_COMPRESSION=zstd
grpc-calamine: in-process calamine vs the same work over gRPC

workbook    : 100mb.xlsx
size        : 105.7 MB
sheet       : Worksheet   (of 4: Worksheet (2), Worksheet (3), Tablo3, Worksheet)
iterations  : 2, arms interleaved
host        : 32 logical cores
profile     : release (cargo defaults; no [profile.release] overrides)

client http2 window: 52428800 bytes
compression : zstd (grpc-encoding for the row stream)
string dict : on (use_string_table)

server     : remote at 192.0.2.10:50055 (arms 0-2 remain local)


  iteration 1/2
  iteration 2/2
                        
same-work proof (digest of the canonical cell stream)
  0 calamine sparse walk             7b7ae3deea46e84e/7888232r/7888232c
  1 native dense                     b1ec698bf30cde49/985351r/7882808c
  2 native dense + protobuf encode   b1ec698bf30cde49/985351r/7882808c
  3 gRPC end to end                  b1ec698bf30cde49/985351r/7882808c

  arms 1-3 identical: yes
  arm 0 walks only populated cells (7888232 vs 7882808), so it is not comparable and is shown as a floor.

wall clock ms (min / median / p95), and CPU seconds burned per run
                                               min      med      p95     CPU s
  0  calamine alone, populated cells only    2136.6   2144.9   2144.9      2.17
  1  + dense canonical grid                  2341.5   2341.9   2341.9      2.37
  2  + protobuf convert and encode           2481.4   2484.6   2484.6      2.51
  3  + gRPC socket, decoded by the client    1822.7   1827.6   1827.6      0.97

where the in-process time goes (arms 0-2 are one serial thread)
  calamine parse, the floor                  2144.9 ms   86.3%
  densify into the contract's grid            197.0 ms    7.9%
  protobuf convert + encode                   142.7 ms    5.7%
  total the server must do per read          2484.6 ms

what the gRPC surface actually costs
  same work, one thread, in process (2)      2484.6 ms wall    2.51 s CPU
  same work, over the wire (3)               1827.6 ms wall    0.97 s CPU
  wall clock                                 -657.0 ms  (-26.4%)
  CPU                                         -1.54 s   (0.39x)
  parallelism used by arm 3                    0.53 cores (CPU / wall)

  Read the two columns together. gRPC finishes sooner in wall clock because
  the server parses while the client decodes, on different cores; it is not
  doing less work. The honest cost of the surface is the CPU column and the
  bytes below, not latency.

  versus plain calamine in your own process (arm 0):
    wall -317.3 ms (0.85x), CPU -1.20 s (0.45x)
  server peak RSS                               0.0 MiB

shape and wire
  rows                                     985351
  cells (dense)                            7882808
  protobuf payload (arm 2 encode)             788.9 MB
  bytes on the socket (arm 3)                  61.6 MB  (compression: zstd, dict: on)
  string table                             1047814 entries, sent once in-stream
  expansion over the source file               0.58x  (socket / source)
  messages on the stream                   3851  (255.9 rows each)
  throughput (arm 3, median)                 539158 rows/s, 4313261 cells/s

latency to the first row
  gRPC stream (min / median / p95)              7.1      7.1      7.1 ms
  a batch API cannot answer before           2484.6 ms  (arm 2 completes)

upload leg, counted separately
  OpenWorkbook for 105.7 MB                  605.4 ms  (175 MB/s)
  bytes written on the socket                 105.8 MB
  paid once per workbook, then any number of reads reuse the handle.
```
### shaped to 250 Mbit/s

```
#### plain
grpc-calamine: in-process calamine vs the same work over gRPC

workbook    : 100mb.xlsx
size        : 105.7 MB
sheet       : Worksheet   (of 4: Worksheet (2), Worksheet (3), Tablo3, Worksheet)
iterations  : 2, arms interleaved
host        : 32 logical cores
profile     : release (cargo defaults; no [profile.release] overrides)

client http2 window: 52428800 bytes
compression : none (grpc-encoding for the row stream)
string dict : off

server     : remote at 192.0.2.10:50055 (arms 0-2 remain local)


  iteration 1/2
  iteration 2/2
                        
same-work proof (digest of the canonical cell stream)
  0 calamine sparse walk             7b7ae3deea46e84e/7888232r/7888232c
  1 native dense                     b1ec698bf30cde49/985351r/7882808c
  2 native dense + protobuf encode   b1ec698bf30cde49/985351r/7882808c
  3 gRPC end to end                  b1ec698bf30cde49/985351r/7882808c

  arms 1-3 identical: yes
  arm 0 walks only populated cells (7888232 vs 7882808), so it is not comparable and is shown as a floor.

wall clock ms (min / median / p95), and CPU seconds burned per run
                                               min      med      p95     CPU s
  0  calamine alone, populated cells only    2146.3   2149.5   2149.5      2.17
  1  + dense canonical grid                  2343.2   2363.7   2363.7      2.38
  2  + protobuf convert and encode           2430.6   2444.4   2444.4      2.47
  3  + gRPC socket, decoded by the client   26278.8  26279.2  26279.2      1.50

where the in-process time goes (arms 0-2 are one serial thread)
  calamine parse, the floor                  2149.5 ms   87.9%
  densify into the contract's grid            214.2 ms    8.8%
  protobuf convert + encode                    80.7 ms    3.3%
  total the server must do per read          2444.4 ms

what the gRPC surface actually costs
  same work, one thread, in process (2)      2444.4 ms wall    2.47 s CPU
  same work, over the wire (3)              26279.2 ms wall    1.50 s CPU
  wall clock                               +23834.7 ms  (+975.1%)
  CPU                                         -0.97 s   (0.61x)
  parallelism used by arm 3                    0.06 cores (CPU / wall)

  Read the two columns together. gRPC finishes sooner in wall clock because
  the server parses while the client decodes, on different cores; it is not
  doing less work. The honest cost of the surface is the CPU column and the
  bytes below, not latency.

  versus plain calamine in your own process (arm 0):
    wall +24129.7 ms (12.23x), CPU -0.68 s (0.69x)
  server peak RSS                               0.0 MiB

shape and wire
  rows                                     985351
  cells (dense)                            7882808
  protobuf payload (arm 2 encode)             788.9 MB
  bytes on the socket (arm 3)                 789.3 MB  (compression: none, dict: off)
  expansion over the source file               7.47x  (socket / source)
  messages on the stream                   3850  (255.9 rows each)
  throughput (arm 3, median)                  37496 rows/s, 299964 cells/s

latency to the first row
  gRPC stream (min / median / p95)              2.3      2.5      2.5 ms
  a batch API cannot answer before           2444.4 ms  (arm 2 completes)

upload leg, counted separately
  OpenWorkbook for 105.7 MB                  614.5 ms  (172 MB/s)
  bytes written on the socket                 105.8 MB
  paid once per workbook, then any number of reads reuse the handle.

#### BENCH_DICT=1
grpc-calamine: in-process calamine vs the same work over gRPC

workbook    : 100mb.xlsx
size        : 105.7 MB
sheet       : Worksheet   (of 4: Worksheet (2), Worksheet (3), Tablo3, Worksheet)
iterations  : 2, arms interleaved
host        : 32 logical cores
profile     : release (cargo defaults; no [profile.release] overrides)

client http2 window: 52428800 bytes
compression : none (grpc-encoding for the row stream)
string dict : on (use_string_table)

server     : remote at 192.0.2.10:50055 (arms 0-2 remain local)


  iteration 1/2
  iteration 2/2
                        
same-work proof (digest of the canonical cell stream)
  0 calamine sparse walk             7b7ae3deea46e84e/7888232r/7888232c
  1 native dense                     b1ec698bf30cde49/985351r/7882808c
  2 native dense + protobuf encode   b1ec698bf30cde49/985351r/7882808c
  3 gRPC end to end                  b1ec698bf30cde49/985351r/7882808c

  arms 1-3 identical: yes
  arm 0 walks only populated cells (7888232 vs 7882808), so it is not comparable and is shown as a floor.

wall clock ms (min / median / p95), and CPU seconds burned per run
                                               min      med      p95     CPU s
  0  calamine alone, populated cells only    2117.0   2142.1   2142.1      2.16
  1  + dense canonical grid                  2347.8   2367.4   2367.4      2.38
  2  + protobuf convert and encode           2433.3   2445.2   2445.2      2.47
  3  + gRPC socket, decoded by the client    5639.7   5640.0   5640.0      0.92

where the in-process time goes (arms 0-2 are one serial thread)
  calamine parse, the floor                  2142.1 ms   87.6%
  densify into the contract's grid            225.3 ms    9.2%
  protobuf convert + encode                    77.8 ms    3.2%
  total the server must do per read          2445.2 ms

what the gRPC surface actually costs
  same work, one thread, in process (2)      2445.2 ms wall    2.47 s CPU
  same work, over the wire (3)               5640.0 ms wall    0.92 s CPU
  wall clock                                +3194.8 ms  (+130.7%)
  CPU                                         -1.55 s   (0.37x)
  parallelism used by arm 3                    0.16 cores (CPU / wall)

  Read the two columns together. gRPC finishes sooner in wall clock because
  the server parses while the client decodes, on different cores; it is not
  doing less work. The honest cost of the surface is the CPU column and the
  bytes below, not latency.

  versus plain calamine in your own process (arm 0):
    wall +3497.8 ms (2.63x), CPU -1.24 s (0.43x)
  server peak RSS                               0.0 MiB

shape and wire
  rows                                     985351
  cells (dense)                            7882808
  protobuf payload (arm 2 encode)             788.9 MB
  bytes on the socket (arm 3)                 172.5 MB  (compression: none, dict: on)
  string table                             1047814 entries, sent once in-stream
  expansion over the source file               1.63x  (socket / source)
  messages on the stream                   3851  (255.9 rows each)
  throughput (arm 3, median)                 174708 rows/s, 1397664 cells/s

latency to the first row
  gRPC stream (min / median / p95)              2.2      2.5      2.5 ms
  a batch API cannot answer before           2445.2 ms  (arm 2 completes)

upload leg, counted separately
  OpenWorkbook for 105.7 MB                  611.7 ms  (173 MB/s)
  bytes written on the socket                 105.8 MB
  paid once per workbook, then any number of reads reuse the handle.

#### BENCH_DICT=1 BENCH_COMPRESSION=zstd
grpc-calamine: in-process calamine vs the same work over gRPC

workbook    : 100mb.xlsx
size        : 105.7 MB
sheet       : Worksheet   (of 4: Worksheet (2), Worksheet (3), Tablo3, Worksheet)
iterations  : 2, arms interleaved
host        : 32 logical cores
profile     : release (cargo defaults; no [profile.release] overrides)

client http2 window: 52428800 bytes
compression : zstd (grpc-encoding for the row stream)
string dict : on (use_string_table)

server     : remote at 192.0.2.10:50055 (arms 0-2 remain local)


  iteration 1/2
  iteration 2/2
                        
same-work proof (digest of the canonical cell stream)
  0 calamine sparse walk             7b7ae3deea46e84e/7888232r/7888232c
  1 native dense                     b1ec698bf30cde49/985351r/7882808c
  2 native dense + protobuf encode   b1ec698bf30cde49/985351r/7882808c
  3 gRPC end to end                  b1ec698bf30cde49/985351r/7882808c

  arms 1-3 identical: yes
  arm 0 walks only populated cells (7888232 vs 7882808), so it is not comparable and is shown as a floor.

wall clock ms (min / median / p95), and CPU seconds burned per run
                                               min      med      p95     CPU s
  0  calamine alone, populated cells only    2110.4   2147.8   2147.8      2.16
  1  + dense canonical grid                  2342.8   2381.9   2381.9      2.39
  2  + protobuf convert and encode           2448.0   2457.1   2457.1      2.47
  3  + gRPC socket, decoded by the client    2515.3   2526.3   2526.3      0.99

where the in-process time goes (arms 0-2 are one serial thread)
  calamine parse, the floor                  2147.8 ms   87.4%
  densify into the contract's grid            234.1 ms    9.5%
  protobuf convert + encode                    75.2 ms    3.1%
  total the server must do per read          2457.1 ms

what the gRPC surface actually costs
  same work, one thread, in process (2)      2457.1 ms wall    2.47 s CPU
  same work, over the wire (3)               2526.3 ms wall    0.99 s CPU
  wall clock                                  +69.2 ms  (+2.8%)
  CPU                                         -1.48 s   (0.40x)
  parallelism used by arm 3                    0.39 cores (CPU / wall)

  Read the two columns together. gRPC finishes sooner in wall clock because
  the server parses while the client decodes, on different cores; it is not
  doing less work. The honest cost of the surface is the CPU column and the
  bytes below, not latency.

  versus plain calamine in your own process (arm 0):
    wall +378.5 ms (1.18x), CPU -1.17 s (0.46x)
  server peak RSS                               0.0 MiB

shape and wire
  rows                                     985351
  cells (dense)                            7882808
  protobuf payload (arm 2 encode)             788.9 MB
  bytes on the socket (arm 3)                  61.6 MB  (compression: zstd, dict: on)
  string table                             1047814 entries, sent once in-stream
  expansion over the source file               0.58x  (socket / source)
  messages on the stream                   3850  (255.9 rows each)
  throughput (arm 3, median)                 390034 rows/s, 3120269 cells/s

latency to the first row
  gRPC stream (min / median / p95)              7.3      7.4      7.4 ms
  a batch API cannot answer before           2457.1 ms  (arm 2 completes)

upload leg, counted separately
  OpenWorkbook for 105.7 MB                  610.6 ms  (173 MB/s)
  bytes written on the socket                 105.8 MB
  paid once per workbook, then any number of reads reuse the handle.
```
