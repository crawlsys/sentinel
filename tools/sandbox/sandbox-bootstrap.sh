#!/usr/bin/env bash
# sandbox-bootstrap.sh — provision the sandbox container so a
# fresh `claude` session inside it has everything needed to
# operate autonomously — not just to "test" sentinel, but to
# actively develop against it: create worktrees, edit, commit,
# rebase, push, file PRs.
#
# Provisions (each step idempotent — re-runs are cheap no-ops):
#   1. sentinel binary (cargo install from the mounted repo).
#   2. PATH wiring (/usr/local/bin if writable, else ~/.bashrc +
#      /etc/profile.d/sentinel-sandbox.sh).
#   3. gh CLI (GitHub CLI) installed via the official apt repo —
#      required for any `gh pr …` workflow and to satisfy any
#      gh-based git credential helper inherited from the host.
#   4. Container-local gitconfig + GIT_CONFIG_GLOBAL export. Host
#      ~/.gitconfig is bind-mounted read-only here; impossible to
#      scrub stale credential helpers or add `url.…insteadOf`
#      rules from inside the container. We hand git a writable
#      global file via GIT_CONFIG_GLOBAL, seeded with user.name/
#      email inherited from the ro-mounted host config + an
#      HTTPS→SSH URL rewrite for github.com (SSH is the only
#      working auth path here; host ~/.ssh is also ro-mounted
#      with the private key in place).
#   5. Worktree pointer repair — worktrees created on the host
#      have .git files pointing at /home/<user>/.../sentinel/
#      paths that don't resolve inside the container, so commits
#      from those worktrees fail. We rewrite pointers to the
#      container's /workspace/sentinel/... prefix and, where the
#      central .git is missing the worktree registration entirely,
#      re-add it with `git worktree add --force` against an
#      inferred branch.
#
# Invoked at container start by docker-compose.yml's inline
# command: directive (the sandbox-compose-override.yml file is a
# secondary/legacy artifact and is not the live entrypoint).
# First boot costs ~3 minutes for the cargo install; cached boots
# return in <1s.

set -euo pipefail

SBIN="${SBIN:-/workspace/.container-state/cargo-bin/bin}"
SENTINEL_REPO="${SENTINEL_REPO:-/workspace/sentinel}"
CONSUL_REPO="${CONSUL_REPO:-/workspace/legatus-consul-agent}"
STATE_DIR="${STATE_DIR:-/workspace/.container-state}"
CONTAINER_GITCONFIG="${CONTAINER_GITCONFIG:-$STATE_DIR/gitconfig}"

log() { printf 'sandbox-bootstrap: %s\n' "$*"; }

mkdir -p "$(dirname "$SBIN")"

if [[ -x "$SBIN/sentinel" ]]; then
    log "sentinel binary cached at $SBIN/sentinel — skipping build"
    "$SBIN/sentinel" --version
    return 0 2>/dev/null || exit 0
fi

if [[ ! -d "$CONSUL_REPO" ]]; then
    log "WARN legatus-consul-agent not mounted at $CONSUL_REPO"
    log "WARN sentinel-legatus needs the sibling repo to build."
    log "WARN check sandbox-compose-override.yml mounts."
    exit 1
fi

if [[ ! -d "$SENTINEL_REPO/crates/sentinel-cli" ]]; then
    log "WARN sentinel repo not mounted at $SENTINEL_REPO"
    exit 1
fi

log "building sentinel-cli (one-time, ~3min)..."
log "  repo: $SENTINEL_REPO"
log "  bin:  $SBIN"
cargo install \
    --path "$SENTINEL_REPO/crates/sentinel-cli" \
    --root "$(dirname "$SBIN")" \
    --locked \
    2>&1 | tail -10

if [[ ! -x "$SBIN/sentinel" ]]; then
    log "ERROR sentinel binary not produced; build failed"
    exit 1
fi

# Put sentinel on PATH. /usr/local/bin needs root; ~/.bashrc works
# unprivileged. We try the global path first (works if container
# entrypoint runs as root) then fall back to the dev user's shell
# rc. The compose override starts the container as the dev user
# so the bashrc path is what actually runs in practice.
if [[ -w /usr/local/bin ]]; then
    ln -sf "$SBIN/sentinel" /usr/local/bin/sentinel
    ln -sf "$SBIN/sentinel-engine" /usr/local/bin/sentinel-engine
    log "symlinked into /usr/local/bin"
