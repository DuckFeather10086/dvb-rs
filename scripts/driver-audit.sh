#!/bin/bash
# Audit one DVB driver against everything dvbr asks of it.  See DRIVERS.md.
#
#   scripts/driver-audit.sh [-a N] [-d N] [-b path] [-s secs] -c channels.json <CHANNEL>
#
# Tunes the given channel, so nothing else may hold the adapter.  Read-only
# apart from that: it opens devices, it does not configure anything.
#
# Answers, in order:
#   1  FE_GET_INFO, and whether the frontend claims ISDB-T
#   2  time to FE_HAS_LOCK
#   3  bytes out of dvr0 with one PID tapped
#   4  how many simultaneous DMX_OUT_TS_TAP filters the driver accepts
#   5  whether the full-mux tap (PID 0x2000) is accepted
#   6  whether DMX_SET_FILTER returns at all          <- caught smsusb
#   7  what a tuned frontend costs in kernel log      <- caught smsdvb

set -u

ADAPTER=0
DEMUX=0
CHANNELS=""
DVBR=""
SECS=20
CHANNEL=""

usage() { sed -n '2,20p' "$0" | sed 's/^# \?//'; exit "${1:-1}"; }

while getopts "a:d:b:s:c:h" o; do
	case "$o" in
	a) ADAPTER=$OPTARG ;;
	d) DEMUX=$OPTARG ;;
	b) DVBR=$OPTARG ;;
	s) SECS=$OPTARG ;;
	c) CHANNELS=$OPTARG ;;
	h) usage 0 ;;
	*) usage ;;
	esac
done
shift $((OPTIND - 1))
CHANNEL="${1:-}"
[ -n "$CHANNEL" ] && [ -n "$CHANNELS" ] || usage

if [ -z "$DVBR" ]; then
	for c in ./target/release/dvbr ./target/release/dvb-rs \
	         ../target/release/dvbr ../target/release/dvb-rs \
	         "$(command -v dvbr 2>/dev/null)"; do
		[ -x "${c:-}" ] && { DVBR=$c; break; }
	done
fi
[ -x "${DVBR:-}" ] || { echo "no dvbr binary found; pass -b <path>"; exit 1; }

FE=/dev/dvb/adapter$ADAPTER/frontend0
DMXDEV=/dev/dvb/adapter$ADAPTER/demux$DEMUX
[ -e "$FE" ] || { echo "no $FE"; exit 1; }

OUT=$(mktemp -d /tmp/dvbr-audit.XXXXXX)
TS=$OUT/ts.bin
trap 'rm -f "$OUT/probe" "$OUT/probe.c"' EXIT

hdr() { printf '\n\033[1m%s\033[0m\n' "$*"; }
kv()  { printf '  %-38s %s\n' "$1" "$2"; }

# ── the probe ──────────────────────────────────────────────────────────────
# Two questions need real ioctls, and both are easier to get right from the
# uapi headers than by hand-packing structs.
cat > "$OUT/probe.c" <<'EOF'
#define _GNU_SOURCE
#include <fcntl.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <sys/ioctl.h>
#include <sys/resource.h>
#include <unistd.h>
#include <linux/dvb/dmx.h>
#include <linux/dvb/frontend.h>

/* probe sec  <dmxdev> <pid> <table_id> <timeout_ms>
 *   Does DMX_SET_FILTER return, and does a section ever arrive?
 * probe taps <dmxdev> <max>
 *   How many simultaneous DMX_OUT_TS_TAP filters are accepted?
 * probe full <dmxdev>
 *   Is the whole-mux PID accepted?
 * probe hold <dmxdev> <pid> <secs>
 *   Pin a TS tap open, so another probe can run against a delivered PID.
 * probe delsys <frontend>
 *   Which delivery systems does the frontend enumerate?  FE_GET_INFO does
 *   not say; DTV_ENUM_DELSYS does.
 */
