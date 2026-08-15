#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 /path/to/uqa-rs" >&2
  exit 2
fi

src="$(cd "$(dirname "$0")/.." && pwd)"
uqa="$(cd "$1" && pwd)"

for required in crates/uqa-engine crates/uqa-core; do
  if [[ ! -d "$uqa/$required" ]]; then
    echo "missing UQA-RS path: $uqa/$required" >&2
    exit 1
  fi
done

dst="$uqa/integrations/cairn"
if [[ -e "$dst" ]]; then
  echo "destination already exists: $dst" >&2
  exit 1
fi
mkdir -p "$(dirname "$dst")"
cp -a "$src" "$dst"
echo "$dst"