else
    # Append once; idempotent on re-runs.
    BASHRC="${HOME}/.bashrc"
    if [[ -f "$BASHRC" ]] && ! grep -qF "$SBIN" "$BASHRC"; then
        printf '\n# sentinel cli (sandbox-bootstrap)\nexport PATH="%s:$PATH"\n' "$SBIN" >> "$BASHRC"
        log "appended PATH export to $BASHRC"
    fi
    # Profile.d works for non-interactive shells too, but only if writable.
    PROFD=/etc/profile.d/sentinel-sandbox.sh
    if [[ -w /etc/profile.d ]] && [[ ! -f "$PROFD" ]]; then
        printf 'export PATH="%s:$PATH"\n' "$SBIN" > "$PROFD"
        chmod a+r "$PROFD" 2>/dev/null || true
        log "wrote $PROFD"
    fi
fi

export PATH="$SBIN:$PATH"
log "ready · $(sentinel --version 2>&1 || echo unknown)"

# -- gh CLI -------------------------------------------------------------------
# Apt-based install via the official cli.github.com repo. Wrapped so
# a transient network/apt failure here does NOT abort the bootstrap —
# the sentinel binary is the primary deliverable, gh is supporting.
install_gh() {
    if command -v gh >/dev/null 2>&1; then
        log "gh already installed: $(gh --version | head -1)"
        return 0
    fi
    if ! command -v apt-get >/dev/null 2>&1; then
        log "WARN apt-get not available; skipping gh install (install manually for this image)"
        return 0
    fi
    # Need sudo for apt unless we're already root.
    local SUDO=""
    if [[ "$(id -u)" -ne 0 ]]; then
        if command -v sudo >/dev/null 2>&1 && sudo -n true 2>/dev/null; then
            SUDO="sudo"
        else
            log "WARN no passwordless sudo and not root; cannot install gh"
            return 0
        fi
    fi
    local KEYRING=/etc/apt/keyrings/githubcli-archive-keyring.gpg
    local SOURCES=/etc/apt/sources.list.d/github-cli.list
    if [[ ! -f "$KEYRING" ]]; then
        log "adding gh apt keyring"
        $SUDO mkdir -p -m 0755 /etc/apt/keyrings
        local tmp
        tmp="$(mktemp)"
        if wget -qO "$tmp" https://cli.github.com/packages/githubcli-archive-keyring.gpg; then
            $SUDO install -m 0644 "$tmp" "$KEYRING"
        else
            rm -f "$tmp"
            log "WARN failed to fetch gh keyring; skipping"
            return 0
        fi
        rm -f "$tmp"
    fi
    if [[ ! -f "$SOURCES" ]]; then
        log "adding gh apt sources"
        printf 'deb [arch=%s signed-by=%s] https://cli.github.com/packages stable main\n' \
            "$(dpkg --print-architecture)" "$KEYRING" \
            | $SUDO tee "$SOURCES" >/dev/null
    fi
    log "installing gh"
    $SUDO apt-get update -q -o Acquire::Retries=2 >/dev/null 2>&1 || {
        log "WARN apt update failed; skipping gh install"
        return 0
    }
    $SUDO apt-get install -y -q gh >/dev/null 2>&1 || {
        log "WARN apt install gh failed; skipping"
        return 0
    }
    log "gh installed: $(gh --version | head -1)"
}
install_gh || log "WARN install_gh returned non-zero (ignored)"

