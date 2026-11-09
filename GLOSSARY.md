# Photo Frame

A full-screen slideshow that shows photos from a shared album on a server, typically on a
Raspberry Pi wired to a display and left running unattended.

## Language

**Album**:
The set of photos a share link points at, including everything in directories below it.
_Avoid_: Gallery, library, folder

**Share link**:
The address of an album, which also says which backend serves it (`https://` for Synology Photos
or Immich, `ftp://` for an FTP server).
_Avoid_: URL, album link, source

**Display sequence**:
The order the album's photos will be shown in, built from a listing and consumed one photo at a
time. Rebuilt from a fresh listing when the album changes.
_Avoid_: Playlist, queue, slideshow order

**Round**:
One pass through the display sequence, from the first photo to the last. On a large album a round
lasts hours, which is why a photo added part way through needs the sequence rebuilt rather than
waiting for the next one.
_Avoid_: Cycle, loop, iteration

**Stall**:
The frame has stopped changing photos while the process is still alive and still answering
requests. Distinct from a crash, where the process is gone, and from standby, where the display
is deliberately off. Worth its own word because the three look identical from across the room and
want completely different responses.
_Avoid_: Hang, freeze, lock-up

**Standby**:
The display switched off because nobody is there to watch it, with the slideshow paused alongside
it. A frame in standby is working, not stalled.
_Avoid_: Sleep, idle, off

**Web app**:
The page the frame serves on the local network for managing the album it is showing: browsing,
uploading and deleting photos. Only available with an FTP album, since a Synology or Immich share
link offers no way to change what is in it.
_Avoid_: Admin panel, UI, dashboard
