#!/usr/bin/env bash
# Shared SSH helpers for hermes (box 5). Source from deploy scripts; do not execute directly.
# shellcheck shell=bash

if [[ -z "${REPO_ROOT:-}" ]]; then
  _hermes_ssh_self="${BASH_SOURCE[0]:-$0}"
  _hermes_ssh_dir="$(cd "$(dirname "$_hermes_ssh_self")" && pwd)"
  REPO_ROOT="$(cd "${_hermes_ssh_dir}/.." && pwd)"
fi

# shellcheck source=/dev/null
source "${REPO_ROOT}/scripts/ssh-vds-host.sh"

HERMES_DIR="${HERMES_DIR:-${REPO_ROOT}/5 - hermes}"

source_hermes_env() {
  if [[ -f "${HERMES_DIR}/.env" ]]; then
    set -a
    # shellcheck source=/dev/null
    source "${HERMES_DIR}/.env"
    set +a
  fi
}

hermes_ssh_init() {
  source_hermes_env
  HERMES_SSH_TARGET="$(vds_ssh_connect_host "${REMOTE_HOST_IP:-132.243.213.9}")"
  HERMES_SSH_USER="${REMOTE_USER:-root}"
  HERMES_SSH_KEY="${SSH_KEY:-$HOME/.ssh/id_ed25519}"
  HERMES_SSH_OPTS=(-i "$HERMES_SSH_KEY" -o BatchMode=yes -o ConnectTimeout=15)
  HERMES_SSH_DEST="${HERMES_SSH_USER}@${HERMES_SSH_TARGET}"
}

hermes_ssh() {
  ssh "${HERMES_SSH_OPTS[@]}" "$HERMES_SSH_DEST" "$@"
}

hermes_scp() {
  local src="$1"
  local dst="$2"
  scp "${HERMES_SSH_OPTS[@]}" "$src" "${HERMES_SSH_DEST}:$dst"
}