# -- container-local gitconfig -------------------------------------------------
# Host ~/.gitconfig is bind-mounted ro in this sandbox image; we
# cannot scrub stale credential helpers or add insteadOf rules
# from inside the container. Hand git a writable global file via
# GIT_CONFIG_GLOBAL instead. Re-run safe: rewritten each boot so
# changes to env vars / host config propagate cleanly.
setup_gitconfig() {
    mkdir -p "$(dirname "$CONTAINER_GITCONFIG")"

    # Inherit user.name / user.email from (read-only) host
    # ~/.gitconfig if present; otherwise from env; otherwise leave
    # blank and let `git commit` complain at use-time. We do NOT
    # synthesize a fake identity — silent wrong-author commits are
    # worse than a loud error.
    local name="" email=""
    if [[ -r "$HOME/.gitconfig" ]]; then
        name="$(git config --file "$HOME/.gitconfig" --get user.name 2>/dev/null || true)"
        email="$(git config --file "$HOME/.gitconfig" --get user.email 2>/dev/null || true)"
    fi
    name="${GIT_USER_NAME:-$name}"
    email="${GIT_USER_EMAIL:-$email}"

    {
        echo "# Container-local gitconfig — written by sandbox-bootstrap.sh."
        echo "# Host ~/.gitconfig is read-only mounted; this file is what git"
        echo "# actually reads via GIT_CONFIG_GLOBAL. Re-generated each boot."
        echo
        if [[ -n "$name" ]];  then echo "[user]";  echo "    name = $name";   fi
        if [[ -n "$email" ]]; then echo "    email = $email"; fi
        echo
        echo "[init]"
        echo "    defaultBranch = main"
        echo
        echo "# Force HTTPS GitHub URLs through SSH. SSH is the only working"
        echo "# auth path in this sandbox (host ~/.ssh is ro-mounted with the"
        echo "# key in place). Avoids depending on credential helpers entirely."
        echo "[url \"git@github.com:\"]"
        echo "    insteadOf = https://github.com/"
        echo
        echo "[pull]"
        echo "    rebase = false"
        echo
        echo "[push]"
        echo "    default = simple"
    } > "$CONTAINER_GITCONFIG"
    chmod 0644 "$CONTAINER_GITCONFIG"
    log "wrote $CONTAINER_GITCONFIG (user='${name:-<unset>}' email='${email:-<unset>}')"

    # Wire GIT_CONFIG_GLOBAL into the shell environment the same way
    # PATH is wired above. Idempotent — only appended if not present.
    local export_line="export GIT_CONFIG_GLOBAL=\"$CONTAINER_GITCONFIG\""
    local BASHRC="${HOME}/.bashrc"
    if [[ -f "$BASHRC" ]] && ! grep -qF "GIT_CONFIG_GLOBAL" "$BASHRC"; then
        printf '\n# git uses the container-local gitconfig (sandbox-bootstrap)\n%s\n' \
            "$export_line" >> "$BASHRC"
        log "appended GIT_CONFIG_GLOBAL export to $BASHRC"
    fi
    local PROFD=/etc/profile.d/sentinel-gitconfig.sh
    if [[ -w /etc/profile.d ]] && [[ ! -f "$PROFD" ]]; then
        printf '%s\n' "$export_line" > "$PROFD"
        chmod a+r "$PROFD" 2>/dev/null || true
        log "wrote $PROFD"
    fi
    export GIT_CONFIG_GLOBAL="$CONTAINER_GITCONFIG"

    # Sanity check: confirm git now sees the rewrite rule we just
    # installed. If something is shadowing it (e.g. /etc/gitconfig
    # set by the base image), the operator needs to know. Run from
    # /tmp so a broken worktree-pointer in the current directory
    # can't false-fail the check.
    if ! ( cd /tmp && git config --global --get-regexp 'url\..*\.insteadof' 2>/dev/null \
            | grep -q "git@github.com:" ); then
        log "WARN insteadOf rule not visible to git; check for /etc/gitconfig shadowing"
    fi
}
setup_gitconfig || log "WARN setup_gitconfig returned non-zero (ignored)"

