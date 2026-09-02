## Highlights

- Reuses one private background conversation per Bot and Swarm while sharing the bounded Swarm Chat.
- Runs Swarm work in a gateway-owned workspace and projects each Bot's real final response into chat.
- Hardens pre-auth connection limits and pairing, and starts a new deliberate on-disk state generation.

Protocol 62, configuration 23, chat specification 14, checkpoint 13, SQLite schema 9, Bot state 4, and Swarm state v4 are strict; migrate older gateway state offline before startup.
