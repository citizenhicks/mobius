# möbius Gateway 0.15.11

Voice calls use the selected Bot's identity and system prompt. The gateway reports its own release version in the authenticated catalog so dashboards can identify the running server independently of the CLI version.

The macOS voice menu gains corner pinning, a compact circular Mini mode, and saved global keyboard shortcuts for start/stop and mute/unmute. Controls appear on hover or keyboard navigation; VoiceOver, approvals, and errors keep them accessible. Chat selection groups conversations by project folder. Saved shortcuts register at application launch.

Protocol 74 and checkpoint format 14 are unchanged; upgrading from 0.15.10 requires no conversation conversion. Upgrade running gateways before using CLI 0.15.11, which expects the new gateway version metadata.

Validation: Rust workspace checks and macOS formatting, complexity, media/transport, pinning, Mini mode, and shortcut tests. Packages preserve LICENSE and NOTICE.
