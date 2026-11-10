# möbius for macOS 0.2.8

- Chats connect and reconnect faster. A reconnect picks an open chat up where it left
  off instead of replaying it, and thumbnails, previews, downloads and uploads share one
  connection instead of opening a new one each, so image-heavy chats no longer run into
  the connection limit. Transfers keep several pieces in flight.
- The chat list updates with only the chats that changed, instead of the whole list
  after every turn.
- The changes counter beside the message field shows Git's own line counts, exact for
  large changes too. Diff blocks in a chat show the file they change.
- Right-click any file (a side tab, a file row, a file card or Chat files) and choose
  Download to save it to Downloads.
- Chat files, and @ in the message field, include the images the Bot looked at.
- A Bot's open chats can find and message each other.

Requires Apple Silicon and macOS 26 or later, and gateway 0.15.54: update the gateway
together with this release.
