# Changelog

## [Unreleased]

- Fewer dropped connections when calls end, packet bearers are added and handsets park.
- Added `rrc.dormancy` to park inactive packet handsets in CELL_PCH or URA_PCH after 20 s without traffic.
- Reduced HSDPA activation time and memory use per handset.
- Fewer dropped connections during calls and when a second handset joins.

## [0.1.1] - 2026-11-19

- Improve multi-UE reliability so calls, SMS and data work across several UEs, with shared uplink and on-demand code and power allocation.
- Ubuntu packages install on x86-64-v2 CPUs and select the x86-64-v3 build when the CPU supports it.
- Added `core.hnbgw.local_address` to bind Iuh to one local address.

## [0.1.0] - 2026-11-15

- Add support for SoapySDR.
- Improve PRACH detection.
- Initial release.
