# möbius Gateway 0.15.16

Bot saves prepare shared model and extension resources once. They no longer wait for or rebuild resident chats. Chats retain their workspace, history, and Bot identity; accepted work finishes with its existing configuration, and the next idle execution uses the updated Bot.

Chat command handling keeps replacement and rollback futures off the actor stack, preventing stack overflow during Bot configuration binding.

Routine edits release their file locks immediately, preventing a following run from being blocked while another process starts.

The Mac app is Developer ID signed and notarized, includes the layered app icon, and places Mac control under the three-dot menu with separate Accessibility and Screen Recording permission controls. Native voice uses echo cancellation and noise suppression.

Protocol 75 and checkpoint format 14 are unchanged. Existing conversations need no conversion. Upgrade the companion app and CLI together. Packages preserve LICENSE and NOTICE.
