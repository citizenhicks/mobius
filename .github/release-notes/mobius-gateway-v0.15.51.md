# möbius Gateway 0.15.51

Authenticated controllers can observe machine-wide runtime activity, including hidden routine sessions, checkpointed work, background commands and active child agents. The activity revision changes for work and native client connections, so short tasks completed between polls remain observable. Dashboard observers do not count as activity.

Controllers can conditionally prepare an idle shutdown against an observed revision. Successful preparation closes admission for session work, routine reservations and native connections until cancellation or process exit; dashboard controls remain available. Runtime activity and successful preparation report the next scheduled routine deadline for external wake scheduling.

`serve --start-quiesced` holds that admission gate before the first routine poll. Controllers can verify a staged gateway through dashboard authentication, then activate it with the existing cancellation operation.

These are provider-neutral operations. The gateway does not choose an idle timeout, stop virtual machines or schedule external wakeups. Missed cron minutes retain the existing skip behavior; controllers should wake before the reported deadline.

Protocol 85 is unchanged: the new operations are opt-in requests with targeted responses. Configuration, checkpoint and database schemas are unchanged. Packages retain LICENSE and NOTICE.
