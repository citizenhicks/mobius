# möbius Gateway 0.9.16

- Validates newly saved API keys as visible, whitespace-free tokens while preserving existing credential files.
- Returns only the final four credential characters so clients can identify a saved key without reading it back.
- Switches an explicitly keyed provider setup from credentialless to provider-default authentication.
- Accepts direct provider credentials from bounded, non-interactive standard input without putting keys in arguments or URLs.
- Bundles möbius 0.9.11 so provider output bursts no longer overflow the recorder command path.

This release remains on protocol 47. Configuration 20 and chat specification 9 are unchanged.