static int do_delsys(const char *dev)
{
	int fd = open(dev, O_RDONLY);
	if (fd < 0) { perror("open"); return 2; }
	struct dtv_property p;
	memset(&p, 0, sizeof(p));
	p.cmd = DTV_ENUM_DELSYS;
	struct dtv_properties props = { .num = 1, .props = &p };
	if (ioctl(fd, FE_GET_PROPERTY, &props) < 0) {
		printf("delsys=error %s\n", strerror(errno));
		close(fd); return 1;
	}
	int isdbt = 0;
	printf("delsys=");
	for (unsigned i = 0; i < p.u.buffer.len; i++) {
		unsigned v = p.u.buffer.data[i];
		printf("%u%s", v, i + 1 < p.u.buffer.len ? "," : "");
		if (v == SYS_ISDBT) isdbt = 1;
	}
	printf("  (%u system%s)\n", p.u.buffer.len, p.u.buffer.len == 1 ? "" : "s");
	printf("isdbt=%s\n", isdbt ? "yes" : "NO");
	close(fd);
	return 0;
}

static int do_sec(const char *dev, int pid, int tid, int timeout_ms)
{
	int fd = open(dev, O_RDONLY | O_NONBLOCK);
	if (fd < 0) { perror("open"); return 2; }

	struct dmx_sct_filter_params p;
	memset(&p, 0, sizeof(p));
	p.pid = pid;
	p.filter.filter[0] = tid;
	p.filter.mask[0] = 0xff;
	p.timeout = 0;
	p.flags = DMX_IMMEDIATE_START | DMX_CHECK_CRC;

	if (ioctl(fd, DMX_SET_FILTER, &p) < 0) {
		printf("ioctl=EXPLICIT_ERROR errno=%s\n", strerror(errno));
		close(fd); return 1;
	}
	printf("ioctl=ok\n");

	struct pollfd pfd = { .fd = fd, .events = POLLIN };
	int r = poll(&pfd, 1, timeout_ms);
	if (r > 0) {
		unsigned char buf[4096];
		ssize_t n = read(fd, buf, sizeof(buf));
		printf("section=%zd bytes\n", n);
	} else if (r == 0) {
		printf("section=TIMEOUT after %dms\n", timeout_ms);
	} else {
		printf("section=poll error %s\n", strerror(errno));
	}
	close(fd);
	return 0;
}

static int set_tap(int fd, int pid)
{
	struct dmx_pes_filter_params f;
	memset(&f, 0, sizeof(f));
	f.pid = pid;
	f.input = DMX_IN_FRONTEND;
	f.output = DMX_OUT_TS_TAP;
	f.pes_type = DMX_PES_OTHER;
	f.flags = DMX_IMMEDIATE_START;
	return ioctl(fd, DMX_SET_PES_FILTER, &f);
}

static int do_taps(const char *dev, int max)
{
	int fds[256], n = 0;
	if (max > 256) max = 256;
	/* One tap is one open fd, so without this the answer is this process's
	 * RLIMIT_NOFILE and not the driver's filter table -- which is how a
	 * previous run reported "32 accepted" and the next one "30". */
	struct rlimit rl;
	if (!getrlimit(RLIMIT_NOFILE, &rl)) {
		rl.rlim_cur = rl.rlim_max;
		setrlimit(RLIMIT_NOFILE, &rl);
	}
	for (n = 0; n < max; n++) {
		fds[n] = open(dev, O_RDWR);
		if (fds[n] < 0) {
			/* EMFILE from a demux device is ambiguous and usually is not
			 * about us: dvb-core returns it from dvb_dmxdev_open() when
			 * every slot in the adapter's filter table is taken, and that
			 * table is shared with whatever else has the adapter open --
			 * our own tune included.  The caller adds the two up. */
			printf("open failed at %d: %s%s\n", n, strerror(errno),
			       errno == EMFILE
				       ? "  (dvb-core's filter table is full, most likely)" : "");
			break;
		}
		if (set_tap(fds[n], 0x100 + n) < 0) {
			printf("filter failed at %d: %s\n", n, strerror(errno));
			close(fds[n]);
			break;
		}
	}
	printf("taps=%d\n", n);
	for (int i = 0; i < n; i++) close(fds[i]);
	return 0;
}

