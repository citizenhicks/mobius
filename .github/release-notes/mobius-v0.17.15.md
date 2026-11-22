# mobius 0.17.15

## Agent tools and middleware

- Tools can return a deferred response. The agent can continue working and receive the outcome for that invocation later. Answering or skipping an `ask_user` question now delivers an explicit result.
- The optional Clock middleware adds a time note when due and keeps it in the conversation for prompt caching. Existing Bots can enable it in their capabilities.
- Authors of custom tools should return `ToolResponse::Completed` or `ToolResponse::Deferred` from the tool interface.
