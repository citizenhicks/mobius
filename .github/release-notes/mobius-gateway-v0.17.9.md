# mobius-gateway 0.17.9

- Apply Bot setting changes to idle chats at once, so widgets such as the approval policy update without waiting for the next message. [a99d1815](https://github.com/citizenhicks/mobius/commit/a99d1815)
- Add `Endpoint::is_reachable` so clients can tell when a full reconnect is worth trying. [7ff1cafb](https://github.com/citizenhicks/mobius/commit/7ff1cafb)
- Include the framework's command output, live output and approval presentation changes. [ee6db260](https://github.com/citizenhicks/mobius/commit/ee6db260), [9f13ecdf](https://github.com/citizenhicks/mobius/commit/9f13ecdf), [f68ffda5](https://github.com/citizenhicks/mobius/commit/f68ffda5)

Protocol **102**, configuration **30**, Bot catalog **10** and checkpoint **21** remain unchanged. No gateway-state conversion is required from 0.17.8, and 0.17.8 clients remain compatible.
