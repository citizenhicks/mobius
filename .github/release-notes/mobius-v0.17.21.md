# möbius 0.17.21

During context compaction, Möbius requests the checkpoint tool explicitly on supported providers. If the model skips it or returns unusable notes, Möbius retries once. A failed checkpoint preserves the conversation and pending input.

This release uses gateway protocol 106.
