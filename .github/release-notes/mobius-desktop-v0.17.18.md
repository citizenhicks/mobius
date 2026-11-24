# möbius for macOS 0.17.18

Storage cleanup keeps a multi-chat export running if a second folder picker finishes during the transfer. Each selected chat gets its own numbered folder. Changing the selection after a successful export requires a fresh export before purging.

Requires macOS 26 or later on Apple Silicon. Local chats use the bundled 0.17.18 gateway. Remote gateways must use protocol 106.
