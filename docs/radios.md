# Radios

Each section gives the `radio` settings for one radio. Replace the `radio`
block in `site.local.yaml` with them and keep the channel numbers (UARFCN)
the site is authorized to use.

## Ettus B200/B210

The B200 and B210 run through the UHD backend. `config/example.yaml` already
uses these settings:

```yaml
radio:
  backend: uhd
  uhd_args: type=b200,master_clock_rate=30720000
  tx_antenna: TX/RX
  rx_antenna: RX2
  uarfcn_dl: 4400
  uarfcn_ul: 4175
  tx_gain_db: 78.0
  rx_gain_db: 30.0
  rx_delay_samples: 95.0
```

With DC-HSPA, set `rx_delay_samples` to `78.0`. The gains are a starting
point, not an RF calibration.

## PlutoSky R1

The PlutoSky R1 runs through the UHD backend, using UHD's SoapySDR device
support (`type=soapy`) and the plutosky-stream SoapySDR module.

```yaml
radio:
  backend: uhd
  uhd_args: type=soapy,driver=plutosky
  tx_antenna: A
  rx_antenna: A_BALANCED
  uarfcn_dl: 4400
  uarfcn_ul: 4175
  tx_gain_db: 80.0
  rx_gain_db: 40.0
  rx_delay_samples: 38.0
  tx_lo_offset_hz: 0
```

`tx_lo_offset_hz: 0` is required. The gains are a starting point, not an RF
calibration.

DC-HSPA is not yet stable on the PlutoSky R1, so keep `cell.packet_modes` to
`rel99`, `hspa` and `hspa-plus`.

## SoapySDR

Install `libsoapysdr-dev` and the SoapySDR module for your radio, then build
with `--features soapy`. Select the driver with `radio.soapy_args` and set
both `tx_antenna` and `rx_antenna` to port names supported by that driver.

For example, these settings select the PlutoSky R1's SoapySDR module directly:

```yaml
radio:
  backend: soapy
  soapy_args: driver=plutosky
  tx_antenna: A
  rx_antenna: A_BALANCED
  uarfcn_dl: 4400
  uarfcn_ul: 4175
  tx_gain_db: 80.0
  rx_gain_db: 40.0
  rx_delay_samples: 38.0
  tx_lo_offset_hz: 0
```

The radio must support hardware time and timestamped TX/RX streams. Recheck
the gains and `rx_delay_samples` calibration when changing the radio or backend.

## Other radios

bladeRF radios use the `bladerf` backend, selected at build time as described
in the [README](../README.md#building-from-source). Each radio needs its own
`rx_delay_samples` calibration.
