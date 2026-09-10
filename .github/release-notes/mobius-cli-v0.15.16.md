# möbius CLI 0.15.16

Bot configuration changes save without waiting for resident chats. Existing and queued work finishes with its current configuration; subsequent work uses the updated Bot. Attachment uploads check the current gateway capability instead of a stale local flag.

Bundles gateway 0.15.16 and uses unchanged protocol 75. Existing conversation storage requires no conversion. The separately installed Mac companion is now Developer ID signed and notarized.
