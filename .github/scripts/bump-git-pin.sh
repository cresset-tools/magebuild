#!/usr/bin/env bash
# Re-pin every Cargo.toml git dependency on one GitHub repository to the commit
# of that repository's newest release, and refresh Cargo.lock.
#
#   bump-git-pin.sh <owner/repo>
#
# Run from the magebuild root. Needs `gh` (authenticated through GH_TOKEN) and
# `cargo`. Called by .github/workflows/bump-git-pins.yml, which reads the
# `changed`, `tag`, `new` and `crates` outputs written to $GITHUB_OUTPUT.
#
# For a dry run without network access, set BUMP_NEW_REV (and optionally
# BUMP_TAG) to skip the release lookup, and BUMP_SKIP_LOCK=1 to leave
# Cargo.lock alone.
set -euo pipefail

repo=${1:?usage: bump-git-pin.sh <owner/repo>}
out=${GITHUB_OUTPUT:-/dev/stdout}
url="https://github.com/${repo}"

# Dependency lines that pin this repository: `name = { git = "<url>", rev = … }`.
# The closing quote after the URL keeps `…/magequery` from matching a
# hypothetical `…/magequery-extras`.
pins=$(grep -E "^[A-Za-z0-9_-]+ *= *\{.*git *= *\"${url}(\.git)?\"" Cargo.toml || true)
if [ -z "${pins}" ]; then
  echo "no git dependency on ${repo} in Cargo.toml" >&2
  exit 1
fi
crates=$(printf '%s\n' "${pins}" | sed -E 's/^([A-Za-z0-9_-]+).*/\1/')
revs=$(printf '%s\n' "${pins}" | grep -oE 'rev *= *"[a-f0-9]{7,40}"' | grep -oE '[a-f0-9]{7,40}' | sort -u)

if [ -n "${BUMP_NEW_REV:-}" ]; then
  tag=${BUMP_TAG:-manual}
  new=${BUMP_NEW_REV}
else
  # Newest published release -> the commit its tag points at.
  tag=$(gh release view --repo "${repo}" --json tagName -q .tagName)
  new=$(gh api "repos/${repo}/commits/${tag}" -q .sha)
fi

echo "latest release ${tag} -> ${new}"
echo "current pin(s)  -> $(echo ${revs})"
if [ "${revs}" = "${new}" ]; then
  echo "Already on the latest release — nothing to do."
  echo "changed=false" >> "${out}"
  exit 0
fi

# Never move a pin backwards. A pin is sometimes set by hand to a commit past
# the newest release (to use a fix before it is released); re-pinning to that
# release would silently drop the fix. Bump only when the release has commits
# some current pin lacks: `compare/<pin>...<release>` reports the release as
# `ahead` of or `diverged` from the pin. `behind` and `identical` mean the pin
# already contains the release.
if [ -z "${BUMP_NEW_REV:-}" ]; then
  needed=false
  for cur in ${revs}; do
    status=$(gh api "repos/${repo}/compare/${cur}...${new}" -q .status)
    echo "release vs pin ${cur}: ${status}"
    case "${status}" in
      behind|identical) ;;
      *) needed=true ;;
    esac
  done
  if [ "${needed}" = false ]; then
    echo "The current pin already contains ${tag} — nothing to do."
    echo "changed=false" >> "${out}"
    exit 0
  fi
fi

# Rewrite the rev only on this repository's lines, so two repositories that
# happen to share a rev prefix, or crates of one repository that drifted onto
# different revs, all end up on the new commit.
sed -i -E "\\|git *= *\"${url}(\\.git)?\"|s/rev *= *\"[a-f0-9]{7,40}\"/rev = \"${new}\"/" Cargo.toml

# Refresh the lock so the PR builds without a stale [[package]] entry.
if [ -z "${BUMP_SKIP_LOCK:-}" ]; then
  args=()
  for c in ${crates}; do args+=(-p "${c}"); done
  cargo update "${args[@]}"
fi

{
  echo "changed=true"
  echo "tag=${tag}"
  echo "new=${new}"
  echo "crates=$(printf '`%s` ' ${crates} | sed -E 's/ $//; s/ / \/ /g')"
} >> "${out}"
