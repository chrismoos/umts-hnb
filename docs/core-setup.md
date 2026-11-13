# Core network

The HNB connects to a core network built from Osmocom daemons. This guide
installs them from Osmocom packages on Ubuntu 26.04 and runs them as system
services with the configuration the packages ship. The core can run on the
HNB machine or on a separate machine or VM.

| Daemon | Role | Address |
| --- | --- | --- |
| HNB-GW | Accepts the HNB's Iuh connection | 0.0.0.0, port 29169 |
| STP | Routes signaling between the HNB-GW, MSC and SGSN | 127.0.0.1 |
| HLR | Stores subscribers and their USIM keys | 127.0.0.1 |
| MSC | Handles calls and SMS | 127.0.0.1 |
| MGW | Carries call audio | 127.0.0.1 |
| SGSN | Handles packet data sessions | 127.0.0.1 |
| GGSN | Assigns phone IP addresses and connects them to the network | 127.0.0.2 |

## 1. Add the Osmocom package repository

Osmocom publishes current packages for each Ubuntu release. Install the tools
needed to add the repository:

```sh
sudo apt-get update
sudo apt-get install --no-install-recommends ca-certificates curl gpg
```

Add the repository and its signing key for the running Ubuntu release:

```sh
. /etc/os-release
OSMOCOM_REPO="https://downloads.osmocom.org/packages/osmocom:/latest/xUbuntu_${VERSION_ID}"
curl -fsSL "$OSMOCOM_REPO/Release.key" | \
  sudo gpg --dearmor -o /etc/apt/keyrings/osmocom.gpg
echo "deb [signed-by=/etc/apt/keyrings/osmocom.gpg] $OSMOCOM_REPO/ ./" | \
  sudo tee /etc/apt/sources.list.d/osmocom.list
```

## 2. Install the daemons

```sh
sudo apt-get update
sudo apt-get install --no-install-recommends \
  osmo-stp osmo-hlr osmo-msc osmo-mgw osmo-sgsn osmo-ggsn osmo-hnbgw
```

Each package installs a systemd service, a configuration file in
`/etc/osmocom/` and starts the service. The packaged configuration connects
the daemons to each other over loopback and uses PLMN `001-01`, which matches
`cell.plmn` in `config/example.yaml`. No further configuration is needed when
the HNB runs on the same machine.

## 3. Check the core

The HNB-GW, MSC and SGSN each register with the STP when they start. Open the
STP console:

```sh
nc 127.0.0.1 4239
```

At the prompt, enter:

```text
enable
show cs7 instance 0 asp
```

The output lists one row for each of the three daemons, and each row reads
`ASP_ACTIVE`. Address columns are omitted here:

```text
ASP Name      AS Name       State          Type  Role  SCTP Role
------------  ------------  -------------  ----  ----  ---------
asp-dyn-0     as-rkm-1      ASP_ACTIVE     m3ua  sg    server
asp-dyn-1     as-rkm-2      ASP_ACTIVE     m3ua  sg    server
asp-dyn-2     as-rkm-3      ASP_ACTIVE     m3ua  sg    server
```

Press Ctrl+C to leave the console.

If a row is missing, check whether its service is running and read its log:

```sh
systemctl status 'osmo-*'
journalctl -u osmo-msc -n 50
journalctl -u osmo-hnbgw -f
```

## 4. Provision the USIM

The HLR stores each subscriber with the secret keys of its USIM. The core uses
those keys to authenticate the phone, so they must match the card exactly.
Take the IMSI, K and OPc (or OP) from the USIM's programming record, and
choose a phone number (MSISDN).

Open the HLR console:

```sh
nc 127.0.0.1 4258
```

Enter these commands, replacing `IMSI`, `MSISDN`, `K` and `OPC`:

```text
enable
subscriber imsi IMSI create
subscriber imsi IMSI update msisdn MSISDN
subscriber imsi IMSI update aud3g milenage k K opc OPC
subscriber imsi IMSI update network-access-mode cs+ps
subscriber imsi IMSI show
```

Use `op` instead of `opc` when the card record lists OP.

Changes take effect immediately. Press Ctrl+C to leave the console.

## 5. Set up packet routing

Phones receive addresses from `172.16.222.0/24` on the APN `internet`. The
GGSN delivers their traffic to the `tun4` interface. To reach the Internet,
the core must forward that traffic and translate it (NAT) to its own uplink
address.

Find the uplink interface. It follows `dev` on the first line of the output:

```sh
ip route show default
```

Enable IP forwarding, now and at every boot:

```sh
echo net.ipv4.ip_forward=1 | sudo tee /etc/sysctl.d/90-ip-forward.conf
sudo sysctl -w net.ipv4.ip_forward=1
```

Add the NAT rule, replacing `eth0` with the uplink interface:

```sh
sudo iptables -t nat -A POSTROUTING -s 172.16.222.0/24 -o eth0 -j MASQUERADE
```

Install `iptables-persistent` to restore the rule at every boot. When the
installer asks whether to save the current IPv4 rules, answer yes:

```sh
sudo apt-get install --no-install-recommends iptables-persistent
```

Check that the rule is in place:

```sh
sudo iptables -t nat -S POSTROUTING
```

The output includes a `MASQUERADE` rule for `172.16.222.0/24`. If the rule is
changed later, save it again with `sudo netfilter-persistent save`.

A firewall that drops forwarded traffic by default, such as Docker or ufw,
also needs forwarding to be accepted for the phones:

```sh
sudo iptables -I FORWARD -i tun4 -j ACCEPT
sudo iptables -I FORWARD -o tun4 -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT
sudo netfilter-persistent save
```

On the phone, set the APN to `internet` with protocol IPv4. The packaged
configuration also has IPv6 APNs, `inet6` and `inet46`, which this rule does
not route.

## HNB on another machine

The HNB-GW accepts Iuh on every address. The MGW and GGSN listen only on
loopback by default. For a core at `192.168.1.10`, edit these settings in
`/etc/osmocom/` so the HNB can reach them and the SGSN uses the new GGSN address:

| File | Setting | Value |
| --- | --- | --- |
| `osmo-mgw.cfg` | `rtp bind-ip` | `192.168.1.10` |
| `osmo-ggsn.cfg` | `gtp bind-ip` | `192.168.1.10` |
| `osmo-sgsn.cfg` | `ggsn 0 remote-ip` | `192.168.1.10` |

Replace the example address with the core machine's address, then restart:

```sh
sudo systemctl restart osmo-mgw osmo-ggsn osmo-sgsn
```

An HNB running on macOS reaches the HNB-GW with SCTP tunneled over UDP port
9899. Enable it on the core, now and at every boot:

```sh
echo net.sctp.udp_port=9899 | sudo tee /etc/sysctl.d/90-sctp-udp.conf
sudo sysctl -w net.sctp.udp_port=9899
```

In the HNB configuration, point at the core and set the HNB machine's own
address for packet data:

```yaml
core:
  hnbgw:
    address: 192.168.1.10
  gtpu:
    bind_ip: 192.168.1.20   # HNB machine, reachable from the core
```

Continue with [building the HNB](../README.md#build).
