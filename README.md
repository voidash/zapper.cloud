# ⚡ zap

Fast, encrypted peer-to-peer file transfers.

## Features

- **End-to-end encrypted** - Files are encrypted before leaving your device
- **Peer-to-peer** - Direct transfers between devices, no cloud storage
- **NAT traversal** - Works across networks and firewalls
- **Simple codes** - Share 6-character codes instead of long URLs
- **No signup required** - Just send and receive

## Install

### macOS (Homebrew)

```bash
brew install voidash/tap/zap
```

### Linux / macOS (curl)

```bash
curl -fsSL https://zapper.cloud/install.sh | sh
```

### From Source

```bash
cargo install --git https://github.com/voidash/zapper.cloud
```

## Usage

### Send a file

```bash
zap send photo.jpg
# Code: abc123
```

### Receive a file

```bash
zap receive abc123
# Saved: photo.jpg
```

### Web interface

Visit [zapper.cloud](https://zapper.cloud) for browser-based transfers.

## How it works

Zap uses [iroh](https://iroh.computer) for peer-to-peer networking with automatic NAT traversal. When you send a file:

1. Your device creates an encrypted connection endpoint
2. A short code is generated and registered with the relay server
3. The receiver uses the code to look up your connection details
4. Files transfer directly between devices, end-to-end encrypted

The relay server only stores connection metadata temporarily - your files never touch our servers.

## Self-hosting

Run your own relay server:

```bash
zap serve --addr 0.0.0.0:8080
```

Then use `--relay` flag to point to your server:

```bash
zap send --relay https://your-server.com photo.jpg
zap receive --relay https://your-server.com abc123
```

## License

MIT
