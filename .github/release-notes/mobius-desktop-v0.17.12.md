# mobius-desktop 0.17.12

- Clean up interrupted attachment uploads and enforce the preview size limit on the bytes actually read, even if the file grows after selection.
- Apply the same transcript updates to live and reopened chats, and use the same line counts as the terminal client for multi-file diffs.
- Open transcript document links through a single accessible control, and keep file previews associated with the selected chat when its snapshot refreshes.

macOS build **61** bundles gateway **0.17.12** and uses protocol **104**. It can also connect to a 0.17.11 gateway.
