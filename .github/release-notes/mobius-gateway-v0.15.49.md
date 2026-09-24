# möbius Gateway 0.15.49

- Stop cloud-managed gateways at the verified subscription expiry plus five minutes, closing paired clients and running routines even when Cloud reconciliation is unavailable.
- Reject startup with a missing, invalid, or expired access lease on cloud-managed gateways. Standalone gateways do not require a lease.
- The lease bounds ordinary reconciliation outages and restarts. A Sprite user with Full access can alter the gateway runtime; adversarial revocation requires a trusted edge outside the Sprite.
- Preserve protocol 84, configuration version 26, checkpoint payload 17, Bot SQLite schema 3, and session SQLite schema 10. Existing gateway state upgrades in place.
