#!/usr/bin/env bash
# tag-audit.sh — workstream-boundary hygiene check.
#
# Greps the sentinel-viz-{api,next} trees for cross-boundary call
# sites that aren't annotated with a `WORKSTREAM:` comment within
# the surrounding 10 lines. Exits non-zero if any unannotated
# touchpoint is found.
#
# Context: see SEPARATION.md in each crate. The viz work is intended
# to peel off into a standalone `sentinel-viz` repo; every place that
# reaches across to sentinel-bridge, claude-code, or the other half
# of viz itself needs a labelled comment so a future split is
# mechanical.
#
# Usage:
#   tools/sentinel-viz-api/scripts/tag-audit.sh
#
# Exit:
#   0  → no unannotated cross-boundary calls found
#   1  → at least one offender; printed to stderr
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"   # tools/

API_DIR="${ROOT}/sentinel-viz-api"
WEB_DIR="${ROOT}/sentinel-viz-next"

if [[ ! -d "${API_DIR}" || ! -d "${WEB_DIR}" ]]; then
    echo "tag-audit: expected ${API_DIR} and ${WEB_DIR} to exist" >&2
    exit 2
fi

# Cross-boundary patterns. A line containing any of these is OK if
# a `WORKSTREAM:` annotation appears within the surrounding 10
# lines (either direction).
PATTERNS=(
    'activegraph-bridge'
    '\.claude/projects'
    '\.claude-sentinel/projects'
    'NEXT_PUBLIC_VIZ_API'
)

# Audit window: how many lines before AND after the match to scan
# for a WORKSTREAM: annotation. 15 covers a typical doc block on
# top of a function whose first body line is the boundary.
WIN=15

# Path-skip patterns. Tests, fixtures, build artefacts, and
# documentation are NOT runtime-boundary code.
skip_path() {
    case "$1" in
        */scripts/tag-audit.sh)  return 0 ;;
        */SEPARATION.md)         return 0 ;;
        */target/*)              return 0 ;;
        */node_modules/*)        return 0 ;;
        */.next/*)               return 0 ;;
        */tests/*)               return 0 ;;
        */playwright.config.ts)  return 0 ;;
        *.md)                    return 0 ;;
        *.json)                  return 0 ;;
        *.lock)                  return 0 ;;
        *)                       return 1 ;;
    esac
}

errors=0
for d in "${API_DIR}" "${WEB_DIR}"; do
    while IFS= read -r match; do
        [[ -z "${match}" ]] && continue
        file="${match%%:*}"
        rest="${match#*:}"
        line_no="${rest%%:*}"
        if skip_path "${file}"; then
            continue
        fi
        # Scan WIN lines before and after for a WORKSTREAM: marker.
        lo=$(( line_no > WIN ? line_no - WIN : 1 ))
        hi=$(( line_no + WIN ))
        if awk -v lo="${lo}" -v hi="${hi}" 'NR>=lo && NR<=hi' "${file}" \
                | grep -q 'WORKSTREAM:'; then
            continue
        fi
        printf 'tag-audit: untagged cross-boundary call → %s:%s\n' "${file}" "${line_no}" >&2
        errors=$((errors + 1))
    done < <(grep -rnE "$(IFS='|'; echo "${PATTERNS[*]}")" "${d}" 2>/dev/null || true)
done

if (( errors > 0 )); then
    echo "tag-audit: ${errors} unannotated cross-boundary call(s) found" >&2
    exit 1
fi
echo "tag-audit: clean — all cross-boundary calls carry a WORKSTREAM: annotation"
