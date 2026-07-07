#!/usr/bin/env bash
# cc_contract_check.sh — verify sentinel's Claude Code boundary assumptions
# against a deobfuscated CC bundle, and flag drift.
#
# Sentinel depends on dozens of CC-side contracts: env vars CC exports into hook
# children, hook-payload field names, protocol method strings, hook event names.
# When CC silently renames one of these (e.g. tool_result → tool_response), a
# sentinel hook goes dark with no error — invisible to greps and unit tests
# because nothing in-repo knows CC changed. This script closes that gap: each
# assumption in cc-boundary-contract.tsv is checked against the actual CC source,
# so drift is caught on every CC version step-up instead of in production.
#
# Usage:
#   scripts/cc_contract_check.sh [BUNDLE_JS]
#
# BUNDLE_JS defaults to the newest decompile-output bundle under
# ~/Documents/GitHub/claude-code-src/decompile-output/*/decompiled.js
#
# Exit codes:
#   0  all must-exist assumptions hold (WARNs allowed)
#   1  one or more must-exist assumptions FAILED (drift — a hook may be broken)
#   2  usage / bundle-not-found error
set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MANIFEST="$SCRIPT_DIR/cc-boundary-contract.tsv"

# ---- locate the bundle -----------------------------------------------------
BUNDLE="${1:-}"
if [ -z "$BUNDLE" ]; then
  BASE="$HOME/Documents/GitHub/claude-code-src/decompile-output"
  # newest decompiled.js by mtime
  BUNDLE="$(ls -t "$BASE"/*/decompiled.js 2>/dev/null | head -1)"
fi
if [ -z "$BUNDLE" ] || [ ! -f "$BUNDLE" ]; then
  echo "ERROR: bundle not found. Pass a decompiled.js path, or generate one under" >&2
  echo "       ~/Documents/GitHub/claude-code-src/decompile-output/<ver>/decompiled.js" >&2
  exit 2
fi
if [ ! -f "$MANIFEST" ]; then
  echo "ERROR: manifest not found at $MANIFEST" >&2
  exit 2
fi

echo "CC boundary contract check"
echo "  bundle:   $BUNDLE"
echo "  manifest: $MANIFEST"
echo "  bundle version markers: $(grep -oE 'VERSION:"[0-9]+\.[0-9]+\.[0-9]+"' "$BUNDLE" | head -1)"
echo

pass=0 fail=0 warn=0
fail_ids=""

# ---- iterate manifest rows -------------------------------------------------
# Read tab-separated fields; skip blanks and comments.
while IFS=$'\t' read -r id kind must_exist severity sentinel_ref expect note; do
  case "$id" in ""|\#*) continue ;; esac
  [ -z "${expect:-}" ] && continue

  hits="$(grep -cE -- "$expect" "$BUNDLE" 2>/dev/null || true)"
  hits="${hits:-0}"

  status=""
  case "$must_exist" in
    true)
      if [ "$hits" -gt 0 ]; then status="PASS"; else status="FAIL"; fi ;;
    false)
      if [ "$hits" -eq 0 ]; then status="PASS"; else status="FAIL"; fi ;;
    warn)
      if [ "$hits" -gt 0 ]; then status="WARN"; else status="PASS"; fi ;;
    *)
      status="FAIL" ;;   # malformed manifest row
  esac

  case "$status" in
    PASS) pass=$((pass+1)); mark="  ok  " ;;
    WARN) warn=$((warn+1)); mark=" WARN " ;;
    FAIL) fail=$((fail+1)); mark=" FAIL "; fail_ids="$fail_ids $id" ;;
  esac

  printf '[%s] %-26s %-11s hits=%-4s %s\n' "$mark" "$id" "$kind/$severity" "$hits" "$sentinel_ref"
  if [ "$status" = "FAIL" ] || [ "$status" = "WARN" ]; then
    printf '            → %s\n' "$note"
    printf '            expect: /%s/\n' "$expect"
  fi
done < "$MANIFEST"

echo
echo "Summary: $pass pass, $warn warn, $fail fail"
if [ "$fail" -gt 0 ]; then
  echo "DRIFT DETECTED — a sentinel hook may be broken by a CC change:$fail_ids"
  echo "Update the affected sentinel code, then update cc-boundary-contract.tsv."
  exit 1
fi
if [ "$warn" -gt 0 ]; then
  echo "No breaking drift; $warn deprecation warning(s) — plan migration before CC removes them."
fi
exit 0
