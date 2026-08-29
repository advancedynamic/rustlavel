# Benchmark results

Run from `20260829-232816`. Median of the repeated runs.
Requests per second, higher is better; p99 latency in milliseconds,
lower is better. Both matter — throughput alone hides a long tail.

| Case | axum | laravel-fpm | laravel-octane | loco | rustlavel | spring |
|---|---|---|---|---|---|---|
| Plaintext | 123,541 <br><small>p99 0.6 ms</small> | 5,588 <br><small>p99 25.6 ms</small> | 11,905 <br><small>p99 12.4 ms</small> | 120,700 <br><small>p99 0.6 ms</small> | 123,484 <br><small>p99 0.7 ms</small> | 94,807 <br><small>p99 1.3 ms</small> |
| JSON | 122,940 <br><small>p99 0.6 ms</small> | 4,818 <br><small>p99 26.9 ms</small> | 12,237 <br><small>p99 12.0 ms</small> | 120,801 <br><small>p99 0.7 ms</small> | 123,342 <br><small>p99 0.7 ms</small> | 94,793 <br><small>p99 1.3 ms</small> |
| Routing | 122,519 <br><small>p99 0.7 ms</small> | 5,845 <br><small>p99 20.5 ms</small> | 11,683 <br><small>p99 13.5 ms</small> | 121,050 <br><small>p99 0.6 ms</small> | 123,421 <br><small>p99 0.7 ms</small> | 95,649 <br><small>p99 1.3 ms</small> |
| Middleware ×5 | 121,513 <br><small>p99 0.7 ms</small> | 5,480 <br><small>p99 24.4 ms</small> | 10,446 <br><small>p99 15.1 ms</small> | 124,877 <br><small>p99 0.8 ms</small> | 123,793 <br><small>p99 0.8 ms</small> | 95,508 <br><small>p99 1.3 ms</small> |
| JSON ×100 | 124,950 <br><small>p99 0.8 ms</small> | 4,925 <br><small>p99 31.1 ms</small> | 9,894 <br><small>p99 17.5 ms</small> | 113,060 <br><small>p99 1.3 ms</small> | 85,337 <br><small>p99 1.6 ms</small> | 80,538 <br><small>p99 1.7 ms</small> |
| DB one row | 10,501 <br><small>p99 7.2 ms</small> | 656 <br><small>p99 146.4 ms</small> | 4,706 <br><small>p99 17.1 ms</small> | 10,212 <br><small>p99 8.3 ms</small> | 26,458 <br><small>p99 3.2 ms</small> | 24,867 <br><small>p99 7.7 ms</small> |
| DB relations | 5,245 <br><small>p99 14.5 ms</small> | 819 <br><small>p99 134.8 ms</small> | 2,146 <br><small>p99 46.6 ms</small> | 9,668 <br><small>p99 8.9 ms</small> | 11,953 <br><small>p99 8.1 ms</small> | 11,899 <br><small>p99 18.6 ms</small> |
| Template | 119,682 <br><small>p99 0.7 ms</small> | 5,343 <br><small>p99 24.8 ms</small> | 8,210 <br><small>p99 24.7 ms</small> | 116,003 <br><small>p99 1.2 ms</small> | 95,871 <br><small>p99 1.5 ms</small> | 37,604 <br><small>p99 4.1 ms</small> |

## Outside the request path

| | axum | laravel-fpm | laravel-octane | loco | rustlavel | spring |
|---|---|---|---|---|---|---|
| Startup | 113 ms | 587 ms | 1027 ms | 109 ms | 121 ms | 1829 ms |
| Memory (RSS) | 18 MB | 5 MB | 60 MB | 30 MB | 17 MB | 632 MB |
| Artifact | 2 MB | 44 MB | 44 MB | 14 MB | 4 MB | 26 MB |

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
