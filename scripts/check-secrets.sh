#!/usr/bin/env bash
set -euo pipefail

patterns='(rgk_[A-Za-z0-9_-]{20,}|gh[ps]_[A-Za-z0-9]{20,}|-----BEGIN (RSA |EC |OPENSSH )?PRIVATE KEY-----)'
if git grep -nEI "${patterns}" -- . ':(exclude)openapi.current.json'; then
  echo "Potential credential material found in tracked files" >&2
  exit 1
fi
