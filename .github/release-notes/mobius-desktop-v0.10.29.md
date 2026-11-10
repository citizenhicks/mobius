# möbius for macOS 0.10.29

Long conversations avoid rebuilding their row layout for each streamed text update. Preview and routine transcripts now use the same virtualized list as chats, and loading earlier history respects a newer scroll or Return to latest action.

Preference changes save in order off the interface thread and finish saving when the window closes or the app quits. Attachment uploads overlap file reads with network writes while preserving cancellation and delivery checks.

Cloud sign-in can load the new account while an older sign-in credential is being revoked. Shared opacity values and quieter chat pills keep the interface consistent.

Bundles gateway 0.16.47. Protocol 97 and stored-state formats are unchanged.
