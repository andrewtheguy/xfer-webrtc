# Architecture

`secure-send-cli` is a CLI client for `secure-send-web`. The web app is the source
of truth for protocol shape and compatibility.

## Modes

### Nostr PIN Mode

This is the default mode.

1. Sender generates a 12-character PIN, 16-byte salt, transfer id, and ephemeral
   Nostr key.
2. Sender derives three PBKDF2-SHA256 AES-256-GCM keys from the PIN and salt:
   metadata, signals, and P2P content.
3. Sender publishes a kind `24243` PIN exchange event:
   - `content`: `base64(nonce || ciphertext || tag)` encrypted with the metadata key.
   - tags: `h`, `s`, `t`, `type=pin_exchange`, `expiration`.
4. Receiver derives current and previous time-bucket PIN hints, fetches matching
   kind `24243` events, derives keys from the event salt, and decrypts metadata.
5. Receiver publishes a kind `24242` authenticated ACK with `seq=0`, encrypted
   with the signals key.
6. Sender and receiver exchange encrypted kind `24242` WebRTC signal events:
   `offer`, `answer`, and `candidate`.
   - Signal events use tags `t`, `p=<sender pubkey>`, and `type=signal`.
   - Sender-side answer subscriptions filter by `t`, `p=<sender pubkey>`, and
     receiver author.
   - Receiver-side offer subscriptions filter by `t` and sender author only,
     matching `secure-send-web`.
   - Offer and answer bundles are republished while the P2P connection is
     pending so relay misses do not strand the session.
7. File bytes transfer directly over the WebRTC data channel using the P2P
   content key.

Default relays match `secure-send-web`.

### Manual SS03 Mode

Manual mode is explicit: `send --manual` and `receive --manual`.

The signaling payload is the web app's SS03 format:

```text
JSON -> raw DEFLATE -> "mag!" || compressed -> time-bucket XOR
     -> "SS03" || obfuscated -> standard base64
```

Manual offer payloads contain SDP, ICE candidate strings, file metadata,
created-at timestamp, sender P-256 public key, and salt. Manual answer payloads
contain SDP, ICE candidate strings, created-at timestamp, and receiver P-256
public key.

Both sides derive the AES content key with:

```text
HKDF-SHA256(
  ikm = P-256 ECDH shared X coordinate,
  salt = offer salt,
  info = "secure-send-mutual",
  len = 32
)
```

## Data Channel Protocol

Each plaintext chunk is at most 128 KiB. Each encrypted binary data-channel
message is:

```text
2-byte chunk index (big-endian) || 12-byte nonce || ciphertext || 16-byte tag
```

The chunk index is also AES-GCM additional authenticated data.

After all chunks, the sender sends:

```text
DONE:<total_chunks>
```

The receiver authenticates and persists the full file, then replies:

```text
ACK
```

Active P2P transfers use a 60-second idle/stall window rather than an overall
wall-clock deadline. The sender applies the window to each chunk hand-off while
waiting for WebRTC backpressure to clear and sending the message. The receiver
arms the same window once the data channel is open and resets it for every
incoming data-channel message, including `DONE:<total_chunks>`.

## Scope

The CLI intentionally has no legacy signaling protocol, no resume path, no QR
support, no relay discovery, and no custom fallback mode.
