# Mirror the album locally and show only from the mirror

The frame downloads each photo from the NAS when it is time to show it. That makes the slideshow
only as available as the FTP server, and the FTP server is not very available: it stops answering
for an hour at a time when somebody else runs a sync client against the same NAS, and it closes
idle control connections after 300 seconds. The frame is also the only thing in the house that
cannot tolerate this, because it has nothing else to show.

So the album is mirrored to the device, and **the display reads only from the mirror**. The
fetcher's job changes from "get the photo that is due" to "keep the mirror in step with the
server". A disconnect stops being a case the display has to handle, which matters more than it
sounds: a fallback path only used during an outage is a path that is never exercised and therefore
never known to work. One read path, used every frame, is the only version of this that stays
correct.

Three tiers, each rebuildable from the one above it without the network: the **original** is
mirrored from the server; a **1920x1080 render** is derived from the original; a **thumbnail** is
derived for the gallery. Only the originals are precious, so they live in `XDG_DATA_HOME` while
the derived tiers stay in `XDG_CACHE_HOME`, where a cleaner is welcome to delete them. The render
tier exists because decoding is the expensive step, not downloading: measured on this hardware at
this resolution, a photo downloads in about 0.9 s and then takes 1.3-2.3 s to decode and resize,
with a tail past 100 s that exceeds the 40 s interval outright. Pre-rendering moves that cost off
the display path for good and cuts peak memory, which matters on a 417 MB device already using
swap. The whole album is about 0.95 GB against 9.4 GB free, so nothing is evicted to save space
and no cache replacement policy is needed; a cap at half the free space exists only so that an
album that grows tenfold cannot fill the card and take the system down with it.

Files are named after the photo rather than content-addressed, and an index records what is known
about each. **The index is an optimisation and never a single point of failure**: if it cannot be
read it is rebuilt by scanning the directory, because the file names are the photo paths. The same
scan runs at every startup to adopt whatever a power cut left behind, which is how the mirror
converges after an interruption rather than merely hoping to.

Durability is split deliberately, because the device is only ever switched off by cutting mains
power. Photo bytes get `sync_all` before the rename, every time: losing data there means serving a
broken image for ever, which this project has already shipped once. The *directory* entry is
synced once per sync pass rather than per photo: losing a name costs a re-download, which is an
annoyance and not a correctness failure. Writing the index once per pass rather than once per
photo follows the same reasoning and is the only write economy worth having here.

Deletion is the dangerous direction, so it is made hard. A failed listing never deletes anything —
the FTP layer returns an error rather than an empty list, and that is an invariant, not an
accident. Beyond that, a photo must be missing from two successive *successful* listings spanning
three days before its bytes go, and a listing that loses more than a quarter of the album at once
deletes nothing at all and says so loudly. This last guard exists because RFC 3659 gives a client
no way to tell a complete listing from one truncated under load, and a mirror that wipes itself
while the source is sick has destroyed the only copy that was still working.

Time is not trusted. The Pi has no real-time clock, so after a power cut its clock is wrong until
NTP syncs — over the network whose absence this design exists to survive. Ageing anything out
therefore requires a clock that passes a sanity check; while it does not, nothing is deleted.
Staleness shown to the viewer is measured from monotonic time within the boot, and where the
wall clock cannot be trusted the frame says the last sync time is unknown rather than stating a
confident falsehood.

What the viewer sees follows HTTP's own correction: RFC 9111 retired the `Warning` header because
staleness alarms were not surfaced usefully, and kept `Age`. So the frame shows a quantity, not an
alarm — a small corner notice reading how long ago the album last synced, and only once that
exceeds a day, since for this frame being a little stale is the normal and perfectly good state.
The photo is never greyed out and colour alone never carries the message. A full-screen notice is
reserved for consequence rather than duration: the frame has nothing to show, or it needs a human.

The evidence these choices were weighed against -- RFC 3659 on what FTP can and cannot
promise, the POSIX and ext4 durability rules, rsync's deletion safeguards, and the measurements
taken on this device -- is in [docs/research/offline-photo-cache.md](../research/offline-photo-cache.md).

## Consequences

The frame keeps working through an outage of any length, and the NAS sees a listing every six
hours instead of a download every forty seconds. A cold start costs about three minutes of filling
before the album is complete, during which the slideshow runs on whatever has arrived.

An edit that changes a photo's bytes without changing its size goes undetected until something
else about it changes, because FTP offers no checksum and polling `SIZE` for 209 files every pass
is not worth it. Albums are overwhelmingly add-and-delete, so this is accepted.

The mirror can hold a photo the server no longer has, for at least two listings and three days,
and indefinitely while the clock is untrusted. That is the deliberate direction of error: keeping
a stale photo is a cosmetic fault, and deleting the last copy during an outage is not.

There is no flag to turn the mirror off. An opt-out would restore the second, unexercised read
path that this decision exists to remove.

Revisit the render tier if the display resolution changes, since the renders are then wrong and
must be rebuilt — they are derived, so this costs CPU and no network. Revisit the deletion grace
if an album ever turns over fast enough that three days of staleness is visible. Revisit the
clock rule if the device gains a real-time clock, which would make all of it unnecessary.
