Custom OpenRouter and Anthropic API roots retain native tool discovery. Anthropic Sonnet 5.5 now advertises native deferred tools, matching its API support.

DeepSeek no longer advertises unsupported image generation, realtime voice, or hosted web search. Saved model settings remain structurally valid when a hosted tool is retired; provider construction rejects an unsupported selection with a clear error before making a request.

Protocol version remains 91.