# -- worktree pointer repair --------------------------------------------------
# Worktrees created on the host (under .worktrees/ or .claude/worktrees/)
# point their .git file at /home/<host-user>/.../sentinel/.git/worktrees/
# <name>, which doesn't resolve inside the container. Worse: when the
# container's .git is a different inode than the host's (typical when
# only the repo files are bind-mounted, not the whole .git tree), the
# central registration is *missing* in the container's .git/worktrees/.
# Rewriting the pointer alone won't help in that case — there's nothing
# to point at.
#
# Strategy: for each broken worktree, infer the branch from naming
# convention, then manually construct the central registration
# (HEAD + commondir + gitdir files) and align the worktree's .git
# pointer with it. This is what `git worktree add` does internally,
# but bypasses the CLI's refusal to operate on a non-empty target dir.
#
# Conservative: only acts on worktrees that fail `git rev-parse HEAD`.
# Healthy worktrees are skipped without inspection.
repair_worktree_pointers() {
    local SENTINEL_WT="${SENTINEL_REPO}"
    [[ -d "$SENTINEL_WT/.git" ]] || {
        log "WARN $SENTINEL_WT is not a git repo; skipping worktree repair"
        return 0
    }

    local registered=0 failed=0 healthy=0
    # Scan known worktree parent dirs. Add to this list as conventions evolve.
    local -a WT_PARENTS=(
        "$SENTINEL_WT/.worktrees"
        "$SENTINEL_WT/.claude/worktrees"
    )

    for parent in "${WT_PARENTS[@]}"; do
        [[ -d "$parent" ]] || continue
        local wt
        for wt in "$parent"/*/; do
            [[ -d "$wt" ]] || continue
            wt="${wt%/}"   # strip trailing slash
            local gitfile="$wt/.git"
            [[ -f "$gitfile" ]] || continue

            # Already-healthy: skip.
            if ( cd "$wt" && git rev-parse HEAD >/dev/null 2>&1 ); then
                healthy=$((healthy + 1))
                continue
            fi

            local wt_basename
            wt_basename="$(basename "$wt")"

            # Infer branch name from convention. Order: exact match,
            # then prefixed forms (feat/, fix/, docs/, etc.), then
            # EnterWorktree's worktree- prefix, then a last-resort
            # endswith-search across all local branches.
            local branch=""
            local cand
            for cand in \
                "${wt_basename}" \
                "feat/${wt_basename}" "fix/${wt_basename}" \
                "docs/${wt_basename}" "refactor/${wt_basename}" \
                "chore/${wt_basename}" \
                "worktree-${wt_basename}" \
                "worktree-feat/${wt_basename}" \
            ; do
                if ( cd "$SENTINEL_WT" && \
                     git rev-parse --verify "refs/heads/${cand}" >/dev/null 2>&1 ); then
                    branch="$cand"
                    break
                fi
            done
            # Last-resort fuzzy match: any local branch whose name
            # ENDS with the worktree basename (e.g. .worktrees/benches
            # → feat/sentinel-benches). Multiple matches → ambiguous,
            # bail out and ask the operator.
            if [[ -z "$branch" ]]; then
                local matches
                matches=$( cd "$SENTINEL_WT" && \
                    git for-each-ref --format='%(refname:short)' refs/heads/ 2>/dev/null \
                    | awk -v n="$wt_basename" 'index($0, n) && \
                        substr($0, length($0)-length(n)+1) == n')
                local match_count
                match_count=$(printf '%s\n' "$matches" | grep -c . || true)
                if [[ "$match_count" -eq 1 ]]; then
                    branch="$matches"
                elif [[ "$match_count" -gt 1 ]]; then
                    log "WARN $wt: $match_count local branches end with '$wt_basename' (ambiguous); manual repair needed"
                    failed=$((failed + 1))
                    continue
                fi
            fi
            if [[ -z "$branch" ]]; then
                log "WARN $wt: no local branch matches naming convention; manual repair needed"
                failed=$((failed + 1))
                continue
            fi

            # Refuse to clobber: if the branch is already checked out
            # in another worktree, registering it here would desync them.
            local conflicting_wt
            conflicting_wt="$( cd "$SENTINEL_WT" && \
                git worktree list --porcelain 2>/dev/null \
                | awk -v b="branch refs/heads/$branch" '
                    $0=="worktree "{ wt=$2 } $0==b { print wt; exit }
                ')"
            if [[ -n "$conflicting_wt" && "$conflicting_wt" != "$wt" ]]; then
                log "WARN $wt: branch $branch already checked out at $conflicting_wt; skipping"
                failed=$((failed + 1))
                continue
            fi

            # Construct the central registration. Three small files:
            #   HEAD       -> symbolic ref into refs/heads/<branch>
            #   commondir  -> relative path from this reg dir back to
            #                 the main .git (always "../..")
            #   gitdir     -> absolute path to the worktree's .git FILE,
            #                 which we then align to point back here.
            local reg="$SENTINEL_WT/.git/worktrees/$wt_basename"
            mkdir -p "$reg"
            printf 'ref: refs/heads/%s\n' "$branch" > "$reg/HEAD"
            printf '../..\n'                       > "$reg/commondir"
            printf '%s/.git\n' "$wt"               > "$reg/gitdir"
            printf 'gitdir: %s\n' "$reg"           > "$gitfile"

            # Verify the surgery worked. If not, roll back so we don't
            # leave a half-registered worktree that confuses git.
            if ( cd "$wt" && git rev-parse HEAD >/dev/null 2>&1 ); then
                log "registered $wt → $branch"
                registered=$((registered + 1))
            else
                rm -rf "$reg"
                log "WARN $wt: registration didn't take; rolled back (manual repair needed)"
                failed=$((failed + 1))
            fi
        done
    done

    log "worktree repair: healthy=$healthy registered=$registered failed=$failed"
}
repair_worktree_pointers || log "WARN repair_worktree_pointers returned non-zero (ignored)"
