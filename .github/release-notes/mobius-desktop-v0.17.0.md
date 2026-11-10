# möbius for macOS 0.17.0

möbius for macOS is now open source under the Apache 2.0 license, in the möbius
repository beside the framework, gateway and terminal client. Its version follows theirs
from this release on, so it jumps from 0.10.33 to 0.17.0.

Previews send their header once, streamed replies arrive once per frame, and preferences
and browser pages fail safely instead of reusing state after an internal error.

Bundles gateway 0.17.0 and requires protocol-100 gateways; older gateways cannot
connect. Read the gateway's upgrade notes before replacing an existing gateway.
