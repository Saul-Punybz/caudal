# IP multicast output

`[[multicast]]` sends a live stream to an IP multicast group. The server
sends one copy; the network replicates it to every receiver that joined
the group. This is how cable/IPTV headends, campus TV and hotel systems
carry live channels, and it is the only way their set-top boxes and TVs
take them.

```toml
[[multicast]]
stream = "tv1"
group = "239.1.1.1:5000"
format = "ts"          # or "rtp"
ttl = 16
interface = "10.0.0.5" # this host's address on the video network
```

## What goes on the wire

- **`format = "ts"`**: MPEG-TS over UDP, seven 188-byte TS packets
  (1316 bytes) per datagram. That is the IPTV convention: it fits a
  1500-byte Ethernet MTU with room for the IP/UDP headers. Receivers open
  `udp://@239.1.1.1:5000`.
- **`format = "rtp"`**: the same 1316 bytes behind a 12-byte RTP header
  (RFC 2250, payload type 33, a 90 kHz timestamp, consecutive sequence
  numbers). Receivers see loss and reordering. They open
  `rtp://@239.1.1.1:5000`, or read an SDP with `m=video 5000 RTP/AVP 33`.
- **In the transport stream**: one program with H.264 or H.265 video on
  PID 0x101, AAC on 0x102 and SCTE-35 cues on 0x103 when the source has
  them. PAT/PMT go out every 100 ms of media time and on every keyframe,
  so a receiver tunes in within 100 ms and decodes from the next keyframe.
  Every video frame carries a PCR, and PTS/DTS run 0.7 s ahead of it
  (ffmpeg's default mux delay), which is how long receivers buffer. Opus
  has no TS mapping here, so it is dropped with a warning.
- **Pacing**: datagrams leave on the stream's own clock (each frame's DTS
  maps to a send time). A keyframe's datagrams are spread at 1.5 times the
  stream's average bitrate instead of leaving in one burst, because
  switches with shallow buffers drop bursts. A receiver joining at the live
  edge gets the GOP since the last keyframe at real-time speed, not all at
  once. `pacing_lag_ms` in the API shows how late the last datagram left
  against its frame's slot, a few ms normally and tens of ms after a large
  keyframe. It is capped at 1 s. `pacing = false` turns pacing off.

The output starts when the stream goes live, stops when it ends, and starts
again on the next publish. A stream behind `[[failover]]` stays live across
source switches, so the group keeps flowing.

## Network requirements

- **IGMP snooping on every switch** (and an IGMP querier on the VLAN,
  usually the router or one switch). Without snooping, a switch floods
  multicast to every port like broadcast, so every device on the VLAN gets
  every channel. With snooping, only ports whose receivers joined get it.
  Put video on its own VLAN.
- **TTL**: each router hop decrements it; at 0 the datagram is dropped.
  `ttl = 1` keeps the stream on the local subnet. Crossing routers also
  needs multicast routing (PIM) on them. The default of 16 suits a campus;
  it does not make the stream leave your network.
- **`interface`**: on a host with more than one network, name the address
  of the one facing the video network, or the OS sends on the default
  route. It is also the source address receivers see, which matters for
  source-specific multicast (SSM, `232.0.0.0/8`) and for firewalls. For an
  IPv6 group it is the interface index (`ip link` / `ifconfig` order), not
  an address.
- **Addresses**: use `239.0.0.0/8` (administratively scoped, private like
  10/8) for your own channels, one group per channel. Don't use
  `224.0.0.0/24`: routers never forward it and some OSes treat it
  specially. For IPv6, `ff15::/16` is the site-local equivalent.
- **Bandwidth**: every joined port carries the whole stream, with no
  adaptation. Size the ladder for the slowest link that joins.
- **Firewalls**: open UDP to the group's port on receivers, and IGMP (IP
  protocol 2) wherever joins must pass.

## Why not over the internet

The public internet does not route multicast. ISPs don't run inter-domain
multicast for customers, and a datagram to 239.x.x.x never leaves your edge
router. For viewers outside the network, use unicast outputs (LL-HLS,
WebRTC, SRT, MoQ), or run a Caudal edge inside each remote network (a
cluster edge, see `docs/research/CLUSTER.md`) and multicast from there.
UDP also has no retransmission, so any loss shows up as picture errors. It
suits a managed LAN, not a lossy WAN. SRT or RTP with FEC/retransmission is
for the WAN hop.

## Checking it

On any host on the video network (or on the server itself with
`loopback = true`):

```sh
ffprobe udp://239.1.1.1:5000                       # finds the program
ffmpeg -i udp://239.1.1.1:5000 -t 10 -f null -     # decodes 10 s
tsp -I ip 239.1.1.1:5000 -P continuity -P pcrverify -O drop   # TSDuck
curl -s localhost:8080/api/v1/multicast            # packets, bytes, pacing lag
```

`/metrics` exports `caudal_multicast_packets_total`,
`caudal_multicast_bytes_total`, `caudal_multicast_send_errors_total` and
`caudal_multicast_pacing_lag_seconds`, labelled by stream, group and format.

A receiver that joins mid-GOP logs decoder errors ("non-existing PPS")
until the next keyframe. That is expected with any multicast source.
