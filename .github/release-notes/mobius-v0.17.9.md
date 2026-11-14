# mobius 0.17.9

- Render `bash` and `manage_command` results as the command's output, a non-zero exit code or its background state instead of the result envelope; a short command is no longer repeated under its own title. [ee6db260](https://github.com/citizenhicks/mobius/commit/ee6db260)
- Stream a foreground command's newest output lines as a transient transcript-tail widget while it runs, removed when the call returns. `Sandbox::run_command` accepts an output observer. [9f13ecdf](https://github.com/citizenhicks/mobius/commit/9f13ecdf)
- Name approval calls by their tool's preview title, or a lone string argument, instead of argument JSON. [f68ffda5](https://github.com/citizenhicks/mobius/commit/f68ffda5)
- Show an asked question as waiting for the reader rather than its acceptance receipt. [ea8553a0](https://github.com/citizenhicks/mobius/commit/ea8553a0)

Coordinated with gateway/CLI **0.17.9**. Protocol **102** is unchanged; the macOS app and iOS app at 0.17.8 remain compatible.
