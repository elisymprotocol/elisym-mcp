#!/usr/bin/env bash
# Sync version from Cargo.toml to npm/package.json, server.json, and platform packages.
# Usage: ./scripts/sync-version.sh

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CARGO_TOML="$REPO_ROOT/Cargo.toml"
PACKAGE_JSON="$REPO_ROOT/npm/package.json"
SERVER_JSON="$REPO_ROOT/server.json"

VERSION=$(grep '^version' "$CARGO_TOML" | head -1 | sed 's/.*"\(.*\)".*/\1/')

if [ -z "$VERSION" ]; then
  echo "Error: could not extract version from $CARGO_TOML" >&2
  exit 1
fi

# Update main version and all optionalDependencies versions
# Uses a temp file for portability (macOS + Linux sed differ)
node -e "
const fs = require('fs');
const pkg = JSON.parse(fs.readFileSync('$PACKAGE_JSON', 'utf8'));
pkg.version = '$VERSION';
if (pkg.optionalDependencies) {
  for (const key of Object.keys(pkg.optionalDependencies)) {
    pkg.optionalDependencies[key] = '$VERSION';
  }
}
fs.writeFileSync('$PACKAGE_JSON', JSON.stringify(pkg, null, 2) + '\n');
"

# Update server.json (top-level version + packages[].version)
if [ -f "$SERVER_JSON" ]; then
  node -e "
const fs = require('fs');
const srv = JSON.parse(fs.readFileSync('$SERVER_JSON', 'utf8'));
srv.version = '$VERSION';
if (srv.packages) {
  for (const pkg of srv.packages) {
    pkg.version = '$VERSION';
  }
}
fs.writeFileSync('$SERVER_JSON', JSON.stringify(srv, null, 2) + '\n');
"
  echo "Synced version $VERSION → server.json"
fi

# Update platform package.json files
for PLAT_PKG in "$REPO_ROOT"/platforms/*/package.json; do
  if [ -f "$PLAT_PKG" ]; then
    node -e "
const fs = require('fs');
const pkg = JSON.parse(fs.readFileSync('$PLAT_PKG', 'utf8'));
pkg.version = '$VERSION';
fs.writeFileSync('$PLAT_PKG', JSON.stringify(pkg, null, 2) + '\n');
"
    echo "Synced version $VERSION → $PLAT_PKG"
  fi
done

echo "Synced version $VERSION → npm/package.json"
