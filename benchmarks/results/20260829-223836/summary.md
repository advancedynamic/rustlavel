# Benchmark results

Run from `20260829-223836`. Median of the repeated runs.
Requests per second, higher is better; p99 latency in milliseconds,
lower is better. Both matter — throughput alone hides a long tail.

| Case | rustlavel |
|---|---|
| Plaintext | 123,744 <br><small>p99 0.7 ms</small> |
| JSON | 123,534 <br><small>p99 0.8 ms</small> |
| Routing | 124,775 <br><small>p99 0.8 ms</small> |
| Middleware ×5 | 124,815 <br><small>p99 0.9 ms</small> |
| JSON ×100 | 76,471 <br><small>p99 2.1 ms</small> |
| DB one row | 26,916 <br><small>p99 3.1 ms</small> |
| DB relations | 12,470 <br><small>p99 7.7 ms</small> |
| Template | 88,761 <br><small>p99 1.8 ms</small> |

## Outside the request path

| | rustlavel |
|---|---|
| Startup | 106 ms |
| Memory (RSS) | 15 MB |
| Artifact | 4 MB |

## Reading these honestly

- Measured on one developer machine, not a server. The *ratios* between
  the apps mean something; the absolute numbers do not travel.
- PostgreSQL runs in Docker, which on macOS means a virtual machine.
  Every database row is therefore pessimistic in the same direction for
  every app, which keeps the comparison fair but the absolutes wrong.
- The load generator shares a machine with the thing it is loading, so
  the fastest cases are partly measuring `oha`.
- Written by the author of one of the frameworks being compared. Treat
  a result flattering that one with more suspicion than the rest.
