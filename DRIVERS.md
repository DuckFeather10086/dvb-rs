# Driver notes

`dvbr` speaks the kernel DVB API directly. The API is the same on every
adapter; what sits behind it is not. Two drivers have already cost a day
each here, and in both cases the symptom pointed somewhere other than the
driver — a scan that "hangs", a box that "fills its disk with logs". This
file is the per-driver record, and the checklist to run against the next
one before believing anything it reports.

Nothing in here is a reason to special-case a driver in code. `dvbr` picks
the approach that works everywhere (see `si_reader`); this file explains
*why* those choices look more roundabout than they need to.

## What `dvbr` actually asks of a driver

Everything below is reachable from three device nodes, and this is the
whole surface — an audit only has to cover these:

| Call | Where | Used by |
|---|---|---|
| `FE_GET_INFO` | `frontend.rs:56` | `dvbr fe-info` |
| `FE_SET_PROPERTY` (DVBv5, `SYS_ISDBT`) | `tuner.rs` | every subcommand |
| `FE_READ_STATUS`, polled | `tuner.rs:247` `wait_lock` | every subcommand |
| `FE_READ_SIGNAL_STRENGTH` (legacy v3) | `signal.rs:25` | `tune` startup report |
| `DMX_SET_PES_FILTER` / `DMX_OUT_TS_TAP`, one PID | `demux.rs:72` `tap_pid_to_dvr` | service PID taps, all SI reads |
| the same with PID `0x2000` | `demux.rs:62` `zap_all_pids_to_dvr` | `tune --full-mux` only |
| `read()` on `dvr0` | `demux.rs` `DvrReader` | the TS itself |
| `DMX_SET_FILTER` (section filter) | `demux.rs:52` | **nothing — deliberately dead** |

That last row is the point of `si_reader`: the kernel demux can filter and
deliver whole sections by itself, which is the obvious way to read PAT /
PMT / SDT / EIT, and `dvbr` does not use it on any driver because it does
not work on ours. The function is kept so that the next person to reach
for it finds this file first.

## The audit

`scripts/driver-audit.sh` runs the mechanical parts. What it answers, and
what each answer means:

1. **Does the frontend enumerate `SYS_ISDBT`?** `FE_GET_INFO` will not
   tell you -- it carries a name and a caps bitfield, and `dvbr fe-info`
   prints exactly that. The delivery systems come from `DTV_ENUM_DELSYS`,
   which is why the probe asks for them separately. A frontend that does
   not list it is not ours to drive. On a mixed card (PT3 = 2×T + 2×S) the terrestrial
   and satellite frontends are separate adapters; dispatching to the wrong
   one does not fail cleanly, it waits out the lock timeout and reports a
   weak signal.
2. **How long does the lock take, and does it lie?** `wait_lock` believes
   `FE_HAS_LOCK`. A driver that raises it early makes every downstream
   timeout look like a signal problem.
3. **Does a single-PID tap to `dvr0` deliver bytes?** This is the whole
   data path. Zero bytes on a locked frontend is the failure mode the
   `DVR_STALL_TIMEOUT` watchdog exists for.
4. **How many simultaneous PID taps?** A service needs PAT + PMT + PCR +
   CA + every ES PID: 16 to 18 on the ISDB-T services here. A driver with a
   smaller filter table silently drops the rest. The table is per *adapter*,
   not per process, so the probe can only count what is left over and the
   report adds its own tune's taps back to get the real number.
5. **Does the full-mux tap (`0x2000`) work?** Optional — `tune --full-mux`
   is the only caller and it warns before using it.
6. **Does `DMX_SET_FILTER` return, and does a section then arrive?** Two
   questions, and smsusb below is why they are asked separately: the ioctl
   returning 0 says nothing about delivery. Ask an SI PID, not PAT, and ask
   it *before* the full-mux probe — a driver that does not fully undo a
   whole-mux tap on close leaves SI arriving afterwards, and the answer then
   describes a configuration no ordinary tune produces.
