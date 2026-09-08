Exposes provider subscription usage and actionable provider errors in profile snapshots. Profile requests run alongside connection traffic, with one active request and one latest queued request, so remote usage reads do not block chat events or controls.

Wire protocol advances from 71 to 72; update gateways and clients together. Gateway configuration 24, chat metadata 15, and checkpoint format 13 are unchanged. No state conversion is required from gateway 0.15.4. Binary packages retain cloudflared, LICENSE, and NOTICE.
