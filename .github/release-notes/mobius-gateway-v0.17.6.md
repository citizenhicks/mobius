# mobius-gateway 0.17.6

Add `mobius-gateway telemetry ready`: an authenticated local readiness check that reports collector delivery status without waiting for periodic telemetry. It neither starts the gateway nor changes collector configuration.

Protocol **101** and configuration **30** are unchanged. Existing clients remain compatible. No gateway-state migration is required from 0.17.5.
