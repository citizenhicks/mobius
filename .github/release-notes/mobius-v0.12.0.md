## Highlights

- Adds GPT-6 Astra to the shared OpenAI and Codex model catalog, with verified pricing and reasoning options. Custom Responses and OpenRouter routes remain supported.
- Gives capability owners control of tool headings, compaction projection cleanup, attachment fork policy, and management command parsing.
- Removes duplicate prompt-cache types and loaded-tool reconstruction; shares checked execution-stat aggregation.
- Defines consistent transcript Append behavior and capability-owned list actions and editor metadata.

## Breaking changes

- Frontend action lists require an `actions` collection; frontend actions can carry optional editor metadata.
- `Model::prompt_cache_capability` returns the existing protocol `PromptCacheMode`.
- Gateway and frontend consumers must use the matching 0.12.0/protocol 67 release.