7. **What does polling `FE_READ_STATUS` cost?** Nothing, on a sane driver.
   See smsdvb below, where it costs about 45 kernel stack traces per call.

Run it against a tuned channel you know is good:

```sh
scripts/driver-audit.sh -a 0 -c channels.json NHK_G
```

It writes a short report and leaves the raw evidence in `/tmp`. It tunes,
so give it the adapter — stop anything else holding it first.

## Drivers

Verified means someone ran the audit on real hardware and wrote the result
down. Everything else is listed because it is in the tree and could
plausibly be under `dvbr` one day, not because it is known to work.

| Driver | Devices | Status |
|---|---|---|
| `smsusb` + `smsdvb` + `smsmdtv` | Siano SMS1xxx / SMS2270. Audited on a Siano Rio Digital Receiver, 6.8.0-137, 2026-08-28 and again 2026-08-30. The PLEX PX-S1UD is reported to be the same family — unverified. | **in use, one live defect (smsdvb, below) and one that no longer reproduces.** `DTV_ENUM_DELSYS` = `SYS_ISDBT` only. Filter table **32, shared per adapter**: a service tune holding 16-18 taps leaves exactly that many for anyone else, and dvb-core reports the ceiling as `EMFILE` from `open()`, which reads as a process fd limit and is not one. Lock in 2.02s, both muxes. Full-mux accepted and real: 36 PIDs vs 17. 1.85-1.88 MiB/s out of dvr0 on a 1-seg+HD mux. |
| `earth-pt3` | Earthsoft PT3 (`tc90522` demod + `qm1d1c0042`) | untested |
| `earth-pt1` | Earthsoft PT1 / PT2 | untested |
| `mb86a20s`, `s921` | Brazilian ISDB-T front ends behind `em28xx` / `cx231xx` | untested, and out of scope for this stack |
| `mn88443x`, `helene` | ISDB-S/T demod and tuner present in this kernel; no consumer device tried here | untested |
| out-of-tree `px4_drv` | PLEX PX4 / PX5 / PX-MLT, PX-S1UR / M1UR, e-Better DTV series | **it is the latter, so none of this file applies to it.** These register `/dev/px4video{N}` and no DVB adapter, so there is no frontend, no demux and no dvr to audit — which is why `dvbr` grew a second backend (`--backend px4`, `src/px4.rs`) rather than a driver note. Tuning is one ioctl with a physical channel number and everything below it happens in userspace. **Untested on hardware**: the ioctl ABI and the `freq_no` mapping are checked against the driver's own source, nothing more. Its own checklist is a different one and does not exist yet. |

## smsusb: `DMX_SET_FILTER` succeeds, and now delivers

This section has been wrong twice, in opposite directions, and the second
time is the one to learn from.

The first account — carried in `si_reader.rs` and HANDOFF.md — was that
smsusb does not implement section filtering and the ioctl blocks forever.
That conflates the ioctl with the read after it: `DMX_SET_FILTER` returns 0,
every time. The second account, written here on 2026-08-28, was that the
ioctl returns but the sections never arrive for the PIDs SI lives on.
Measured again on 2026-08-30, with the audit script's own two defects fixed
(it was parsing `0x0011` with `atoi()` and so probing PAT every time, and it
was running the section probe *after* a full-mux tap), that is not what the
hardware does:

| Configuration | `DMX_SET_FILTER` | section delivered |
|---|---|---|
| PID 0x0011 (SDT), service tap running, NHK_G | returns 0 | **yes, 125 bytes** |
| PID 0x0012 (EIT), service tap running, NHK_G | returns 0 | **yes, 82-103 bytes** |
| PID 0x0011 (SDT), service tap running, TBS1 | returns 0 | **yes, 106 bytes** |
| PID 0x0012 (EIT), service tap running, TBS1 | returns 0 | **yes, 324 bytes** |
| PID 0x0000 (PAT), service tap running | returns 0 | yes, 32 bytes |

Three runs, two muxes, each inside a 10s budget, and neither 0x0011 nor
0x0012 was among the PIDs the tune had tapped (the audit prints that list).
So on this box, today, the kernel section filter works.

