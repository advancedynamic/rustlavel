# Benchmark results

Run from `20260829-225906`. Median of the repeated runs.
Requests per second, higher is better; p99 latency in milliseconds,
lower is better. Both matter — throughput alone hides a long tail.

| Case | axum | laravel-fpm | laravel-octane | loco | rustlavel | spring |
|---|---|---|---|---|---|---|
| Plaintext | 123,783 <br><small>p99 0.6 ms</small> | 6,534 <br><small>p99 16.9 ms</small> | 13,866 <br><small>p99 9.4 ms</small> | 122,242 <br><small>p99 0.6 ms</small> | 123,431 <br><small>p99 0.7 ms</small> | 100,036 <br><small>p99 1.3 ms</small> |
| JSON | 123,538 <br><small>p99 0.6 ms</small> | 6,457 <br><small>p99 18.2 ms</small> | 13,980 <br><small>p99 9.0 ms</small> | 122,188 <br><small>p99 0.6 ms</small> | 124,940 <br><small>p99 0.7 ms</small> | 97,228 <br><small>p99 1.3 ms</small> |
| Routing | 123,314 <br><small>p99 0.6 ms</small> | 6,461 <br><small>p99 18.1 ms</small> | 13,741 <br><small>p99 9.3 ms</small> | 122,545 <br><small>p99 0.6 ms</small> | 125,385 <br><small>p99 0.7 ms</small> | 98,569 <br><small>p99 1.3 ms</small> |
| Middleware ×5 | 121,707 <br><small>p99 0.6 ms</small> | 6,274 <br><small>p99 18.8 ms</small> | 12,730 <br><small>p99 9.9 ms</small> | 135,831 <br><small>p99 0.9 ms</small> | 126,115 <br><small>p99 0.7 ms</small> | 97,808 <br><small>p99 1.3 ms</small> |
| JSON ×100 | 123,641 <br><small>p99 0.8 ms</small> | 5,816 <br><small>p99 22.5 ms</small> | 11,224 <br><small>p99 12.6 ms</small> | 123,979 <br><small>p99 1.0 ms</small> | 78,774 <br><small>p99 2.1 ms</small> | 88,848 <br><small>p99 1.6 ms</small> |
| DB one row | 10,592 <br><small>p99 7.1 ms</small> | 875 <br><small>p99 99.7 ms</small> <br><small>⚠ 2 run(s) failed</small> | 5,349 <br><small>p99 16.2 ms</small> | 10,537 <br><small>p99 7.1 ms</small> | 26,763 <br><small>p99 3.2 ms</small> | 26,448 <br><small>p99 7.2 ms</small> |
| DB relations | 5,315 <br><small>p99 13.9 ms</small> | 718 <br><small>p99 113.0 ms</small> | 2,832 <br><small>p99 29.0 ms</small> | 9,803 <br><small>p99 8.0 ms</small> | 12,553 <br><small>p99 6.9 ms</small> | 12,823 <br><small>p99 17.3 ms</small> |
| Template | 119,927 <br><small>p99 0.6 ms</small> | 6,020 <br><small>p99 18.9 ms</small> | 11,520 <br><small>p99 11.0 ms</small> | 120,933 <br><small>p99 1.1 ms</small> | 99,960 <br><small>p99 1.4 ms</small> | 40,867 <br><small>p99 3.8 ms</small> |

## Outside the request path

| | axum | laravel-fpm | laravel-octane | loco | rustlavel | spring |
|---|---|---|---|---|---|---|
| Startup | 110 ms | 600 ms | 2884 ms | 105 ms | 109 ms | 1781 ms |
| Memory (RSS) | 19 MB | 5 MB | 60 MB | 30 MB | 17 MB | 610 MB |
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
