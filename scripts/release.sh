#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<EOF >&2
Usage: $(basename "$0") [--middle] -m "subject" [-m "body"]
EOF
}

increment_middle=0
messages=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --middle) increment_middle=1; shift ;;
    -m)
      if [[ $# -lt 2 ]]; then
        usage
        exit 2
      fi
      messages+=("$2")
      shift 2
      ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; usage; exit 2 ;;
  esac
done

if [[ "${#messages[@]}" -eq 0 ]]; then
  usage
  exit 2
fi

new_version="$(python3 - "$increment_middle" <<'PY'
import re
import sys
from pathlib import Path

middle = sys.argv[1] == "1"
path = Path("Cargo.toml")
text = path.read_text()
match = re.search(r'(?m)^version\s*=\s*"(\d+)\.(\d+)\.(\d+)"', text)
if not match:
    raise SystemExit("Cargo.toml version not found")
major, minor, patch = map(int, match.groups())
if middle:
    minor += 1
    patch = 0
else:
    patch += 1
version = f"{major}.{minor}.{patch}"
text = text[:match.start()] + f'version = "{version}"' + text[match.end():]
path.write_text(text)
print(version)
PY
)"

cargo generate-lockfile

git add Cargo.toml Cargo.lock
commit_args=()
for message in "${messages[@]}"; do
  commit_args+=("-m" "$message")
done
git commit "${commit_args[@]}"
git tag "$new_version"
git push
git push origin "$new_version"

echo "Released $new_version"
