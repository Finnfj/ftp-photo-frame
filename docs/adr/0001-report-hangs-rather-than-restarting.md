# Report hangs rather than restarting

The frame occasionally stops changing photos while the process stays alive and every web route
keeps answering. The obvious remedy is a watchdog — systemd's `WatchdogSec`, or an in-process
timer that restarts the frame when a thread stops reporting — and the unit in `dpkg/` deliberately
does not have one. A hang that happens once a week is diagnosed from the state it leaves behind,
and restarting destroys exactly that: the thread that was stuck, what it was stuck on, and for how
long. Automatic recovery would also hide the problem well enough that nobody would look again, so
the frame is instrumented to say where it stopped (`/api/status`, `src/diagnostics.rs`) and left
stopped until somebody has looked.

## Consequences

A wedged frame stays wedged until it is restarted by hand. That is the cost, and it is accepted
only while a hang is still undiagnosed. `Restart=on-failure` is set, so this applies to a frame
that *hangs*, not one that exits — a crash is already visible and is restarted.

Revisit once a hang has been diagnosed. At that point the trade reverses: the evidence has served
its purpose, and availability is worth more than another sample of a bug that is understood.
