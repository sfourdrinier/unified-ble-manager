#!/usr/bin/env bash
set -euo pipefail

# CI and publication run the same real Electron boundary; Xvfb provides the
# Linux display only. The synthetic radio smoke still fails if its addon is absent.
if [[ "$(uname -s)" == 'Linux' ]]; then
  exec xvfb-run -a ./node_modules/.bin/electron --no-sandbox scripts/ci/electron-main-smoke.js
fi
exec ./node_modules/.bin/electron scripts/ci/electron-main-smoke.js
