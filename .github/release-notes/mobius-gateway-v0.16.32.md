The gateway adds native Mistral configuration with Large 4 and Small 4 presets, both defaulting to high reasoning. Anthropic defaults include Haiku 5.5 and all five reasoning levels; its tool schema serialization avoids the rejected top-level combinators.

The framework's shared Responses and Chat Completions transports retain cancellation, continuation, image replay and credential boundaries. OpenRouter advertises its own provider symbol for supporting clients.

Protocol 92, config version 28 and checkpoint formats are unchanged from 0.16.31. State already upgraded to 0.16.28 or newer needs no additional conversion. Existing saved provider catalogs remain as configured. Binary archives include the pinned cloudflared runtime, manuals and licenses.
