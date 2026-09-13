Centralize session ownership and typed frontend events so framework consumers share one validated protocol path. Model preparation avoids redundant history copies, and OpenAI fallback state is bounded.

Checkpoint payload 17 and SQLite schema 10 intentionally replace `bot_id` session context with `owner_id`. Existing checkpoint databases require the documented offline conversion before opening them with this release.
