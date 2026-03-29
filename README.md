# HopToDesk for FreeBSD

### Free, Secure Remote Desktop for FreeBSD

![HopToDesk on FreeBSD](https://www.hoptodesk.com/img/hoptodesk-freebsd.png)

HopToDesk for FreeBSD is a free, fast, and lightweight remote desktop application built in Rust with a modern WebKitGTK-based UI. It brings full remote access capabilities to **FreeBSD (amd64 and aarch64)**, including remote desktop control, file transfer, chat, and more. This is a separate project from the main [HopToDesk project on GitLab](https://gitlab.com/hoptodesk/hoptodesk) due to the unique requirements of building for FreeBSD. For the full cross-platform version (Windows, macOS, Linux), visit [hoptodesk.com](https://www.hoptodesk.com).

## Features

- **End-to-End Encryption** — Curve25519 key exchange + XSalsa20-Poly1305 authenticated encryption with Ed25519 signing. Wire-compatible with the standard HopToDesk client.
- **Remote Desktop Control** — Low-latency screen sharing with keyboard and mouse input, multi-monitor support, and aspect ratio preservation.
- **File Transfer** — Bidirectional file and folder transfer with progress tracking, directory browsing, and hidden file support.
- **Chat** — Real-time text chat during remote sessions.
- **Clipboard Sync** — Automatic bidirectional clipboard text synchronization with compression.
- **Wake on LAN** — Send magic packets to wake sleeping machines on your network.
- **Unattended Access** — Set a permanent password for always-on remote access without user interaction.
- **TCP Tunneling** — Port forwarding and RDP tunneling through encrypted connections.
- **Proxy Support** — Route connections through an HTTP or SOCKS5 proxy server.
- **Custom Network** — Use your own API server instead of the default HopToDesk network.
- **Session Recording** — Record incoming and outgoing remote sessions.
- **Two-Factor Authentication** — TOTP-based 2FA for enhanced security.
- **Dark Theme** — Full dark mode support across all windows.
- **LAN Discovery** — Automatically find HopToDesk devices on your local network.
- **Direct IP Access** — Connect directly by IP address without a relay server.
- **MCP Capable** — Allow AI agents to view and manage devices with powerful MCP support function.
- **44 Languages** — Runtime language switching with no restart required.

## Requirements

- FreeBSD 13+ (amd64 or aarch64)
- X11 or Wayland display server
- WebKitGTK and GTK3
- Minimal RAM and disk usage

## Download

Get the latest version at [hoptodesk.com](https://www.hoptodesk.com/)
