# mobius-gateway 0.17.3

Approval history keeps the original request alongside the decision. Requests
closed without a decision remain visible in the transcript.

Use protocol **101** clients before replacing a gateway serving protocol **100**
clients. Configuration version **30**, checkpoint format **21**, and SQLite
schema **14** are unchanged from 0.17.2. The 0.17.2 to 0.17.3 upgrade requires
no state conversion. Back up gateway state before upgrading. An older gateway
cannot read newly journaled approval decision events.

For 0.16.50 gateways, follow the
[offline 0.17 upgrade instructions](https://github.com/citizenhicks/mobius/releases/tag/mobius-gateway-v0.17.1)
first.
