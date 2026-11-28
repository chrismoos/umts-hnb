# RRC dormancy

RRC dormancy lets an inactive handset give up its dedicated radio channel
while keeping its RRC connection, security context, packet bearers and core
connections. In `CELL_PCH` or `URA_PCH`, the handset listens for paging at
its configured discontinuous reception (DRX) interval.

## When a handset enters dormancy

By default, a handset can enter dormancy after 20 seconds without packets or
NAS messages. It must have no active voice call or circuit-switched connection.
It may have packet bearers or have no bearers at all.

## Configuration

Set these options in the site configuration. See the
[README](../README.md#settings) for configuration paths and
[default settings](../config/defaults.yaml) for the shipped values.

| Setting | Default | Behavior |
| --- | --- | --- |
| `rrc.dormancy.after_seconds` | `20` | Inactivity interval before entering dormancy. Set to `0` to disable it. Use a value shorter than `rrc.inactivity_release_seconds` so dormancy can begin before the connection is released. |
| `rrc.dormancy.state` | `cell_pch` | Choose `cell_pch` or `ura_pch`. In `ura_pch`, the cell broadcasts `rrc.ura_identity` in SIB2. |
| `rrc.dormancy.drx_cycle_length_coefficient` | `6` (640 ms) | Sets the interval between paging opportunities. A handset attached for calls uses a cycle no longer than `rrc.cn_drx_cycle_length_coefficient`, which uses the same scale. |
| `rrc.dormancy.fast_dormancy_inhibit_seconds` | `null` (5 seconds when enabled) | Sets T323, the minimum interval between handset requests to enter dormancy early. Allowed values are `0`, `5`, `10`, `20`, `30`, `60`, `90` and `120`. The cell broadcasts T323 only while dormancy is enabled. |

The DRX coefficient maps to the following paging intervals.

| DRX coefficient | Paging interval |
| --- | --- |
| `3` | 80 ms |
| `4` | 160 ms |
| `5` | 320 ms |
| `6` | 640 ms |
| `7` | 1.28 s |
| `8` | 2.56 s |
| `9` | 5.12 s |

A longer paging interval can save handset battery at the cost of a longer
wait for incoming data or calls.

## Returning to a dedicated channel

Uplink data, incoming downlink packets or a page from the core can bring the
handset back to a dedicated channel. The HNB handles the transition locally
and reactivates HSDPA and HSUPA afterward. User data travels on the dedicated
channel after the transition.

With HSDPA, the transition takes about four seconds, most of it spent preparing
scheduling resources. The first uplink packet can be lost and may need to be
retransmitted by the sender.

## Connection release

The inactivity timer (`rrc.inactivity_release_seconds`) continues running
during dormancy. When it expires, the HNB pages the handset to return it to
idle mode. The HNB also releases a handset as lost after 30 minutes without a
periodic update or eight unanswered pages.
