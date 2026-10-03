#!/bin/sh
#
# Fail if a test suite that this run was supposed to execute skipped itself.
#
#   check-skips.sh LOG VAR [VAR ...]
#
# **Why this exists.** Every test that needs a server skips itself, with a
# printed line, when its environment variable is unset — which is what keeps
# `cargo test` green on a laptop with no Docker. The same property means a CI
# job that forgets to set a variable goes *green* while running nothing, and
# stays green forever. That is not a hypothetical: the ledger, payment, OAuth
# and billing suites were absent from CI entirely, and the queue was only ever
# run against PostgreSQL, so it worked on nothing else for as long as nobody
# looked — until a MySQL run found four independent reasons it could not.
#
# LOG is the output of a `cargo test ... -- --nocapture` run, because a skip
# message is printed, not returned. Each VAR is a variable this job promised to
# provide; a skip message naming one is a promise broken.
#
# The match is on the whole variable name. `MYSQL_URL` is a substring of
# `REVOCATION_MYSQL_URL`, whose suite is deliberately a nightly job's, and a
# check that cried wolf about it would be switched off within a week.
#
# Two spellings are in use — "skipped: set X to run …" and "skipping: X is not
# set" — and both are matched. A third would silently defeat this, so the
# script also refuses a log with no test results in it at all: a detector that
# read nothing has proved nothing.

LOG="$1"
shift

if [ -z "$LOG" ] || [ $# -eq 0 ]; then
  echo "usage: $0 LOG VAR [VAR ...]" >&2
  exit 2
fi
if [ ! -s "$LOG" ]; then
  echo "check-skips: '$LOG' is missing or empty, so nothing was checked" >&2
  exit 2
fi
if ! grep -q "test result:" "$LOG"; then
  echo "check-skips: '$LOG' has no 'test result:' lines — it is not a test run" >&2
  exit 2
fi

status=0
for var in "$@"; do
  hits=$(grep -E "skipp(ed|ing):" "$LOG" | grep -E "(^|[^A-Z_])${var}([^A-Z_]|\$)" | sort -u)
  if [ -n "$hits" ]; then
    echo "SKIPPED although $var was meant to be set:" >&2
    echo "$hits" | sed 's/^/    /' >&2
    status=1
  fi
done

if [ $status -eq 0 ]; then
  echo "check-skips: no suite skipped itself for: $*"
fi
exit $status
