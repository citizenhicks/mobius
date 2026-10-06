Use mobius 0.16.24 for checkpointed once-only guidance, bounded model retries, and endpoint-aware media capabilities. Remove repeated computer-runtime restart notices from new context.

Restore model-only provider registration: --model X without --model-id replaces the configurable provider's text-model list with X, subject to existing Bot and default-model safety checks. Explicit model-ID lists remain authoritative.

Validate voice routes when creating a Bot or changing its voice selection. Existing invalid or bare voice selections remain readable and allow unrelated Bot edits, but a voice call reports an error until a valid route is selected instead of silently choosing another voice. Generic custom Responses endpoints do not automatically advertise native media catalogs; explicitly configured image IDs and saved selections remain usable. Protocol version remains 91.
