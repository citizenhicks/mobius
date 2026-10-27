# mobius 0.16.44

Adds frontend-neutral transcript presentation and scoped durable memory. Memories belong to a Bot in a project unless explicitly saved globally; approved writes and per-chat memory settings share the gateway-owned store.

Checkpoint 21 and SQLite schema 14 are strict. Existing checkpoint 20/schema 13 state and scratchpad records require explicit offline record conversion before the new runtime opens them. No runtime migration, fallback, aliases or automatic state reset is provided. Preserve verified backups before conversion or rollback.