static int do_hold(const char *dev, int pid, int secs)
{
	int fd = open(dev, O_RDWR);
	if (fd < 0) { perror("open"); return 2; }
	if (set_tap(fd, pid) < 0) { printf("hold=fail %s\n", strerror(errno)); close(fd); return 1; }
	printf("hold=0x%04x held for %ds\n", pid, secs);
	fflush(stdout);
	sleep(secs);
	close(fd);
	return 0;
}

static int do_full(const char *dev)
{
	int fd = open(dev, O_RDWR);
	if (fd < 0) { perror("open"); return 2; }
	int rc = set_tap(fd, 0x2000);
	printf("fullmux=%s\n", rc < 0 ? strerror(errno) : "accepted");
	close(fd);
	return 0;
}

int main(int argc, char **argv)
{
	if (argc < 3) { fprintf(stderr, "usage: probe sec|taps|full|hold|delsys <dev> ...\n"); return 2; }
	if (!strcmp(argv[1], "sec"))
		/* strtol with base 0, not atoi: the PIDs are passed as 0x0011 and
		 * atoi() reads that as 0, which silently probes PAT -- the one
		 * configuration DRIVERS.md says not to draw conclusions from. */
		return do_sec(argv[2], strtol(argv[3], 0, 0), strtol(argv[4], 0, 0), atoi(argv[5]));
	if (!strcmp(argv[1], "taps"))
		return do_taps(argv[2], atoi(argv[3]));
	if (!strcmp(argv[1], "full"))
		return do_full(argv[2]);
	if (!strcmp(argv[1], "hold"))
		return do_hold(argv[2], strtol(argv[3], 0, 0), atoi(argv[4]));
	if (!strcmp(argv[1], "delsys"))
		return do_delsys(argv[2]);
	return 2;
}
EOF
if ! cc -O1 -o "$OUT/probe" "$OUT/probe.c" 2>"$OUT/cc.err"; then
	echo "probe would not compile:"; sed 's/^/  /' "$OUT/cc.err"; exit 1
fi

# ── 1. frontend identity ───────────────────────────────────────────────────
hdr "1. frontend identity"
"$DVBR" fe-info -a "$ADAPTER" 2>&1 | sed 's/^/  /' | tee "$OUT/info.txt"
# FE_GET_INFO carries the name and a caps bitfield, not the delivery systems.
"$OUT/probe" delsys "$FE" | sed 's/^/  /' | tee "$OUT/delsys.txt"
grep -q "isdbt=yes" "$OUT/delsys.txt" \
	|| kv "!!" "frontend does not enumerate SYS_ISDBT -- dvbr cannot drive it"

# ── 2, 3. tune ─────────────────────────────────────────────────────────────
hdr "2-3. tune, lock, bytes"
SINCE=$(date '+%Y-%m-%d %H:%M:%S'); sleep 1
t0=$(date +%s.%N)
# dvbr's default log filter is `warn`, so without this the line the loop
# below waits for is never printed and the lock time is never reported.
RUST_LOG=${RUST_LOG:-info} timeout "$SECS" "$DVBR" tune -a "$ADAPTER" -d "$DEMUX" \
	-c "$CHANNELS" "$CHANNEL" -o "$TS" >"$OUT/tune.out" 2>"$OUT/tune.err" &
tunepid=$!

locked=""
for _ in $(seq 1 $((SECS * 2))); do
	if grep -q "locked" "$OUT/tune.err" 2>/dev/null; then
		locked=$(echo "$(date +%s.%N) - $t0" | bc); break
	fi
	kill -0 "$tunepid" 2>/dev/null || break
	sleep 0.5
done
# dvbr reports "locked" once it has the PAT and PMT too, so this is lock
# plus the SI read, not the frontend on its own.
kv "time to lock + PAT/PMT:" "${locked:-not reported} s"

sleep 6
early=$(stat -c %s "$TS" 2>/dev/null || echo 0)
kv "bytes after ~7s:" "$early"

