Context usage now includes an input breakdown by prompt, tools and history, with loaded tools counted correctly. Tool arguments tolerate provider string/item-wrapper encodings without changing stored model history. Completed background commands free capacity after their first result is delivered; compaction counts remain visible after a chat unloads.

Tightens workspace lint and ownership rules, shares immutable model/tool data, and trims repeated test setup while preserving rollback, cancellation and file-access checks.
