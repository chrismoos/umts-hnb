# umts-hnb

[![CI](https://github.com/chrismoos/umts-hnb/actions/workflows/ci.yml/badge.svg)](https://github.com/chrismoos/umts-hnb/actions/workflows/ci.yml)
[![Rust 1.96](.github/badges/rust.svg)](rust-toolchain.toml)
[![License: AGPL-3.0](.github/badges/license.svg)](LICENSE)

A UMTS (WCDMA/FDD) Home NodeB for software-defined radios. It connects to an
Osmocom core network over Iuh and provides voice, SMS and packet data to
standard 3G phones, including HSPA+ and dual-carrier HSPA.

Transmit only on frequencies and equipment the site is authorized to use.

| | |
| --- | --- |
| Services | Rel99, HSDPA, HSUPA, HSPA+, DC-HSPA, voice, SMS, packet data, multiple UEs |
| Radios | Ettus B200/B210 and PlutoSky R1, see [radios](docs/radios.md). bladeRF and SoapySDR backends are also available. |
| Hosts | Ubuntu 24.04 and 26.04 packages, or build on Ubuntu 26.04 and macOS |
| Core | Osmocom HNB-GW, MSC, SGSN, GGSN, HLR, MGW and STP |

Set up the Osmocom core first, as described in
[core setup](docs/core-setup.md). Then install the
[Ubuntu packages](#ubuntu-packages) or [build from source](#building-from-source).

## Ubuntu packages

### Install

Packages for Ubuntu 24.04 and 26.04 on x86_64:

```sh
sudo curl -fsSLo /usr/share/keyrings/1xbts-archive-keyring.gpg \
  https://apt.1xbts.org/1xbts-archive-keyring.gpg
echo "deb [signed-by=/usr/share/keyrings/1xbts-archive-keyring.gpg] https://apt.1xbts.org $(. /etc/os-release && echo $VERSION_CODENAME) main" \
  | sudo tee /etc/apt/sources.list.d/1xbts.list
sudo apt-get update
sudo apt-get install umts-hnb 1xbts-archive-keyring
sudo uhd_images_downloader -t b2xx
```

The archive key fingerprint is
`6477 3ECB A18C 5860 297A  D3B2 2FCC 3B77 6B2C D08E`.

### Configure

Edit `/etc/umts-hnb/site.yaml`, as described in [settings](#settings). Its
settings override `/usr/share/umts-hnb/defaults.yaml`.

### Run

The `umts-hnb` systemd service is not enabled on install.

```sh
sudo systemctl enable --now umts-hnb
journalctl -u umts-hnb -f
sudo umts-hnbctl status
sudo umts-hnbctl ue list
```

The service runs as the `umts-hnb` user with real-time scheduling and keeps
runtime state in `/var/lib/umts-hnb`.

## Building from source

### Build on Ubuntu

```sh
sudo apt-get update
sudo apt-get install --no-install-recommends \
  build-essential git pkg-config libclang-dev \
  libuhd-dev uhd-host libsctp-dev protobuf-compiler rustup
sudo uhd_images_downloader -t b2xx
cargo build --release -p umts-hnb -p umts-management \
  --features umts-hnb/uhd
```

### Build on macOS

```sh
brew install pkg-config uhd libusrsctp protobuf
uhd_images_downloader -t b2xx
BINDGEN_EXTRA_CLANG_ARGS="-I$(brew --prefix)/include" \
  cargo build --release -p umts-hnb -p umts-management \
  --features umts-hnb/uhd,umts-hnb/usrsctp
```

macOS has no kernel SCTP, so the HNB uses userspace SCTP and tunnels it over
UDP to the core. The core must accept SCTP over UDP, as described in
[core setup](docs/core-setup.md#hnb-on-another-machine).

For a bladeRF or SoapySDR radio, replace `uhd` with `bladerf` or `soapy`.

### Configure

Copy the example configuration and edit `site.local.yaml`, as described in
[settings](#settings). Its settings override `config/defaults.yaml`.

```sh
cp config/example.yaml site.local.yaml
```

On macOS, also set:

```yaml
core:
  hnbgw:
    transport: usrsctp
    udp_encapsulation_port: 9899
```

### Run

```sh
sudo target/release/umts-hnb \
  --defaults config/defaults.yaml --config site.local.yaml --serve
```

Logs are written to the terminal. Press Ctrl+C to stop the HNB. Runtime state
is kept in `.runtime/`.

In another terminal, from the same directory:

```sh
sudo target/release/umts-hnbctl status
sudo target/release/umts-hnbctl ue list
```

## Settings

- `cell`: PLMN, LAC, RAC, SAC and cell ID. These must match the core.
- `core`: HNB-GW address and the local GTP-U address.
- `radio`: channel numbers (UARFCN), antennas and gains. Settings for each
  radio are in [radios](docs/radios.md).

With the core on the same machine, the example `core` settings work
unchanged.

For dual-carrier HSPA, change these settings:

| Setting | Change |
| --- | --- |
| `cell.packet_modes` | Add `dc-hspa`. |
| `cell.secondary_carrier.uarfcn_dl` | Set the second downlink channel, 25 UARFCNs (5 MHz) from the first. |
| `radio.rx_delay_samples` | On a B200/B210, use `78.0` instead of the single-carrier `95.0`. Other radios need their own timing calibration. |

Single-carrier modes default to 7.68 MS/s TX for one 5 MHz channel. DC-HSPA
automatically selects 15.36 MS/s TX for two 5 MHz channels, with centers
5 MHz apart and a combined 10 MHz span. RX remains at 7.68 MS/s. TX analog
bandwidth adjusts automatically unless explicitly set.

An inactive handset with a packet session can be parked: it keeps its
connection and its packet session, stops transmitting, and listens for paging
only. A handset is parked after 20 seconds without packets by default. These settings control it:

| Setting | Meaning |
| --- | --- |
| `rrc.dormancy.after_seconds` | Seconds without packets before a handset is parked, `20` by default. `0` disables parking. A value not shorter than `rrc.inactivity_release_seconds` has no effect, because the handset is released first. |
| `rrc.dormancy.state` | `cell_pch` or `ura_pch`. With `ura_pch` the cell broadcasts `rrc.ura_identity` in SIB2. |
| `rrc.dormancy.drx_cycle_length_coefficient` | Paging cycle of a parked handset: `3` = 80 ms, `4` = 160 ms, `5` = 320 ms, `6` = 640 ms, `7` = 1.28 s, `8` = 2.56 s, `9` = 5.12 s. A longer cycle saves handset battery and delays downlink data and incoming calls. A handset attached for calls uses at most the `rrc.cn_drx_cycle_length_coefficient` cycle (the idle-mode paging cycle the core network uses for calls, same values). |
| `rrc.dormancy.fast_dormancy_inhibit_seconds` | Minimum time between two requests from a handset to end its data session (T323): `0`, `5`, `10`, `20`, `30`, `60`, `90` or `120`. `null` selects `5`. Broadcast only while parking is enabled. |


A parked handset has no dedicated channel and transmits nothing. It keeps its RRC connection, its
security context, its packet bearers and its core connections, and listens for paging on its DRX cycle.

- A handset is parked when it has had no packets and no NAS messages for `after_seconds`, has no
  voice call and no circuit-switched connection, and holds either packet bearers only or no bearer.
- It returns to a dedicated channel when it has uplink data, when downlink packets arrive for it, or
  when the core pages it. HSDPA and HSUPA are activated again after the return. No message is sent
  to the core.
- Returning takes about 4 seconds with HSDPA, most of it preparing the HSDPA scheduling resources.
  The uplink packet that triggers the return can be lost and is then retransmitted by its sender.
- `rrc.inactivity_release_seconds` still releases the connection of a handset that stays parked
  without traffic. The handset is told by paging and returns to idle mode.
- A parked handset that sends no periodical update for 30 minutes, or answers none of eight pages,
  is released as lost.
- User data is not carried on FACH and RACH.

`rrc.inactivity_release_seconds` still releases a handset that stays inactive,
parked or not.

If a phone attaches but has no data, check
[packet routing](docs/core-setup.md#5-set-up-packet-routing).

## Development

```sh
bash scripts/check.sh
```

Runs formatting, Clippy and the workspace tests.

## License

AGPL-3.0-or-later. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
