CLI and bundled gateway advance together to 0.16.29.

- Setup trusts the gateway's provider and middleware catalogs instead of repeating backend validation. Incomplete credentials still block the appropriate setup step, and routes without usable credentials are omitted from model selection.
- Clipboard uploads use the gateway's advertised limits and upload admission instead of duplicate hard-coded client caps and policy checks. Local file consistency and transfer correlation checks remain.
- The bundled gateway removes repeated submission, usage-save, credential, computer and routine validation work while preserving ingress and live authorization checks.

Protocol 92 and config version 28 are unchanged from 0.16.28. Existing 0.16.27 configurations still require the [offline upgrade](https://github.com/citizenhicks/mobius/blob/mobius-cli-v0.16.29/scripts/README-portable-upgrade.md). No new migration is needed from 0.16.28.
