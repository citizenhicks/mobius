# mobius-desktop 0.17.10

Bundle gateway **0.17.10** and use protocol **103**. A matching protocol-103 gateway is required; protocol 102 gateways and clients are incompatible. Configuration 30 and checkpoint 21 are unchanged, so gateway state from 0.17.8 needs no conversion. Coordinate remote gateway and client upgrades.

- Show each output file's type and size under its name and its modified time on the right, in the transcript footer's format. [af4f3f90](https://github.com/citizenhicks/mobius/commit/af4f3f90)
- Keep Dictate beside the send button, and start voice from the send button while the draft is empty. [af4f3f90](https://github.com/citizenhicks/mobius/commit/af4f3f90)
- Render command results as the command's output, non-zero exit code or background state, and stream a running command's newest output under its call. [ee6db260](https://github.com/citizenhicks/mobius/commit/ee6db260), [9f13ecdf](https://github.com/citizenhicks/mobius/commit/9f13ecdf)
- Name approval requests by the tool's title or lone argument instead of argument JSON, and show an asked question as waiting for you. [f68ffda5](https://github.com/citizenhicks/mobius/commit/f68ffda5), [ea8553a0](https://github.com/citizenhicks/mobius/commit/ea8553a0)
- Apply Bot setting changes to idle chats at once. [a99d1815](https://github.com/citizenhicks/mobius/commit/a99d1815)
- Release quiet chats from the bundled gateway's memory and reload them on the next message. [312f40d5](https://github.com/citizenhicks/mobius/commit/312f40d5)
- Cache memory pages between model calls and let memory reads run together. [188001cf](https://github.com/citizenhicks/mobius/commit/188001cf), [67a2414c](https://github.com/citizenhicks/mobius/commit/67a2414c)
