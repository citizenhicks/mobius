# möbius 0.15.12

Computer control supports browser actions and native Mac actions through the existing sandbox worker channel. The sandbox remains the execution and approval owner; native access requires Full access and an available desktop connection.

Worker cancellation closes native requests and stops their execution. Tests cover native reply framing, errors, cancellation, and retained browser state.
