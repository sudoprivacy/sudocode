#!/usr/bin/env bash
# Path translation shared by the A2A harnesses.
#
# `MSYS_NO_PATHCONV=1` is required so `/agents=<zone>` reaches the daemon as
# written — Git Bash otherwise rewrites it to `C:/Program Files/Git/agents=…`
# and the daemon refuses to boot on a topology it cannot parse. The same switch
# also stops the conversion the daemon's OTHER path arguments need, and a
# Windows binary reads `/tmp/x` as `C:\tmp\x` — a different directory from this
# shell's `/tmp`.
#
# So the daemon wrote its data where the cleanup never looked: eight abandoned
# data directories and 2.2 GB in `C:\tmp` before anyone noticed, because
# nothing in the harness reads the filesystem the daemon writes to — every
# assertion goes over gRPC, and a leak is invisible from there.
#
# Translate explicitly instead. Both functions are the identity where `cygpath`
# does not exist, which is every CI runner these scripts normally run on.

# A path the daemon will resolve the same way this shell does.
native_path() {
  if command -v cygpath >/dev/null 2>&1; then
    cygpath -w "$1"
  else
    printf '%s' "$1"
  fi
}

# A path this shell can test and delete, from one the daemon printed.
#
# Also normalises separators: the daemon joins its own, so a printed path can
# mix the two (`/tmp/x/data\agents\name`), and a backslash is a literal
# character to this shell rather than a separator.
shell_path() {
  local p="${1//\\//}"
  if command -v cygpath >/dev/null 2>&1; then
    cygpath -u "$p" 2>/dev/null || printf '%s' "$p"
  else
    printf '%s' "$p"
  fi
}
