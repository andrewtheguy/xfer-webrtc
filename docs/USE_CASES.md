# Use Cases

## Send Between CLI and secure-send-web

Sender:

```bash
secure-send-cli send ./file.bin
```

Enter the printed PIN in `secure-send-web` receive mode.

## Receive From secure-send-web

Start a send in `secure-send-web`, then run:

```bash
secure-send-cli receive <PIN>
```

Use `--output` to choose a destination directory.

## CLI to CLI

Sender:

```bash
secure-send-cli send ./file.bin
```

Receiver:

```bash
secure-send-cli receive <PIN>
```

## Manual Copy/Paste Signaling

Use this when Nostr relays are unavailable but both peers can still establish a
WebRTC connection.

Sender:

```bash
secure-send-cli send --manual ./file.bin
```

Receiver:

```bash
secure-send-cli receive --manual
```

Manual mode exchanges SS03 offer and answer text. It does not add QR support.
