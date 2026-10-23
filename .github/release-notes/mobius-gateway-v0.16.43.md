# mobius-gateway 0.16.43

Local clients can connect as soon as the gateway is ready, while Cloudflare assigns its public address in the background. Pairing still waits up to 30 seconds for a Quick Tunnel address, and WebSocket upgrades remain blocked until the expected hostname is known. Updates the WebSocket transport dependency to 0.30.
