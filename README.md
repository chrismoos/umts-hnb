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
| Hosts | Ubuntu 26.04, macOS |
| Core | Osmocom HNB-GW, MSC, SGSN, GGSN, HLR, MGW and STP |

## Getting started

| Step | |
| --- | --- |
| 1. [Core network](docs/core-setup.md) | Install the Osmocom core and add a USIM to the HLR. |
| 2. [Build](#build) | Compile the HNB and `umts-hnbctl` for Ubuntu or macOS. |
| 3. [Configure](#configure) | Set the cell identity, core addresses, channels and gains. |
| 4. [Run](#run) | Start the cell and check attached phones. |

## Build

### Ubuntu

```sh
sudo apt-get update
sudo apt-get install --no-install-recommends \
  build-essential git pkg-config libclang-dev \
  libuhd-dev uhd-host libsctp-dev protobuf-compiler rustup
sudo uhd_images_downloader -t b2xx
cargo build --release -p umts-hnb -p umts-management \
  --features umts-hnb/uhd
```

### macOS

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

### Debian package

The package installs both binaries, the stock configuration as
`/etc/umts-hnb/site.yaml` and a `umts-hnb` systemd service. Built for
x86-64-v3, the package installs only on CPUs with AVX2 and FMA (Intel Haswell,
AMD Zen or later).

```sh
cargo install --locked cargo-deb
RUSTFLAGS="-C target-cpu=x86-64-v3" cargo build --release \
  -p umts-hnb -p umts-management --features umts-hnb/uhd
cargo deb --no-build -p umts-hnb
sudo apt-get install ./target/debian/umts-hnb_*.deb
```

The service is not enabled on install. It runs as the `umts-hnb` user with
real-time scheduling and keeps runtime state in `/var/lib/umts-hnb`:

```sh
sudoedit /etc/umts-hnb/site.yaml
sudo systemctl enable --now umts-hnb
journalctl -u umts-hnb -f
```

`umts-hnbctl` uses the service's sockets when the working directory has no
`.runtime/` sockets. Members of the `umts-hnb` group can run read-only
commands without sudo.

## Configure

Copy the example configuration:

```sh
cp config/example.yaml site.local.yaml
```

Edit `site.local.yaml`:

- `cell`: PLMN, LAC, RAC, SAC and cell ID. These must match the core.
- `core`: HNB-GW address and the local GTP-U address.
- `radio`: channel numbers (UARFCN), antennas and gains. Settings for each
  radio are in [radios](docs/radios.md).

With the core on the same machine, the example `core` settings work
unchanged.

On macOS, also set:

```yaml
core:
  hnbgw:
    transport: usrsctp
    udp_encapsulation_port: 9899
```

Settings in `site.local.yaml` override `config/defaults.yaml`.

For dual-carrier HSPA, change these settings in `site.local.yaml`:

| Setting | Change |
| --- | --- |
| `cell.packet_modes` | Add `dc-hspa`. |
| `cell.secondary_carrier.uarfcn_dl` | Set the second downlink channel, 25 UARFCNs (5 MHz) from the first. |
| `radio.rx_delay_samples` | On a B200/B210, use `78.0` instead of the single-carrier `95.0`. Other radios need their own timing calibration. |

Single-carrier modes default to 7.68 MS/s TX for one 5 MHz channel. DC-HSPA
automatically selects 15.36 MS/s TX for two 5 MHz channels, with centers
5 MHz apart and a combined 10 MHz span. RX remains at 7.68 MS/s. TX analog
bandwidth adjusts automatically unless explicitly set.

## Run

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

If a phone attaches but has no data, check
[packet routing](docs/core-setup.md#5-set-up-packet-routing).

## Development

```sh
bash scripts/check.sh
```

Runs formatting, Clippy and the workspace tests.

## License

AGPL-3.0-or-later. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
