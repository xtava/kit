#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
managed_dir="${HOME}/.local/bin"
install_lock="${managed_dir}/.kit-install.lock"
owns_install_lock=false
inherited_lock_owner="${KIT_INSTALL_LOCK_OWNER_PID:-}"

mkdir -p -- "$managed_dir"

reclaim_stale_install_lock() {
  if [[ ! -d "$install_lock" || -L "$install_lock" ]]; then
    return 1
  fi
  local owner_file="$install_lock/pid"
  if [[ -f "$owner_file" && ! -L "$owner_file" ]]; then
    local owner_pid
    owner_pid="$(<"$owner_file")"
    if [[ "$owner_pid" =~ ^[0-9]+$ ]]; then
      if kill -0 "$owner_pid" 2>/dev/null; then
        return 1
      fi
      rm -f -- "$owner_file"
      rmdir -- "$install_lock"
      return 0
    fi
  fi

  local modified_at
  case "$(uname -s)" in
    Darwin) modified_at="$(stat -f '%m' "$owner_file" 2>/dev/null || stat -f '%m' "$install_lock")" ;;
    Linux) modified_at="$(stat -c '%Y' "$owner_file" 2>/dev/null || stat -c '%Y' "$install_lock")" ;;
    *) return 1 ;;
  esac
  if (( $(date +%s) - modified_at < 30 )); then
    return 1
  fi
  rm -f -- "$owner_file"
  rmdir -- "$install_lock"
}

if [[ -n "$inherited_lock_owner" ]]; then
  recorded_owner="$(<"$install_lock/pid")"
  if [[ "$recorded_owner" != "$inherited_lock_owner" ]]; then
    echo "Kit install lock owner changed; refusing to race another installer" >&2
    exit 1
  fi
  printf '%s' "$$" >"$install_lock/pid"
else
  if ! mkdir -- "$install_lock" 2>/dev/null; then
    if ! reclaim_stale_install_lock || ! mkdir -- "$install_lock" 2>/dev/null; then
      echo "Another Kit install or update is already in progress" >&2
      exit 1
    fi
  fi
  chmod 700 -- "$install_lock"
  printf '%s' "$$" >"$install_lock/pid"
  chmod 600 -- "$install_lock/pid"
  owns_install_lock=true
fi

release_install_lock() {
  local recorded_owner
  recorded_owner="$(<"$install_lock/pid")"
  if [[ "$recorded_owner" != "$$" ]]; then
    return
  fi
  if [[ "$owns_install_lock" == true ]]; then
    rm -f -- "$install_lock/pid"
    rmdir -- "$install_lock"
  elif [[ -n "$inherited_lock_owner" && -d "$install_lock" ]]; then
    printf '%s' "$inherited_lock_owner" >"$install_lock/pid"
  fi
}
trap release_install_lock EXIT

cargo install --locked --force -j 2 --root "${HOME}/.local" --path "$repo_dir" &
cargo_pid=$!
printf '%s' "$cargo_pid" >"$install_lock/pid"
if ! wait "$cargo_pid"; then
  exit 1
fi
printf '%s' "$$" >"$install_lock/pid"

installed_binary="${HOME}/.local/bin/kit"
"$installed_binary" --json update __register-source "$repo_dir" >/dev/null &
registration_pid=$!
printf '%s' "$registration_pid" >"$install_lock/pid"
if ! wait "$registration_pid"; then
  exit 1
fi
printf '%s' "$$" >"$install_lock/pid"
