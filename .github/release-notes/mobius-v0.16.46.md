# mobius 0.16.46

Fixes idle compaction promotion: when the first accepted message turns maintenance into real work, the runtime durably emits a normal start for the same turn before delivering that message. Rejected messages leave maintenance hidden. This restores voice handoff completion and the normal turn-start lifecycle without changing protocol 97 or stored record formats. History pagination keeps maintenance and promotion starts sharing one ID on the same logical turn page.
