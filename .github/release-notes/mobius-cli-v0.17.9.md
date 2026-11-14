# mobius-cli 0.17.9

- Animate a working turn in its own activity row (spinner, current step, elapsed time and keys) at 80 ms frames, and spin running tool rows. [df95862a](https://github.com/citizenhicks/mobius/commit/df95862a)
- Show a running command's newest output under the chat until it finishes. [9f13ecdf](https://github.com/citizenhicks/mobius/commit/9f13ecdf)
- Decide approvals from a choice card: arrows and Enter, a digit picks at once, typed text denies with a reason. [4e9f6ba0](https://github.com/citizenhicks/mobius/commit/4e9f6ba0)
- Answer open questions inline with numbered choices. [427647b9](https://github.com/citizenhicks/mobius/commit/427647b9)
- Reconnect to the gateway automatically and keep unsent text in the composer. [7ff1cafb](https://github.com/citizenhicks/mobius/commit/7ff1cafb)
- Warn before discarding unsaved Bot settings; Ctrl-S applies them. [6a26f668](https://github.com/citizenhicks/mobius/commit/6a26f668)
- Print `mobius resume <id>` when closing a chat, and add the `resume` command. [300dd52b](https://github.com/citizenhicks/mobius/commit/300dd52b)
- Show a failed turn's error once, with a pointer to `/login` for provider failures. [b4ab3164](https://github.com/citizenhicks/mobius/commit/b4ab3164)
- Dithered accents on the Nord theme. [c9972f5e](https://github.com/citizenhicks/mobius/commit/c9972f5e)

Bundles gateway **0.17.9**; protocol **102** is unchanged. For library users, `FrontendExit::Reconnect` now carries the unsent draft and `frontend::run` takes an `Opening`.
