Uses möbius 0.15.35 so active attachment workspaces on macOS survive mount-device renumbering after a reboot without weakening path-replacement checks.

Wire protocol 82, gateway configuration 26, checkpoint payload 17, and SQLite schema 10 are unchanged from 0.15.32. Existing macOS attachment-workspace markers require the one-time `volume_uuid` rewrite before upgrade; Linux cloud gateways can upgrade in place.
