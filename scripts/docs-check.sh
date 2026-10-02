#!/bin/sh
# The documentation-currency gate (root AGENTS.md, "Standing documentation rule").
#
# A commit that changes files under `crates/<crate>/` must also touch that
# crate's README.md or AGENTS.md in the same changeset — the crate's
# documentation (features, bug fixes, usage examples, process changes) is part
# of the change, not an afterthought. Root-level files are checked leniently:
# code-supporting root changes without a root README/AGENTS touch print a
# review note but do not fail the gate.
#
# Usage:
#   scripts/docs-check.sh           # auto: staged, else tree, else last commit
#   scripts/docs-check.sh cached    # pre-commit: the staged set only
#   scripts/docs-check.sh tree      # working tree vs HEAD (staged + unstaged)
#   scripts/docs-check.sh commit    # the HEAD commit vs its parent
#
# Exit 0 on pass, 1 on failure.

set -u

cd "$(git rev-parse --show-toplevel)" || exit 1

MODENAME=${1:-auto}
case "$MODENAME" in
    -h|--help)
        sed -n '2,20p' "$0"
        exit 0
        ;;
    auto|cached|tree|commit) ;;
    *) printf 'docs-check: unknown mode "%s"\n' "$MODENAME"; exit 1 ;;
esac

changed() {
    case "$MODENAME" in
        cached) git diff --cached --name-only --diff-filter=ACMR ;;
        tree)   git diff HEAD --name-only --diff-filter=ACMR ;;
        commit) git diff "HEAD^" --name-only --diff-filter=ACMR ;;
        auto)
            if [ -n "$(git diff --cached --name-only --diff-filter=ACMR)" ]; then
                git diff --cached --name-only --diff-filter=ACMR
            elif [ -n "$(git diff HEAD --name-only --diff-filter=ACMR)" ]; then
                git diff HEAD --name-only --diff-filter=ACMR
            elif git rev-parse --verify --quiet HEAD >/dev/null; then
                git diff "HEAD^" --name-only --diff-filter=ACMR
            fi
            ;;
    esac
}

# Working set for the auto mode's decision on which files to inspect: the
# whole diff is re-listed per mode call below (POSIX sh: no arrays).
CHANGED=$(changed)
if [ -z "$CHANGED" ]; then
    printf 'docs-check: nothing changed; passing\n'
    exit 0
fi

violations=
root_review=
docs_only=1

# --- per-crate path partitions ------------------------------------------------
# Auto mode resolved to diff listing `CHANGED` above; crate code changes need
# a doc touch. Whitespace-only changes (a rustfmt pass, say) change no
# documentation content and don't trigger the rule; the substantive list
# carries the files whose diff survives whitespace-ignoring.
SUBSTANTIVE=
for file in $CHANGED; do
    case "$file" in
        .DS_Store|Cargo.lock|target/*) continue ;;
    esac
    case "$MODENAME" in
        cached) if [ -n "$(git diff --cached -w -- "$file")" ]; then SUBSTANTIVE="$SUBSTANTIVE $file"; fi ;;
        tree)   if [ -n "$(git diff HEAD -w -- "$file")" ]; then SUBSTANTIVE="$SUBSTANTIVE $file"; fi ;;
        commit) if [ -n "$(git diff "HEAD^" -w -- "$file")" ]; then SUBSTANTIVE="$SUBSTANTIVE $file"; fi ;;
    esac
done

docs_only=1
for file in $SUBSTANTIVE; do
    case "$file" in
        crates/*) docs_only=0; break ;;
        *) docs_only=0 ;;
    esac
done

# Auto mode resolved to diff listing `CHANGED` above; crate code changes need
# a doc touch. Emit violation lines when a crate changed non-doc files but
# touched neither of its two docs.
for crate in $(printf '%s\n' "$SUBSTANTIVE" | sed -n 's|^crates/\([^/]*\)/.*$|\1|p' | sort -u); do
    code_changed=0
    doc_changed=0
    for file in $SUBSTANTIVE; do
        case "$file" in
            "crates/$crate/README.md"|"crates/$crate/AGENTS.md")
                doc_changed=1
                ;;
            "crates/$crate/"*)
                code_changed=1
                ;;
        esac
    done
    if [ "$code_changed" = 1 ] && [ "$doc_changed" = 0 ]; then
        violations="$violations $crate"
    fi
done

# Root-level review note (never fails): "keep it to the overall" means the
# root pair follows repo-level changes; a reviewer decides, the gate hints.
root_doc_changed=0
for file in $CHANGED; do
    case "$file" in
        README.md|AGENTS.md) root_doc_changed=1 ;;
    esac
done
root_others=
for file in $CHANGED; do
    case "$file" in
        README.md|AGENTS.md|.DS_Store|Cargo.lock|target/*) continue ;;
        crates/*) continue ;;
        *) root_others="$root_others $file" ;;
    esac
done
if [ -n "$root_others" ] && [ "$root_doc_changed" = 0 ]; then
    root_review=1
fi

printf 'docs-check: mode=%s, %s changed file(s)\n' "$MODENAME" "$(printf '%s\n' "$CHANGED" | grep -c .)"

if [ "$docs_only" = 1 ]; then
    printf 'docs-check: pass (documentation-only changeset)\n'
    exit 0
fi

status=0
if [ -n "$violations" ]; then
    for crate in $violations; do
        printf 'docs-check: FAIL crate `%s` changed without touching crates/%s/README.md or crates/%s/AGENTS.md in the same changeset.\n' "$crate" "$crate" "$crate"
        printf 'docs-check:      keep the README current (features, bug fixes, usage examples) and AGENTS.md current\n'
        printf 'docs-check:      (development/testing/documentation/contribution processes, evaluation criteria).\n'
    done
    status=1
fi
if [ -n "${root_review:-}" ]; then
    printf 'docs-check: REVIEW root-level change(s)%s without a root README.md/AGENTS.md touch (reviewer call, not a gate failure).\n' "$root_others"
fi
if [ "$status" = 0 ]; then
    printf 'docs-check: pass\n'
fi
exit $status
