Bundles möbius Gateway 0.15.34 and möbius 0.15.34, fixing false attachment-workspace changes on macOS after a reboot.

Wire protocol 82 and stored gateway formats other than the macOS attachment-workspace marker are unchanged. Rewrite existing macOS markers with `volume_uuid` before upgrading; Linux gateways need no state conversion.
