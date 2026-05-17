#!/usr/bin/env bash
# Map canonical VDS IPs to ~/.ssh/config Host aliases (vpn, apps, claw).
# Other values pass through unchanged (custom IPs, or aliases already set in .env).
# shellcheck shell=bash

vds_ssh_connect_host() {
    case "${1:-}" in
        104.128.131.166) printf '%s' vpn ;;
        144.31.188.163) printf '%s' apps ;;
        2.27.120.75) printf '%s' n8n ;;
        132.243.213.9) printf '%s' hermes ;;
        *) printf '%s' "${1:-}" ;;
    esac
}
