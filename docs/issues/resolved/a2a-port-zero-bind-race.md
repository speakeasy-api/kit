# A2A ephemeral port selection had a bind race

Kit previously bound `127.0.0.1:0` to discover an ephemeral port, built the Agent Card with that address, dropped the reservation, and asked `a2a-protocol-server` to bind the numeric address again. Another process could claim the port in that gap, causing normal startup to fail with `AddrInUse` or advertise a port Kit did not own.

## Resolution

Kit now binds one listener, builds the Agent Card from that listener's actual address, and moves the same listener into its HTTP accept loop. The selected port remains continuously owned. A regression test verifies a second bind fails with `AddrInUse` after startup returns.
