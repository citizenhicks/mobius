# mobius 0.17.12

- Fix tool-call replay when a provider omits a call ID. A later tool result can no longer complete the wrong call.
- Use the same rules to apply streamed transcript updates and count changed lines in multi-file diffs. Live output and reopened history now agree on those changes.
- Apply the same write-size limit to sandbox file mutations, and use bounded reads for provider responses and Git credential-helper output.
- Consolidate session catalog matching, checkpoint reads and provider response parsing to reduce duplicate validation and allocation.

Upgrading from 0.17.11 preserves protocol **104** and the existing checkpoint format.
