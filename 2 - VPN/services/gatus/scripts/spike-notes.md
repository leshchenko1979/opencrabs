# Gatus → OpenCrabs bridge — spike notes (2026-05-17)

Option A. **Full plan:** `.cursor/plans/gatus_opencrabs_dm_bridge_6607474a.plan.md`

## Spike results

| ID | Result |
|----|--------|
| 0a | `http://172.18.0.1:9081/gatus/alert`, bind `0.0.0.0:9081` |
| 0b | Gatus placeholders documented; log first POST at bridge deploy |
| 0c | MCP `send_message` works (Bearer + SSE Accept) |
| 0e | Hermes needs Mac `id_ed25519` + ssh config |

## Locked (post-spike)

- Ops bot: **@redevest_admin_tools_bot** (`7357853620`, same as Gatus Telegram)
- OpenCrabs: **`ops` profile** + `default` (@oc_l1979_bot) both running
- Hermes: **`/root/vds-servers`** from **leshchenko1979/servers**; pull before `[Gatus]` + nightly ops cron
- Bridge MCP target: **@redevest_admin_tools_bot**

## Build order

Phase 1 scrub/git → Phase 2 hermes → Phase 3 bridge → Phase 4 Gatus custom → Phase 5 docs

## Post-deploy fix (2026-05-17)

- **Ops bot silent:** `keys.toml` must use `[channels.telegram] token` and `[providers.minimax] api_key` (not `[telegram] bot_token`). Profile must be registered via `opencrabs profile create ops`.

## Implementation artifacts

- `gatus_opencrabs_bridge.py` + `deploy-gatus-bridge.sh` on VPN
- `5 - hermes/opencrabs-brain/`, `deploy-opencrabs-ops-profile.sh`
- Custom alerts on `host-box2`–`host-box5` only
