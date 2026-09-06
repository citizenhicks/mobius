Provider sign-in can resume after a paired client reconnects. Retrying the same request returns its device code or completed result, including when another device has since started signing in. Code acquisition and polling continue independently of the client connection, and stale completions cannot disturb a newer attempt.

Protocol 70 and the existing configuration and storage formats are unchanged. Deploy this gateway patch before native clients that automatically retry interrupted sign-in requests. The framework dependency remains mobius 0.15.0.

Binary packages include pinned cloudflared, LICENSE, and NOTICE.
