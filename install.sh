#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

cargo install --locked --force -j 2 --root "${HOME}/.local" --path "$repo_dir"
