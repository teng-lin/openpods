# Wear orientation evidence and limits

AACP reports primary and secondary wear states. Those names describe roles,
not physical left and right ears. The upstream
[AAP definitions](https://github.com/librepods-org/librepods/blob/89356c08cea3db0b6b54792d9329d09a87cc85fa/docs/AAP%20Definitions.md#ear-detection)
document a microphone/primary role switch after removal and another wear
notification. The preceding battery packet's first pod therefore does not prove
the absolute orientation of a later wear report. No captured ordering guarantee
or physical one-bud trace has established that correlation for this port.

The reducer retains AACP battery-primary context separately, but publishes
`wear_primary_side: null` for AACP wear. Native primary/secondary states and ear
policy remain available. Legacy left/right `in_ear_known` is false for an
asymmetric report with unproved orientation; `in_ear` remains false for old
boolean-only consumers. If both known wear states agree, their absolute in-ear
booleans are known without identifying the roles. Verified BLE advertisements
carry an explicit primary bit and retain that source's orientation. New battery
updates never retroactively relabel an earlier wear report.

Physical acceptance remains pending: record sanitized packet ordering and exact
left/right removals, microphone role switches, reinsertion and case placement
on the selected model/firmware. Automated mixed-source and role-switch ambiguity
regressions establish conservative publication, not hardware correctness. Any
future AACP absolute mapping needs that evidence before changing this policy.
