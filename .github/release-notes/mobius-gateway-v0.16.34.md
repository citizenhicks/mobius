Computer control on Linux now lists and captures individual windows on the gateway's private X11 display. Input is bound to the observed window and screenshot geometry; stale captures, unrelated windows and user takeover fail closed. Native X11 operations have a bounded deadline.

A stalled read-only observation reconnects without closing desktop apps. A timeout after possible input resets the private desktop to prevent delayed actions; this closes its apps and can lose unsaved work, while retaining the browser profile.

Desktop actions produce a fresh scoped observation. Zooming an old screenshot no longer suppresses that observation, and capture failures remain visible even after lengthy tool output. The tool guidance documents app-scoped macOS and window-scoped Linux control.

Protocol 93, config 29, Bot state 9 and checkpoint 20 remain unchanged. State already converted for 0.16.33 needs no further conversion. Binary archives include the pinned cloudflared runtime, manuals and licenses.
