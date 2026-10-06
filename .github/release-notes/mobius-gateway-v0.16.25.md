Uses mobius 0.16.25 with corrected OpenRouter and Anthropic discovery settings and DeepSeek capabilities.

Media routes use the selected endpoint when checking native voice and image support. An authenticated custom Responses endpoint no longer inherits voice routes from its registered default endpoint; explicitly configured image models continue to work.

Gateways with a previously saved unsupported hosted-search selection still start without rewriting that setting. Using the selection returns a clear error; it does not prevent unrelated supported providers from running. New provider registrations validate hosted-search support.

Protocol version remains 91.
