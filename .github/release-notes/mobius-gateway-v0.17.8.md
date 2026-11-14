# mobius-gateway 0.17.8

- Use protocol **102** for committed live execution statistics, with terminal totals reconciled against real tool results and transcript history. [cc27b053](https://github.com/citizenhicks/mobius/commit/cc27b053), [d4135fd9](https://github.com/citizenhicks/mobius/commit/d4135fd9)
- Keep one transcript row when streamed final-answer text completes as commentary; count nested tool invocations and avoid repeated full-history scans. [259e4b44](https://github.com/citizenhicks/mobius/commit/259e4b44), [796dccee](https://github.com/citizenhicks/mobius/commit/796dccee)
- Preserve committed saves when delivery of their receipt cannot continue, and recover routine editors after a lost save reply. [80711c75](https://github.com/citizenhicks/mobius/commit/80711c75), [d75302e2](https://github.com/citizenhicks/mobius/commit/d75302e2)
- Bound activity reports and report only chats a Live Activity can display. [1ced754c](https://github.com/citizenhicks/mobius/commit/1ced754c), [d1b62f11](https://github.com/citizenhicks/mobius/commit/d1b62f11)
- Drop unavailable inherited model choices when creating a Bot, while validating explicit updates against the current catalog. [347dabe9](https://github.com/citizenhicks/mobius/commit/347dabe9)
- Check populated calculation source fields and the basis and impact of material assumption changes. [08019d6b](https://github.com/citizenhicks/mobius/commit/08019d6b)
- Give gateway lifecycle, state, activity, validation, publication and rejection their owning modules without changing validation or error behavior. [0f9630e7](https://github.com/citizenhicks/mobius/commit/0f9630e7)

Configuration **30**, Bot catalog **10** and checkpoint **21** remain unchanged.
No gateway-state conversion is required from 0.17.7. Protocol 101 clients are
incompatible: make protocol-102 clients available before switching remote gateways,
and preserve verified backups during the coordinated upgrade.
