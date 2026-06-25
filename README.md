# Vantage

A from-scratch remote desktop tool in Rust. It captures a host machine's screen, encodes it, streams it over the network, decodes and renders it on a viewer, and drives the host's mouse and keyboard from that viewer. All first-party code, not a RustDesk fork.

It is built LAN-only first to prove the capture, encode, transport, and input pipeline end to end on real hardware. The transport is plain TCP for now and is meant to swap to WebRTC (str0m) later for use over the internet. The long-term goal is controlling my own PCs from anywhere, including a phone.

## Rung ladder

The project is built in verifiable rungs. Each was confirmed on real hardware before moving to the next.

| Rung | Proves |
|------|--------|
| 0 | Workspace scaffold. Three crates compile, each binary runs and prints its role. |
| 1 | Capture. The host grabs the primary display with scap and writes one frame to PNG. |
| 2 | Encode. The host captures a few seconds and encodes a playable H.264 file with openh264. |
| 3 | Live stream. The host streams encoded frames over TCP, the viewer decodes and renders them live in a window. |
| 4 | Remote input. The viewer forwards its mouse and keyboard to the host, which injects them with enigo. |

## Crates

| Crate | Role |
|-------|------|
| vantage-core | Shared protocol. serde `Message` types and length-prefixed framing over async streams. |
| vantage-host | The machine being controlled. Captures, encodes, streams, and injects received input. |
| vantage-viewer | The controlling machine. Connects, decodes, renders, and forwards local input. |

Stack:

- Capture: scap, continuous BGRA frames
- Codec: openh264, software H.264, compiled from source
- Transport: tokio TCP, length-prefixed bincode messages
- Render: winit window with softbuffer
- Input: enigo with serde input tokens

## Running

Build and run in release. Debug software encode and decode at 1080p or higher is too slow to be usable.

The host is the server. It binds `0.0.0.0:9000` and waits for a viewer.

On the host machine:

```sh
cargo run --release -p vantage-host
```

On the viewer machine:

```sh
cargo run --release -p vantage-viewer <host-lan-ip>:9000
```

With no argument the viewer connects to `127.0.0.1:9000`. Move the mouse over the viewer window to drive the host cursor, click to click, type to type. Close the viewer window to disconnect. The host keeps running and accepts the next viewer.

### Notes

- Use two machines to test input. On a single machine the host injects a mouse move, the cursor lands on the viewer window, and the viewer forwards it again, so it feeds back on itself.
- Building the host or viewer compiles OpenH264 from source, so a C++ compiler must be present. On Windows, cc finds MSVC through vswhere, no Developer Command Prompt needed.
- The first host run may trigger a Windows Firewall prompt for port 9000. Allow it for LAN access.

Developed and tested on Windows 11.

## Status

Rungs 0 through 4 are complete. The full LAN loop of capture, encode, transport, decode, render, and input works. Next is the transport swap to WebRTC for internet use, plus input refinements such as modifier combos and full key press and release.
