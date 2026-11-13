# mobius-gateway 0.17.8

Clarify the default prompt: check that calculation categories have populated source
fields, and verify the basis and impact of material assumption changes.

Protocol **102** adds committed run statistics to existing live events.
Configuration **30** is unchanged. Protocol 101 clients cannot connect: roll out
protocol 102 clients, including the separately maintained iOS app, before upgrading
the gateway. This release does not require gateway-state conversion from 0.17.7.
