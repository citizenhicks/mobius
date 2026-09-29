# möbius Gateway 0.15.55

A newer gateway again replaces an older one already running on the same machine. 0.15.54
always sent an empty catalog hint when authenticating, so its version check could not reach
a running 0.15.53, and starting 0.15.54 failed with "gateway closed during version
authentication" instead of replacing it. An empty hint is now left out.

Pins mobius 0.15.50. The protocol version stays 86 and the wire behaviour matches 0.15.54.
Gateway configuration 26, chat specification 15, checkpoint payload 17, SQLite schema 10,
Bot state 6, and Bot storage schema 3 are unchanged; no data reset is required. Packages
retain LICENSE and NOTICE.
