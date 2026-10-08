Gateway validation now fails at the owning boundary and avoids repeated work on unchanged data.

- Usage updates no longer validate the whole configuration or reread the tunnel credential file. Arbitrary configuration saves still validate before publication; failed writes retain rollback behavior.
- Credential updates validate the changed credential once. Loading credentials from disk still validates the stored map.
- Submission checks are carried through session dispatch into agent admission without repeated content scans.
- Computer control requires Full access or Allow · no network only when the computer-control middleware is enabled. Desktop viewing follows that capability; unrelated Bots no longer need a permissive sandbox policy merely because desktop support is enabled.
- Computer runtime preparation and worker creation avoid duplicate configuration checks; routine validation uses one bounded recursive traversal.

Protocol 92, config version 28, Bot state and checkpoint formats are unchanged from 0.16.28. Upgrades from older config formats still require the documented offline conversion. Binary archives include the pinned cloudflared runtime, manuals and licenses.
