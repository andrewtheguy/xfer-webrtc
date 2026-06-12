# Common Use Cases & Scenarios

This guide describes common scenarios where `xfer-webrtc` shines and which
mode to use for each.

## 1. Standard Internet Transfer (Nostr signaling)
**Scenario**: You want to send a file to a peer over the internet without
exchanging IP addresses manually.

**Solution**: **Online Mode** (default)
- **Why**: Nostr relays handle signaling so the two peers can negotiate a direct
  WebRTC data channel. STUN provides NAT traversal. Relays are auto-discovered.
- **Command**:
  ```bash
  # Sender
  xfer-webrtc send /path/to/file

  # Receiver
  xfer-webrtc receive <XFER_CODE>
  ```
- **Experience**: Share the printed xfer code via any channel (chat, paper,
  verbal). The Nostr relays only carry signaling; file bytes flow directly
  peer-to-peer.

---

## 2. No Internet / Relays Blocked (LAN or routed private network)
**Scenario**: You need to transfer files when Nostr relays are unavailable (no
internet, or relays blocked), but both machines can still reach each other
directly over a LAN or routed private/VPN network.

**Solution**: **Manual Mode** (`send --manual` / `receive`)
- **Why**: Signaling is exchanged by copy-paste instead of through a relay, so no
  relay or third-party signaling service is required. The data channel is still a
  direct peer-to-peer WebRTC connection.
- **Note**: Manual mode only removes *relay signaling*. The peers are still
  created with public STUN servers (`WebRtcPeer::new()`), so ICE will attempt to
  contact them for reflexive candidates if the network allows it. For a true
  air-gapped/LAN-only setup with no outbound STUN traffic, a no-STUN peer
  constructor (`WebRtcPeer::new_offline()`) exists but is not currently wired
  into the manual commands.
- **Command**:
  ```bash
  # Sender
  xfer-webrtc send --manual /path/to/file

  # Receiver (paste the manual offer; the mode is detected automatically)
  xfer-webrtc receive
  ```
- **Experience**: The sender prints an offer code; the receiver pastes it into
  `receive` (which auto-detects manual mode) and replies with an answer code. The
  exchanged text includes signaling metadata needed to establish the WebRTC
  data channel.

---

## 3. Folder Transfer
**Scenario**: Sending an entire directory rather than a single file.

**Solution**: Pass the directory path; it is auto-detected and archived (tar)
before transfer.
```bash
xfer-webrtc send /path/to/folder
```
