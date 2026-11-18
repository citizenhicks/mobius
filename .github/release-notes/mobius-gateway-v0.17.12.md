# mobius-gateway 0.17.12

- Serialize gateway startup, shutdown, and configuration reset so a concurrent start cannot slip into a reset.
- Reuse stored metadata when listing and reading session files. Small metadata records now require one filesystem task per read.
- Read gateway credentials through a single owner-checked file descriptor and apply consistent size limits to credential, process, and policy files.
- Drain output and errors concurrently during remote desktop commands and bound the retained output.

Uses protocol **104**. Gateways and clients running 0.17.11 and 0.17.12 can connect in either direction. An upgrade from 0.17.11 requires no database conversion.
