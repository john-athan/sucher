#!/bin/sh
# Every check sucher has, in one script. The Makefile owns the list (fmt,
# clippy, both test runs, deny, the notices staleness check), so a person, `oss
# check` and CI all run the one definition of green.
set -eu
cd "$(dirname "$0")/.."
exec make check
