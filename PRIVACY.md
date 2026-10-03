# möbius Privacy Policy

Effective August 16, 2026.

möbius connects to local, self-hosted, or möbius-hosted AI gateways. The app
developer does not sell your personal data or use it for advertising.

The app stores gateway addresses, preferences, pairing credentials, transcript
caches, and drafts on your device. Pairing credentials are stored in the Apple
Keychain. When you use the app, the content and commands you choose to send are
transmitted to the gateway you configure. That gateway may store conversations,
attachments, tool activity, schedules, credentials, and usage records, and may
send relevant content to the model providers and services you configure. Their
retention and privacy policies apply.

A local or self-hosted gateway sends no telemetry unless its operator configures an
endpoint. A möbius-hosted gateway may have a service endpoint configured by the
host. Each enabled endpoint always receives envelope fields including the gateway
instance ID, software and protocol versions, send time, sequence, uptime, delivery
reason, and configured static labels. It also receives only the snapshot sections
and event kinds selected for that endpoint. Sections may contain activity, usage,
run, or storage totals; event kinds may contain committed Bot hook facts. The
endpoint’s operator controls retention. Credentials used for delivery are not sent
to paired clients in telemetry reports.

Apple may provide TestFlight or App Store diagnostics according to your device
and Apple privacy settings. You can remove locally stored möbius data by
removing gateways in the app or deleting the app.

For privacy questions, open an issue in the
[möbius repository](https://github.com/citizenhicks/mobius/issues).
