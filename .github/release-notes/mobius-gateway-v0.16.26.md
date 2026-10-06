Uses mobius 0.16.26 with reasoned cancellation and corrected background-command activity. An uncollected completed command no longer keeps an idle gateway's activity hook running.

Turn-aborted logs and collector telemetry preserve known stop reasons while redacting arbitrary provider or middleware text. Private journal-sequence metadata ties each cancellation to its exact turn without depending on event-ID formatting or changing the client wire. Gateway runtime diagnostics use consistent UTC millisecond timestamps.

Protocol version remains 91.
