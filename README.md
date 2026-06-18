# dvb-rs

A Rust, DVBv5-style **tune / scan / EPG** tool for the Linux DVB API v5,
targeting ISDB-T tuners. Speaks to the kernel frontend/demux devices
through **direct ioctls** (`nix`/`libc`) — no `libdvbv5`, no other C
shared-library dependency. `bindgen` is used at build time only to
generate the kernel ABI structs.

Think of it as a focused, scriptable replacement for `dvbv5-zap` /
`dvbv5-scan` tailored to this stack.

## Subcommands

| Command   | What it does |
|-----------|-------------|
| `tune`    | Lock the frontend and stream the full TS to stdout (`dvbv5-zap -P -o -` style). The primary feed for live playback / recording. |
| `scan`    | Lock, read one PAT + SDT, emit the transport's services as JSON. |
| `info`    | `FE_GET_INFO` — frontend name + capability summary. |
| `epg`     | Collect EIT (PID `0x0012`) present/following (`0x4E`) and optionally schedule (`0x50–0x5F`); print or emit events as JSON. |
| `dump-conf` | Convert a legacy `.conf` channel file to UTF-8 JSON. |
| `migrate`   | Migrate a legacy `.conf` to `channels.json` / `channels.toml`. |

Channels are resolved by name / alias from `channels.json` (the format
is shared with `isdb-hub`).

## Where it fits

`dvbr` is the **frontend** of the stack:
[`isdb-hub`](https://github.com/DuckFeather10086/isdb-hub) spawns `dvbr tune`
as a subprocess, pipes its TS through
[`b25`](https://github.com/DuckFeather10086/libaribb25-rs) for
descrambling, and fans the result out to HLS / recordings. `dvbr epg`
feeds the EPG store.

It depends on [`libaribb24-rs`](https://github.com/DuckFeather10086/libaribb24-rs)
to decode SDT service names and EIT programme text to UTF-8.

## Build

```bash
cargo build --release          # produces target/release/dvbr
```

Cross-process adapter serialization is an flock on
`/tmp/dvbr-adapter{N}.lock`; set `DVBR_SKIP_ADAPTER_LOCK=1` only when
the caller already holds the lock.

See the umbrella repo
[`isdb-workspace`](https://github.com/DuckFeather10086/isdb-workspace) for the
full picture.
