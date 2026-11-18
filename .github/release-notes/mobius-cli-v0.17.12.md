# mobius-cli 0.17.12

- Keep the requested end of chat history when paging through older messages. Live and reopened transcripts now apply the same block updates and count multi-file diffs consistently.
- Publish attachment downloads at the destination only after the transfer completes. A failed download leaves no partial file at the requested path.
- Show recoverable session refresh errors in the transcript across terminal commands.
- Use the same navigation and confirmation behavior across terminal pickers, including destructive actions.

Bundles gateway **0.17.12** and uses protocol **104**. The CLI also works with a 0.17.11 gateway.
