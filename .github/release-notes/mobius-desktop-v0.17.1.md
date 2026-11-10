# möbius for macOS 0.17.1

möbius for macOS is now open source under the Apache 2.0 license, in the möbius
repository beside the framework, gateway and terminal client. Its version follows theirs
from this release on, so it jumps from 0.10.33 to 0.17.1.

Previews send their header once, streamed replies arrive once per frame, and preferences
and browser pages fail safely instead of reusing state after an internal error.

Bundles gateway 0.17.1 and requires protocol-100 gateways; older gateways cannot
connect. Read the gateway's upgrade notes before replacing an existing gateway.

Release verification now reuses successful main CI for the exact source commit. macOS audio conversion compiles with the supported SDK concurrency annotations; sandbox tests use the intended Apple Git and deterministic command completion.