**What changed is not known, and the most likely candidate is the other
defect in this file.** Between the two measurements this box moved to
`debugfs=off`, which stops `smsdvb`'s statistics printers — and those run
inside `smsdvb_onresponse()`, the driver's own response handler, at 32 to 58
kernel backtraces per statistics message, one message per 100 ms while
tuned. A driver spending that much time unwinding stacks in its response
path is a plausible reason for sections to go missing, which would make the
two defects below one defect. That is a hypothesis, not a measurement: it
needs a reboot with debugfs on and this audit run again, and nobody has done
it.

**None of which changes what `dvbr` does, and the reasons are not about
smsusb.** `si_reader` reassembles sections in userspace because that works on
every driver, because it is what the px4 backend must do anyway — that
chardev has no kernel demux at all — and because the one measurement that has
never been in doubt is the original symptom: `dvbr scan` hung on every
invocation until it stopped using the kernel section filter. Switching back
on the strength of three runs on one adapter, to save nothing a user can
see, is not a trade worth making.

**So: do not reintroduce `DMX_SET_FILTER` without running the audit on
smsusb — and read the script's own caveats first, because both of its
defects produced confident, wrong answers.**

### What this cost before it was understood

`dvbr scan` hung on every invocation while `dvbr tune`, which never took that
path, worked fine: 120s external timeout, no output, exit 124, nothing in the
log. The channel names in `channels.json` went un-refreshed for months as a
result, which is how a blocked read in one subcommand turned into "half the
channel list is mojibake".

## smsdvb: every `FE_READ_STATUS` costs ~45 kernel backtraces

Not a `dvbr` bug and not one `dvbr` can avoid, but it looks like one, so it
belongs here.

`smsdvb`'s debugfs statistics printers were converted from `scnprintf()` to
`sysfs_emit_at()` in 2f7d0c94396e (v6.2). `sysfs_emit_at()` WARNs unless its
buffer is page aligned, and this buffer is a member of a `kzalloc()`ed
struct, so it never is:

```
WARNING: CPU: 2 PID: 0 at fs/sysfs/file.c:777 sysfs_emit_at+0x64/0xd0
 smsdvb_print_isdb_stats_ex+0x2ab/0x650 [smsdvb]
 smsdvb_update_isdbt_stats_ex+0x31/0x6e0 [smsdvb]
 smsdvb_onresponse+0x2bc/0x660 [smsdvb]
 smscore_onresponse+0x91/0x520 [smsmdtv]
 smsusb_onresponse+0x12d/0x230 [smsusb]
```

The chain that arms it starts at `.read_status`:
`smsdvb_read_status()` → `smsdvb_send_statistics_request()` (rate-limited
to one per 100 ms) → the device answers → the printer runs and every one of
its 32 call sites WARNs separately. One statistics message is 32 to 58
backtraces, depending on how many ISDB-T layers the mux carries.

**Nothing userspace does can dodge it.** dvb-core's own frontend thread
calls `read_status` while the frontend is tuned, so a tune that never asks
for status floods anyway. Measured on 6.8.0-137, one ferrite EPG pass —
twelve muxes, about a minute each: **29,182 backtraces, 1.6 million lines of
kernel log, in 12m43s**. Four passes a day. It also breaks the debugfs
`stats` file outright — the WARN path returns 0, so the byte count never
advances and `read()` on that file blocks forever.

Mitigation is `debugfs=off` on the kernel command line: with debugfs
disabled `smsdvb_debugfs_register()` bails, `prt_isdb_stats_ex` is never
assigned, and the printers never run. Verified — the same EPG pass then
produced **0 backtraces and 1 line of kernel log**. Nothing is lost; that
debugfs file never worked in any kernel that has this bug.

Affected: v6.2 through at least v7.3-rc1 — still unfixed in mainline. A
revert is prepared but unsent; ask before assuming it landed.

## Adding a driver to this file

Run the audit, paste the report, and write a row in the table. A row that
says "untested" is worth more than a row that says "should work" — the two
defects above were both in drivers that should have worked.
