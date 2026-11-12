#!/usr/bin/env bash
# Fails if a tree about to be public contains local paths, this machine's
# user / host names, secrets, or files that never belong in a repo.
#   scripts/check-public.sh [dir]   (default: current directory)
set -euo pipefail
cd "${1:-.}"
fail=0
hit() {
  echo "check-public: $*"
  fail=1
}
skip=(-path ./.git -prune -o -path ./target -prune -o -path ./.publish -prune -o)

while IFS= read -r f; do hit "forbidden file: $f"; done < <(find . "${skip[@]}" -type f \( \
  -name '.env*' -o -name '*.pem' -o -name '*.key' -o -name '*.p12' -o -name '*.pfx' \
  -o -name 'id_rsa*' -o -name 'id_ed25519*' -o -name '*.log' -o -name '*.sqlite*' \
  -o -name '.netrc' -o -name 'credentials*' \) -print)
while IFS= read -r f; do hit "file over 2 MiB: $f"; done < <(find . "${skip[@]}" -type f -size +2M -print)

patterns=(
  '/home/[A-Za-z0-9._-]+'
  '/Users/[A-Za-z0-9._-]+'
  '/root/'
  '[A-Za-z]:\\Users\\'
  '-----BEGIN [A-Z ]*PRIVATE KEY'
  'gh[pousr]_[A-Za-z0-9]{36}'
  'github_pat_[A-Za-z0-9_]{20,}'
  'AKIA[0-9A-Z]{16}'
  'AIza[0-9A-Za-z_-]{35}'
  'xox[abprs]-[A-Za-z0-9-]{10,}'
  'sk-[A-Za-z0-9_-]{32,}'
  '(api[_-]?key|secret|passw(or)?d|token)["'\'']?[[:space:]]*[:=][[:space:]]*["'\''][^"'\''[:space:]]{8,}'
)
# On a dev machine also look for who and where we are (CI runners are anonymous).
if [ -z "${CI:-}" ]; then
  patterns+=("$HOME" "${USER:-$(id -un)}@" "@$(hostname)")
  host="$(hostname)"
  [ ${#host} -ge 3 ] && patterns+=("\\b${host}\\b")
  [ ${#USER} -ge 5 ] && patterns+=("\\b${USER}\\b")
  for email in "$(git config --global user.email || true)" "$(git config user.email || true)"; do
    [ -n "$email" ] && [[ "$email" != *noreply* ]] && patterns+=("$email")
  done
fi

for p in "${patterns[@]}"; do
  if out=$(grep -rnIE --exclude-dir=.git --exclude-dir=target --exclude-dir=.publish \
    --exclude=check-public.sh -e "$p" . 2>/dev/null); then
    hit "matches /$p/:"
    echo "$out" | cut -c1-200 | head -20
  fi
done

[ "$fail" = 0 ] && echo "check-public: clean"
exit "$fail"
