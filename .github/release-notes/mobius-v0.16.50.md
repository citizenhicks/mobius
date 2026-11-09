# mobius 0.16.50

Memory now reports changed pages rather than repeating the full index. It also notices direct page edits even when their timestamp is unchanged.

Existing legacy SQLite memories are retained but are not automatically migrated to Markdown pages.
