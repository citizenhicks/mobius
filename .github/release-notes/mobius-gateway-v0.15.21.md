Group chats replace Swarms. Create a chat in one workspace with multiple Bots; only mentioned members run, and their replies appear in the shared conversation. Group history has a dedicated SQLite journal and durable bounded delivery queue, with no placeholder Agent checkpoint.

Chat startup and cleanup release gateway locks before slow preparation or workspace access. Preparation detects concurrent Bot, credential, provider, and extension changes. Delivery preserves completed replies under queue saturation and avoids an actor lock cycle.

Wire protocol 77 requires matching clients and cloud event readers. Mac bundles are Developer ID signed and notarized. Back up gateway state before upgrading; existing ordinary conversation checkpoints are preserved.
