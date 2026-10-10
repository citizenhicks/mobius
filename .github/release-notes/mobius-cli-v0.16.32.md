CLI and bundled gateway advance together to 0.16.32.

- The gateway adds native Mistral with Large 4 and Small 4 reasoning presets.
- Anthropic defaults include Haiku 5.5 and all five reasoning levels; outgoing tool schemas avoid unsupported root combinators.
- Shared provider transports preserve native wire behavior, cancellation and continuation while borrowing request history and image data.

Protocol 92 and config version 28 are unchanged. State already upgraded to 0.16.28 or newer needs no additional conversion.
