# mobius-gateway 0.17.4

Session-event telemetry collectors now receive bounded session activity snapshots, including approval and question resolution. Changed activity is attached to pending events in the same request, with a standalone snapshot when no events are pending. Unchanged activity does not add requests.

Collectors subscribed to `session_turn_started` receive this additional session metadata through the existing authenticated telemetry transport.

Protocol **101**, configuration version **30**, checkpoint format **21**, and SQLite schema **14** are unchanged. Upgrading from 0.17.3 requires no state conversion. For older gateways, follow the [0.17.3 upgrade notes](https://github.com/citizenhicks/mobius/releases/tag/mobius-gateway-v0.17.3).
