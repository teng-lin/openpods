# Configuration confirmation through an AACP reopen

AirPods Pro 2 hardware testing exposed configuration values that were reported
at AACP connection setup but did not produce fresh reports after writes or a
repeated notification subscription. A zero-to-all notification mask cycle also
did not produce fresh configuration reports in that diagnostic. Qt and Android
clients update their local settings optimistically, which does not establish
hardware acceptance.

Conversational Awareness, one-bud ANC and adaptive strength therefore use an
explicit readback transaction. Each new request writes its desired value once,
even when the startup cache matches: changes made through another host may be
unobservable on the existing connection. It then releases only the owned vendor
L2CAP channel and reconnects to the same address in one designated new generation.
Fresh AACP metadata and the exact requested configuration must both be observed
there before the request is confirmed. Configuration reports arriving before
metadata are retained by the reducer and verified after identity arrives.

The original request deadline includes queueing, the write, connection setup,
handshake and readback. It never restarts. Connection, subscription-pause, send
and receive budgets are capped by the time remaining. The active request stays
owned throughout verification, and no setting mutation can be dispatched by the
readback phase. Ordinary disconnect and generation-change cancellation rules
remain in force outside this explicit transition.

A failed reopen, changed identity, missing report, expiry, suspend or cancellation
after the setting write has an unknown outcome. Recovery may restore the control
channel, but never replays that mutation. A fresh different value also cannot
confirm the requested value; it does not establish firmware normalization or an
unsupported-value mapping. Inspect a subsequent fresh status snapshot before repeating a setting request.

This process does not invoke BlueZ device connect/disconnect, change the adapter
identity, or alter audio profiles, defaults or stream routes. Listening-mode
changes retain their live same-generation confirmation path and its `AlreadySet`
optimization. The notification-mask experiment is not part of production
verification. Hardware behavior and audio continuity still need evidence for
each tested model and firmware; synthetic peers establish transaction safety.
