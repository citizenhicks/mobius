# möbius CLI 0.15.11

Live chat and Ctrl+T share improved Markdown rendering: paragraphs reflow, wrapped lists and quotes keep their indentation, task checkboxes render, code preserves blank lines and uses syntax colors, and tables align or stack on narrow terminals.

Bots now expose **Identity & system prompt** for editing names, descriptions, and multiline prompts. Ctrl+U clears the selected field, Shift+Enter inserts a newline, and Ctrl+S saves. Updates retain the original configuration revision to reject concurrent edits instead of overwriting them.

The CLI header shows its build version; the gateway dashboard shows the connected gateway's reported version. Upgrade gateways to 0.15.11 before connecting this CLI. The bundle includes the matching gateway and retains LICENSE and NOTICE.

Validation: all required Rust workspace checks, terminal rendering at narrow and wide widths, Ctrl+T, scrolling/resizing, and Bot editing tests.
