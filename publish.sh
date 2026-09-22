#!/bin/zsh
#
# Publish every crate in the workspace, in the order `publish-order.py` works
# out, at whatever version the manifests say.
#
# **Resumable, because a release is interrupted more often than it is not.** A
# crate already published at this version is skipped rather than treated as a
# failure, so running this again after a stop, a rate limit or a dropped
# network picks up where it left off. The 0.8.1 release was paused after four
# crates and resumed cleanly; that is the case this is written for.
#
# **Stops at the first real failure**, leaving the rest unpublished. That is
# deliberate: a half-published release is recoverable — Cargo resolves through
# version ranges, so nobody can install a half-matched set while the meta-crate
# is still at the old version — whereas a wrong crate pushed to crates.io
# cannot be taken back, only yanked.
#
#   ./publish.sh              publish
#   ./publish.sh --dry-run    say what would happen, touch nothing
#
# Verify afterwards from the registry rather than from this script's output:
#
#   for c in $(python3 publish-order.py); do
#       printf '%-28s %s\n' "$c" \
#         "$(curl -s https://crates.io/api/v1/crates/$c | python3 -c \
#            'import sys,json; print(json.load(sys.stdin)["crate"]["max_version"])')"
#   done

set -e
cd "${0:A:h}" || exit 1

DRY_RUN=0
[ "$1" = "--dry-run" ] && DRY_RUN=1

# `publish-order.py` prints its summary on stderr, so stdout is exactly the
# list. Reading it with the summary included once cost a release its first
# crate: `tail -n +2` dropped `rustlavel-core` instead of the header.
ORDER=(${(f)"$(python3 publish-order.py 2>/dev/null)"})

# Two guards against publishing a truncated list. The count is taken from the
# tree rather than written here, so adding a crate does not silently disarm it.
EXPECTED=$(ls -d framework/crates/*/ | wc -l | tr -d ' ')
if [ ${#ORDER} -ne "$EXPECTED" ]; then
  echo "expected $EXPECTED crates, got ${#ORDER} — refusing to publish a partial list" >&2
  exit 1
fi
if [ "${ORDER[1]}" != "rustlavel-core" ]; then
  echo "the order does not start at rustlavel-core (got ${ORDER[1]}) — refusing" >&2
  exit 1
fi

VERSION=$(grep -m1 '^version' framework/Cargo.toml | cut -d'"' -f2)
echo "publishing ${#ORDER} crates at $VERSION"
[ $DRY_RUN -eq 1 ] && echo "(dry run — nothing will be uploaded)"

n=0
for crate in $ORDER; do
  n=$((n + 1))
  printf '\n=== [%d/%d] %s ===\n' $n ${#ORDER} $crate

  if [ $DRY_RUN -eq 1 ]; then
    echo "would publish $crate"
    continue
  fi

  attempt=0
  while true; do
    attempt=$((attempt + 1))
    out=$(cd framework && cargo publish -p "$crate" 2>&1)
    code=$?

    if [ $code -eq 0 ]; then
      echo "published $crate"
      break
    fi

    if echo "$out" | grep -q "already exists\|already uploaded"; then
      echo "$crate is already at this version — skipping"
      break
    fi

    # crates.io rate-limits new crates harder than new versions. Waiting is the
    # correct response to a 429; failing the release is not.
    if echo "$out" | grep -qi "429\|too many requests\|rate limit"; then
      if [ $attempt -ge 8 ]; then
        echo "RATE LIMITED past patience on $crate" >&2
        echo "$out" | tail -20 >&2
        exit 1
      fi
      wait=$((attempt * 120))
      echo "rate limited on $crate; waiting ${wait}s (attempt $attempt)"
      sleep $wait
      continue
    fi

    echo "FAILED on $crate after $attempt attempt(s)" >&2
    echo "$out" | tail -30 >&2
    echo >&2
    echo "The crates before this one are published and cannot be withdrawn." >&2
    echo "Fix the cause, then run this again — it resumes from here." >&2
    exit 1
  done
done

echo
echo "ALL_PUBLISHED"
