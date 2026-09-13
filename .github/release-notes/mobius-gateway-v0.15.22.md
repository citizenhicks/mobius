Public chats now preserve one shared journal while every participating Bot runs in a private checkpoint. Single-Bot and group conversations share the same durable chat model, routing, attachments, approvals, and lifecycle.

This release requires an offline state conversion from 0.15.21. It upgrades gateway config 24 to 25, Bot SQLite 1/state 4 to SQLite 2/state 5, checkpoint payload 14 to 15, chat SQLite 1 to 2, and wire protocol 77 to 79. Back up the complete state directory and upgrade clients and cloud event readers together.