# Everything below measures the driver *while this script's own tune holds the
# frontend*. If that tune never started -- someone else has the adapter, the
# channel is wrong, the aerial is out -- the probes still run and still print
# numbers, and those numbers describe whatever else is tuned. That is exactly
# the confounded configuration this file exists to warn about, so stop here
# instead of publishing it.
if [ "$early" -eq 0 ]; then
	hdr "aborting: this script's own tune delivered nothing"
	if [ -s "$OUT/tune.err" ]; then sed 's/^/  /' "$OUT/tune.err" | head -8; fi
	echo "  The demux probes below would have measured whoever does hold the"
	echo "  adapter. Free it (stop the daemon, or wait out an EPG pass) and"
	echo "  run this again."
	kill "$tunepid" 2>/dev/null
	echo; echo "  evidence: $OUT"
	exit 1
fi

# ── 4, 5, 6. demux, while the frontend is up ───────────────────────────────
hdr "4-6. demux, with the frontend tuned"
# Order is load-bearing, and getting it wrong is how this script produced a
# result that contradicted DRIVERS.md. The section probe is a question about
# what the *service tune* leaves reaching the demux, so it has to be asked
# while that is still the only thing on the device. A full-mux tap (probe 5)
# asks the device to forward every PID, and a driver that does not fully undo
# that on close leaves SI arriving afterwards — at which point probe 6 answers
# "sections are delivered" about a device configuration no ordinary tune
# produces. So: sections first, then the tap count, and the full mux last.
#
# PAT is also the wrong PID to ask about: on smsusb it comes back whenever
# some other feed is already tapping it, and the driver then looks fine. SI is
# what si_reader actually needs, so ask for SI first and read PAT as the
# control.
for spec in "0x0011 0x42 SDT" "0x0012 0x4e EIT" "0x0000 0x00 PAT"; do
	set -- $spec
	echo "  DMX_SET_FILTER on PID $1 ($3), 10s budget:"
	"$OUT/probe" sec "$DMXDEV" "$1" "$2" 10000 | sed 's/^/    /'
done
"$OUT/probe" taps "$DMXDEV" 32 | tee "$OUT/taps.txt" | sed 's/^/  /'
"$OUT/probe" full "$DMXDEV" | sed 's/^/  /'
# Question 4 is "how many taps does the driver allow", and the probe can only
# see the ones it opened itself. The filter table is per adapter and our own
# tune is holding a service's worth of it, so the answer is the sum -- without
# which this reports a different number for every channel, since a service
# with more elementary streams leaves fewer slots.
probe_taps=$(sed -n 's/^taps=//p' "$OUT/taps.txt")
tune_taps=$(sed -n 's/.*started \([0-9]*\) PID taps.*/\1/p' "$OUT/tune.err" | tail -1)
if [ -n "${probe_taps:-}" ] && [ -n "${tune_taps:-}" ]; then
	kv "filter table:" "$((probe_taps + tune_taps)) ($probe_taps here + $tune_taps held by our tune)"
fi

wait "$tunepid" 2>/dev/null
sz=$(stat -c %s "$TS" 2>/dev/null || echo 0)
kv "bytes total in ${SECS}s:" "$sz  ($((sz / 1024 / SECS)) KiB/s)"
if [ -s "$OUT/tune.err" ]; then
	echo "  dvbr said:"; sed 's/^/    /' "$OUT/tune.err" | head -8
fi

# ── 7. what it cost the kernel log ─────────────────────────────────────────
hdr "7. kernel log during the tune"
journalctl -k --since "$SINCE" --no-pager > "$OUT/kmsg.txt" 2>/dev/null
w=$(grep -c "WARNING: CPU" "$OUT/kmsg.txt")
kv "WARNING backtraces:" "$w"
kv "kernel lines:" "$(wc -l < "$OUT/kmsg.txt")"
if [ "$w" -gt 0 ]; then
	echo "  first one:"
	grep -m1 -A5 "WARNING: CPU" "$OUT/kmsg.txt" | sed 's/^/    /'
	echo "  a tuned frontend must not cost anything here. Write it up in DRIVERS.md."
fi

hdr "evidence"
echo "  $OUT"
