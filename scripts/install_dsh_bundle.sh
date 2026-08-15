#!/bin/sh
set -eu
profile=${1:-web}
[ "$#" -eq 0 ] || shift
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
plugin="$root/integrations/deepseek-harness"
tarball="$root/dist/cairn-uqa-dsh-1.0.0.tgz"
fail() { printf 'install_dsh_bundle: %s\n' "$1" >&2; exit 1; }
command -v node >/dev/null 2>&1 || fail 'Node.js is required'
command -v dsh >/dev/null 2>&1 || fail 'dsh is not on PATH'
node - <<'NODE'
const [major, minor] = process.versions.node.split('.').map(Number)
if (!(major >= 24 || (major === 22 && minor >= 19))) {
  console.error(`install_dsh_bundle: DeepSeek Harness requires Node ^22.19.0 or >=24; found ${process.version}`)
  process.exit(1)
}
NODE
[ -f "$tarball" ] || fail "missing prepacked bundle: $tarball"
[ -f "$tarball.sha256" ] || fail "missing checksum: $tarball.sha256"
if command -v sha256sum >/dev/null 2>&1; then
  (cd "$(dirname -- "$tarball")" && sha256sum -c "$(basename -- "$tarball.sha256")") >/dev/null || fail 'bundle checksum failed'
elif command -v shasum >/dev/null 2>&1; then
  expected=$(awk '{print $1}' "$tarball.sha256"); actual=$(shasum -a 256 "$tarball" | awk '{print $1}')
  [ "$expected" = "$actual" ] || fail 'bundle checksum failed'
else fail 'sha256sum or shasum is required'; fi
printf 'Installing CAIRN bundle into profile %s...\n' "$profile"
dsh plugin --profile "$profile" add "$tarball"
dsh --profile "$profile" --dump-config >/dev/null
if [ "${CAIRN_DSH_SKIP_DOCTOR:-0}" != 1 ]; then "$plugin/bin/cairn-dsh-doctor.mjs" "$@"; fi
printf 'Ready: dsh --profile %s web\n' "$profile"
