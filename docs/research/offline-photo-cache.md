# Offline-durable photo cache: research note

**Baseline:** branch `main`, commit `6e14f8c` ("Never serve a thumbnail that was never written"),
working tree clean. Written 2026-11-15. No source code was changed for this note.

**Question behind it.** The frame downloads each photo on demand from an FTP server on a Synology
NAS (~209 JPEGs in one directory) over flaky Wi-Fi, on a Raspberry Pi Zero 2 W (417 MB RAM, SD
card, Debian bookworm, glibc 2.36). The source being slow or absent for an hour is normal: today
the NAS accepted the TCP connection instantly and then never sent the greeting, and
`421 Timeout (300 seconds): closing control connection` on idle control connections is routine. We
want a cache of ready-to-show full images so a disconnect is invisible. The device is only ever
switched off by cutting mains power, so durability has to hold at every instant — this repo has
already been bitten once by `fs::write` + `fs::rename` without an fsync leaving zero-length files
that were then served as valid images for months. SD write endurance matters.

**This note does not propose a design.** It collects what the primary sources actually say, flags
where they disagree, and labels folklore. Each section ends with a short "what this means for this
frame".

**Conventions used here.** Normative = a standard or specification that implementations are
required to follow (RFC, POSIX). Documented behaviour = a specific implementation's own docs
(Linux man pages, kernel docs, rsync, SQLite). Folklore = widely repeated with no primary source
found. Where kernel behaviour exceeds what POSIX promises, that is called out explicitly, because
depending on it is a choice rather than a given.

---

## 1. Established designs for a local mirror that reconciles after arbitrary disconnection

### 1.1 rsync-style change detection (metadata quick check, then optionally content)

rsync's default comparison is deliberately cheap. Its man page calls it the *quick check*, which
"looks for files that have changed in size or in last-modified time"
([rsync(1)](https://download.samba.org/pub/rsync/rsync.1)). `--size-only` narrows it to size alone;
`--ignore-times` disables it so everything transfers; `--checksum` (`-c`) widens it to "compare a
checksum for each file that has a matching size", with the man page's own warning that "this can
slow things down significantly". `--modify-window` exists because timestamps are not exact
everywhere: "rsync treats the timestamps as being equal if they differ by no more than the
modify-window value" ([rsync(1) on
man7](https://man7.org/linux/man-pages/man1/rsync.1.html)), and the BUGS section notes that on FAT
destinations "rsync may re-sync unmodified files".

The *delta* algorithm underneath (Tridgell's work) is a separate thing from change detection: the
receiver splits its copy into fixed-size blocks and computes for each "a weak 'rolling' 32-bit
checksum ... and a strong 128-bit MD4 checksum", the sender scans at every byte offset for blocks
matching both, and sends matched-block references plus literal data for the rest, in a single round
trip ([rsync technical report](https://rsync.samba.org/tech_report/node2.html)).

- **Consistency guaranteed:** none on its own. The quick check is a *heuristic*: it detects changes
  that alter size or mtime and silently misses changes that alter neither. `-c` upgrades this to
  "equal unless the checksum collides".
- **Cost here:** the quick check costs one listing and nothing else. `-c` costs a full re-read of
  every remote file, i.e. the entire transfer we are trying to avoid — on FTP there is no
  server-side checksum command in RFC 3659, so "checksum the remote file" means downloading it.
- **Failure modes left open:** same-size, same-mtime edits; clock skew and timestamp granularity;
  and the delta algorithm is irrelevant for JPEGs, which are not incrementally edited — a changed
  photo is a different photo.

### 1.2 Content-addressed storage (CAS)

Git is the canonical worked example: "at the core of Git is a simple key-value data store ... you
can insert any kind of content into a Git repository, for which Git will hand you back a unique key
you can use later to retrieve that content", where the key is "the SHA-1 hash — a checksum of the
content you're storing plus a header", and the object is stored at a path derived from that hash
([Pro Git, Git Internals — Git
Objects](https://git-scm.com/book/en/v2/Git-Internals-Git-Objects)). Immutability and deduplication
are consequences rather than stated features: new content produces a new object and the old key
still resolves.

OSTree applies the same idea to whole filesystem trees for atomic OS upgrades: "a git-like
content-addressed object store", where each deployment is "composed of a set of hardlinks into the
repository", so an upgrade "only costs disk space proportional to the new files, plus some constant
overhead", and OSTree "performs atomic upgrades of complete filesystem trees"
([OSTree introduction](https://ostreedev.github.io/ostree/introduction/)). Note that the OSTree
introduction states the atomicity *goal* and does not document the filesystem-level mechanics of
the swap, so it is a design precedent, not a durability citation.

- **Consistency guaranteed:** a name that resolves resolves to exactly the bytes that hashed to it,
  provided the write was durable. This is the property that would have prevented the zero-length
  thumbnail bug by construction: an empty file does not hash to a photo's digest.
- **Cost here:** one full-content hash per stored photo. BLAKE3 or SHA-256 over a few MB on a Zero
  2 W is well under the JPEG decode already being done per photo, and it is a streaming pass over
  bytes already in hand. Writes are not amplified: the object is written once.
- **Failure modes left open:** CAS says nothing about *which* object is current — you still need a
  mutable pointer (a ref, a manifest), and that pointer is where the crash-consistency problem
  moves to. It also needs garbage collection, and GC on a cache is exactly the eviction problem in
  section 4. Content addressing alone cannot detect that a remote file changed; it can only tell
  you, after downloading, that it did.

### 1.3 Write-ahead logging / journaling

SQLite's WAL is the clearest documented small-scale instance. "The original content is preserved in
the database file and the changes are appended into a separate WAL file. A COMMIT occurs when a
special record indicating a commit is appended to the WAL." Durability is explicitly a knob:
writers "sync the WAL on every transaction commit if PRAGMA synchronous is set to FULL but omit
this sync if PRAGMA synchronous is set to NORMAL", and with NORMAL "transactions are no longer
durable and might rollback following a power failure or hard reset"
([SQLite WAL](https://www.sqlite.org/wal.html)). Moving the log back into the main file is a
checkpoint, which has its own ordering requirement: "The WAL must be synced to persistent storage
prior to moving content from the WAL into the database and the database file must be synced prior
to resetting the WAL."

The costs are documented rather than folklore. WAL "uses many fewer fsync() operations" than the
rollback journal and writes content once rather than twice at commit, but "read performance
deteriorates as the WAL file grows", there is "a tradeoff between average read performance and
average write performance" tuned by checkpoint frequency, and it adds "an additional
quasi-persistent '-wal' file and '-shm' shared memory file".

- **Consistency guaranteed:** atomic, ordered, recoverable transactions over *metadata* — which
  photos exist, their digests, when each was last seen. Recovery is replay, so an interrupted run
  leaves a well-defined state.
- **Cost here:** small. An index of ~209 entries is kilobytes. SQLite in WAL mode with
  `synchronous=FULL` is one fsync per commit; batching a sync pass into one transaction makes that
  one fsync per pass, not per photo.
- **Failure modes left open:** a log protects the index, not the image bytes. Putting multi-megabyte
  JPEGs *through* a log doubles writes (the thing SQLite's rollback journal does and WAL avoids),
  which is the wrong trade on an SD card.

### 1.4 Manifest + atomic swap

The pattern: write all new content under fresh names, then publish by atomically replacing a single
pointer — a manifest file, a symlink, a directory entry. OSTree's deployments under
`/ostree/deploy/$STATEROOT/$CHECKSUM` are this shape ([OSTree
introduction](https://ostreedev.github.io/ostree/introduction/)); so is rsync's `--delay-updates`
("put all updated files into place at end") and its default per-file behaviour of "creating a new
copy of the file and moving it into place", which `--inplace` exists to opt out of
([rsync(1)](https://man7.org/linux/man-pages/man1/rsync.1.html)).

- **Consistency guaranteed:** readers see either the old complete set or the new complete set, never
  a mixture — *if* the swap is a single atomic namespace operation and *if* the content it points
  at was made durable first. Both conditions are exactly section 3's subject.
- **Cost here:** one extra directory entry per file in flight, one fsync of the manifest, one
  rename. Negligible.
- **Failure modes left open:** the window between "content durable" and "pointer durable" is where
  everything goes wrong, and a manifest that is fsynced while the objects it names are not is worse
  than no manifest at all, because it asserts something false. Also: a whole-set swap is a poor fit
  for an incremental cache that fills over hours of flaky connectivity — you want per-photo
  publication, with the manifest describing the set rather than gating it.

### What this means for this frame

The four are not alternatives; they compose. The shape the sources support for this problem is:
content-addressed image blobs (1.2) published one at a time by atomic rename (1.4), with a small
logged or atomically-swapped index (1.3) recording remote name → digest → last-seen, and rsync's
quick check (1.1) as the *only* affordable change detector over FTP. What the sources rule out:
running photo bytes through a write-ahead log (write amplification on SD), and whole-set atomic
swap (incompatible with filling a cache incrementally across a connection that disappears for an
hour). The existing thumbnail cache in `src/web/thumbnails.rs` already implements 1.4 at the
per-file level (`sync_all` then `fs::rename`), which is the right shape — see section 3 for the one
thing it does not do.

---

## 2. Change detection over FTP specifically

### 2.1 What RFC 3659 actually guarantees

[RFC 3659](https://www.rfc-editor.org/rfc/rfc3659.txt) is the owning document for `MDTM`, `SIZE`,
`MLST` and `MLSD`.

**Time format (§2.3).** "Time values are always represented in UTC (GMT), and in the Gregorian
calendar", and "implementations MUST NOT add precision to time-vals where that precision does not
exist". So a second-granularity server must not pad fractional digits — but it also means you may
receive only second granularity.

**`MDTM` (§3).** The RFC itself recommends against relying on it: the `MLST` `modify` fact "is
recommended as a superior alternative" (§3). For a file that exists but cannot be retrieved, a
server may return an error *or* "a time-val with an unspecified value" (§3.1) — i.e. a syntactically
valid timestamp that means nothing. §3 also warns that clocks "are not necessarily synchronised". A
server that supports MDTM "MUST include" it in `FEAT` (§3.3), which makes `FEAT` the way to find
out, and implies MDTM is optional.

**`SIZE` (§4).** This one *is* exact when it answers: it returns the exact octet count that would
be transferred under the current `STRU`/`MODE`/`TYPE`, and "Estimates of the file transfer size
MUST NOT be returned" (§4). Two caveats. First, 550 is not a verdict on transferability: a 550
"MUST NOT be taken by the client as an indication that the file cannot be transferred" (§4.2) —
servers may refuse for cost reasons, since exact calculation "may take considerable effort on the
part of the server-PI" (§4.4). Second, the value is mode-dependent: under `TYPE A` it reflects
CR-LF line endings and can exceed the stored length (§4.4, §5.2). In binary mode, which this frame
sets explicitly (`src/ftp.rs`, `transfer_type(TransferType::Binary)`), that caveat does not apply.

**`MLSD` facts (§7).** Format is rigid — implementations "MUST NOT vary the format", e.g. by adding
spaces or header lines (§7.2) — which is what makes `MLSD` parseable where `LIST` is not. But the
facts themselves are soft:

- `size` "should always reflect the approximate size of the file", "as accurate as the server can
  make it, without going to extraordinary lengths", and clients "must understand this size may not
  be precise"; clients needing exact sizes should use `SIZE` (§7.5.7). **This is the trap:** the
  `size` fact in a listing is explicitly *not* the exact `SIZE` answer.
- `modify` records last content modification, a "change of substance" should alter it, and clients
  "should not assume" a changed value means changed content (§7.5.3).
- No fact set is mandatory. Servers "SHOULD, if conceivably possible, support at least the type,
  perm, size, unique, and modify facts" (§7.5.10) — SHOULD, not MUST. Facts "inappropriate to the
  file or directory being listed" are simply omitted and "This is not an error" (§7.9), so facts
  can vary *per entry* within one listing. The frame already handles this case
  (`parse_entry_reports_no_modification_time_when_mlsd_omits_it` in `src/ftp.rs`).
- Discovery: a server supporting `MLST`/`MLSD` "MUST indicate that this support exists" in `FEAT`,
  and "there is no distinct FEAT output for MLSD" (§7.8) — one `MLST` feature line covers both.
- `OPTS MLST` restricts what comes back: "Facts not included in the 'OPTS MLST' command MUST NOT be
  returned by the server" (§7.9). Useful for trimming a listing, and the RFC warns that "generating
  values for some facts can be possible, but very expensive, for some servers" and clients "should
  use particular caution before requesting any facts not in that set".

**Security considerations (§11)** add a reason servers refuse: "Implementing the SIZE command, and
perhaps some of the facts of the MLSx commands, may impose a considerable load on the server, which
could lead to denial of service attacks." So a NAS already saturated by another user's heavy sync
client is exactly the condition under which these commands get slow or refused.

**There is no ETag equivalent.** The nearest is the `unique` fact, and §11 explicitly warns against
implementing it with a filesystem identifier, so its content is server-defined and not a content
hash. RFC 3659 defines no checksum command. (`XCRC`/`XMD5`/`HASH` exist in the wild but are not in
RFC 3659; the `HASH` draft never became an RFC. Treat any checksum-over-FTP capability as a
per-server bonus to be probed, never assumed.)

### 2.2 What Synology's FTP server does — thin, and the honest answer is "probe it"

**I could not find a primary Synology source that states which FTP daemon DSM runs or what its
`FEAT` response contains.** Synology's own [FTP/FTPS DSM help
page](https://kb.synology.com/en-global/DSM/help/DSM/AdminCenter/file_ftp_setting?version=7) and the
[DSM 7.4 technical specifications](https://www.synology.com/en-us/dsm/7.4/software_spec/dsm) confirm
the service exists but name neither the daemon nor its supported extensions. Community reports
conflict and are a decade old: one Synology community thread asks for `MLSD` support, saying the
server "does not support MLSD command (it only supports LIST command)"
([community.synology.com](https://community.synology.com/enu/forum/17/post/30396)), while a DSM 5.1
forum thread describes a server that *does* advertise MLSD and then drops connections when clients
use it ([forum.synology.com](https://forum.synology.com/enu/viewtopic.php?t=97635)). **Label: the
daemon identity is folklore.** It is widely said to be vsftpd and I found no Synology document
saying so.

That attribution matters, because if it *is* vsftpd, `MLSD` is definitively absent upstream.
vsftpd's own `REFS` file, listing the standards it implements, says of draft-ietf-ftpext-mlst-16
("Standardizes SIZE, MDTM, MLST and MLSD"): "Note that vsftpd has not implemented MLST and MLSD due
to lack of demand from users", adding "Perhaps the client support just isn't there"
([vsftpd REFS](https://raw.githubusercontent.com/openSUSE/vsftpd/master/REFS)).

Against that, this repo's own behaviour is first-hand evidence: `src/ftp.rs` probes `MLSD` once per
connection and falls back to `LIST` on an unsupported-command error, logging "Server does not
support MLSD ... falling back to LIST", and the README documents the consequence ("Servers too old
to support the `MLSD` command report no usable modification times"). **Whichever branch this NAS
takes is already observable in the frame's own logs, and a one-line `FEAT` probe settles it
definitively.** That is the right way to close this gap, and it is cheap:

```
printf 'USER x\r\nPASS y\r\nFEAT\r\nQUIT\r\n' | nc display.local 21   # or via the frame itself
```

The `421 Timeout (300 seconds): closing control connection` the frame already sees is a control-
connection idle timeout, which is consistent with a server configured with a 300-second idle limit;
RFC 959/3659 define no timeouts, as `src/ftp.rs` already notes in a comment. It is not evidence
about `MLSD`.

### 2.3 Fallback when a timestamp cannot be trusted

rsync's documented answer is to stop trusting metadata and compare content: `--checksum` "changes
the way rsync checks if the files have been changed and are in need of a transfer", comparing "a
checksum for each file that has a matching size", with the explicit cost warning; `--ignore-times`
forces transfer of everything; `--size-only` goes the other way and trusts size alone
([rsync(1)](https://download.samba.org/pub/rsync/rsync.1)). And where timestamps are merely
*imprecise* rather than untrustworthy, rsync does not abandon them, it widens the comparison:
`--modify-window` treats timestamps "as being equal if they differ by no more than the
modify-window value", which is the documented remedy for FAT's 2-second granularity.

Note the asymmetry rsync relies on and FTP breaks: rsync's checksum fallback is cheap because both
ends compute locally. Over FTP, "compare content" means "download the content", so the fallback
costs the same as just re-fetching.

### What this means for this frame

Over FTP the affordable change signals are, in descending order of trust: `SIZE` in binary mode
(exact when answered, per §4, but expensive and refusable); the `MLSD` `modify` fact (per-second,
per-entry-optional, explicitly "do not assume changed value means changed content"); the `MLSD`
`size` fact (approximate by specification — do **not** use it as an integrity check on a download);
`LIST` (unparseable reliably, no usable mtime). There is no ETag and no standard checksum.

For a photo *cache*, this is less bad than it looks, because the dominant question is not "did this
photo change?" but "is this photo still there, and do I have it?" — photos in an album are
overwhelmingly append-and-delete, not edit-in-place. A name+size+mtime tuple as the cache key,
re-validated opportunistically, matches what the protocol can actually promise. The one thing that
should be verified locally rather than trusted from the wire is that the *downloaded bytes are a
complete, decodable JPEG* — because the known failure mode here is a truncated transfer (MEMORY
records the NAS double-answering `ABOR`, dropping the session mid-download), and the `size` fact
cannot adjudicate that. Content-addressing (1.2) or a decode check does.

---

## 3. Reaching a consistent state after an interrupted write on POSIX/ext4

This is the section where the difference between "normative" and "what the kernel happens to do"
decides a real bug, so the layers are kept separate.

### 3.1 What POSIX requires (normative, and weaker than people expect)

**`rename()` is atomic with respect to *observers*, and says nothing about power loss.** POSIX's
rationale says "That specification requires that the action of the function be atomic", and the
description says a directory entry named `new` "shall remain visible to other threads throughout
the renaming operation and refer either to the file referred to by new or old before the operation
began" ([POSIX
rename](https://pubs.opengroup.org/onlinepubs/9799919799/functions/rename.html)). That is a
statement about the namespace as seen by concurrent threads. **POSIX says nothing about durability,
synchronization to storage, or fsync in connection with rename.** There is no normative ordering
guarantee between a file's data and a rename that publishes it.

**`fsync()` is weaker than its reputation, normatively.** POSIX says fsync "shall request that all
data for the open file descriptor named by fildes is to be transferred to the storage device", and
that "The nature of the transfer is implementation-defined"
([POSIX fsync](https://pubs.opengroup.org/onlinepubs/9799919799/functions/fsync.html)). The hard
guarantee — forcing queued I/O to "the synchronized I/O completion state" — is conditional on
`_POSIX_SYNCHRONIZED_IO` being defined. Where it is not, the rationale states plainly: "It is
explicitly intended that a null implementation is permitted." On directories, POSIX is permissive
rather than prescriptive: an application that modifies a directory "can invoke fsync() on the
directory to ensure that the directory's entries and file attributes are synchronized", and "For
most applications, synchronizing the directory's entries should not be necessary."

### 3.2 What Linux documents (stronger, and this is what to build on)

**`fsync(2)` is explicit that the directory is a separate act.** "As well as flushing the file data,
fsync() also flushes the metadata information associated with the file", but "Calling fsync() does
not necessarily ensure that the entry in the directory containing the file has also reached disk.
For that an explicit fsync() on a file descriptor for the directory is also needed."
([fsync(2)](https://man7.org/linux/man-pages/man2/fsync.2.html)) `fdatasync()` "does not flush
modified metadata unless that metadata is needed in order to allow a subsequent data retrieval to be
correctly handled" — a size change from a write *does* require that flush, atime/mtime changes do
not. Error reporting: since Linux 4.13, write-back errors "will be reported to all file descriptors
that might have written the data", and for most local filesystems to "all file descriptors that were
open on the file when the error was recorded". **An ignored fsync return value is an ignored data
loss.**

**`rename(2)` repeats POSIX's atomicity and adds nothing about crashes.** "If newpath already
exists, it will be atomically replaced, so that there is no point at which another process
attempting to access newpath will find it missing", and "If newpath exists but the operation fails
for some reason, rename() guarantees to leave an instance of newpath in place"
([rename(2)](https://man7.org/linux/man-pages/man2/rename.2.html)). **The page does not mention
durability, fsync, or power loss at all.** Anyone citing `rename(2)` as a durability guarantee is
citing a page that does not contain one.

### 3.3 What ext4 documents (implementation behaviour, not a standard)

[Kernel ext4 admin guide](https://www.kernel.org/doc/html/latest/admin-guide/ext4.html):

- **`data=ordered` (the default):** "All data are forced directly out to the main file system prior
  to its metadata being committed to the journal." For the replace-via-rename case specifically, the
  doc states "the data blocks of the new file are forced to disk before the `rename()` operation is
  committed." **This is the ordering guarantee people actually rely on, and it exists only in ext4's
  documentation, not in POSIX.** It orders data *before* metadata; it does not say *when* either
  reaches disk.
- **Delayed allocation:** "Defer block allocation until just before ext4 writes out the block(s) in
  question." The `commit` option's note is the sting: "due to delayed allocation even older data can
  be lost on power failure."
- **`auto_da_alloc` (default on):** "ext4 will detect the replace-via-rename and replace-via-truncate
  patterns" and "avoids the 'zero-length' problem that can happen when a system crashes before the
  delayed allocation blocks" are written. The truncate pattern is given as
  `fd = open("foo", O_TRUNC)/write(fd,..)/close(fd)`.
- **`data=writeback`:** "Data ordering is not preserved, data may be written into the main file
  system after its metadata", and "A crash+recovery can cause incorrect data to appear in files
  which were written shortly before the crash" — i.e. the exact bug this repo already fixed, made
  unfixable-by-ordering.
- **`data=journal`** disables delayed allocation and O_DIRECT, and is deprecated with "support will
  be removed in January 2028" — so it is not a durability strategy to adopt now.

**What `auto_da_alloc` does *not* cover, precisely.** It is described as pattern detection that
forces *allocation* so a crash cannot expose an unallocated (zero-length) file where a replacement
was expected. The kernel documentation makes no claim that it makes the new contents *durable* —
there is no statement that the data is on stable storage when rename returns. Ted Ts'o, ext4's
maintainer, draws the same line for the application-level pattern: write temp, fsync, close, rename
gives "atomicity without durability" — after a crash either the old or the new contents will be
present, but you do not get to say which — and "if the new version must survive a crash, it's
necessary to fsync the containing directory" ([Don't fear the
fsync!](https://thunk.org/tytso/blog/2009/03/15/dont-fear-the-fsync/)). He also notes fsync "doesn't
create any extra I/O", it only changes *when* the I/O happens — relevant to section 4's wear
argument. **This is the ext4 maintainer's own commentary, not a specification; it is the best
available statement of intent for a mechanism the kernel docs describe only briefly.**

### 3.4 Where sources disagree, and what is folklore

- **"rename is atomic, therefore write-then-rename is crash-safe."** Folklore, and the most
  consequential one here. Both `rename(2)` and POSIX describe atomicity against concurrent
  *observers*; neither mentions crashes. On ext4 `data=ordered` the ordering happens to hold per the
  kernel docs, but that is an ext4 property, not a POSIX one, and `data=writeback` breaks it by
  documentation.
- **"auto_da_alloc means you don't need fsync."** Folklore. The kernel docs scope it to avoiding the
  zero-length problem for two detected patterns; they do not promise durability, and Ts'o's
  description of the fsync-less pattern as "atomicity without durability" is the closest primary
  statement. It is a safety net against the *worst* outcome, not a substitute for fsync.
- **"fsync guarantees data is on the platter."** Contested by the people who depend on it most.
  SQLite documents that "neither of these interfaces works as advertised on many systems", that
  "Some historical versions of Linux contain versions of fsync() which are no-ops on some
  filesystems", and that "often the IDE disk control lies and says that data has reached oxide while
  it is still held only in the volatile control cache"
  ([SQLite atomic commit §9.2](https://www.sqlite.org/atomiccommit.html)). POSIX permits a null
  implementation outright (§3.1). **Normative position: fsync is a request, and the strength of the
  guarantee is implementation-defined.** On SD cards, with an opaque FTL and a volatile write
  buffer, this is not hypothetical.
- **Directory fsync necessity.** POSIX says it "should not be necessary" for most applications;
  Linux's `fsync(2)` says it *is* needed for the directory entry to reach disk. **These are not
  actually in conflict** — POSIX is describing what most applications need, Linux is describing what
  the guarantee requires — but quoting only POSIX would license skipping it. Linux's statement is the
  operative one here. SQLite does the directory sync: "the directory that contains the super-journal
  is also synced in order to make sure the super-journal file will appear in the directory following
  a power failure" (§5.2) and it "tries to prevent this by opening and syncing the directory
  containing the rollback journal at the same time it syncs the journal file itself" (§9.5).

### 3.5 The rule set, stated precisely

For publishing file `F` with new contents by write-temp-then-rename:

1. `write()` all bytes to temp file `T` in the **same directory** as `F` (rename is only atomic
   within a filesystem, and same-directory keeps it within one).
2. `fsync(T)` — or `fdatasync(T)`, which suffices for a freshly written file only because the size
   change is metadata "needed in order to allow a subsequent data retrieval", per `fsync(2)`. **Check
   the return value.**
3. `close(T)` — close is not a sync; Ts'o quotes `close(2)`: "A successful close does not guarantee
   that the data has been successfully saved to disk."
4. `rename(T, F)` — atomic against readers (POSIX + `rename(2)`); ordered after the data on ext4
   `data=ordered` (kernel docs), not ordered by POSIX.
5. `fsync(dirfd)` on the containing directory — **required** for the *name* `F` to survive power
   loss (`fsync(2)`). Without it you have atomicity without durability (Ts'o); the frame may come
   back to the old name, which for a cache is acceptable, or — if step 2 was skipped — to the new
   name over nothing, which is the bug that was fixed in `6e14f8c`'s lineage.

In Rust: `File::sync_all()` is `fsync`, `File::sync_data()` is `fdatasync`, and a directory is
synced by `File::open(dir)?.sync_all()`. The `tempfile` crate's `NamedTempFile::persist` performs
the rename but **does not** fsync the file or the directory — those remain the caller's job. (Stated
from the API's own semantics; verify against the crate docs before relying on it.)

### What this means for this frame

`src/web/thumbnails.rs` already does steps 1, 2, 4 correctly, with a comment that states the
reasoning exactly right ("A rename is atomic against the data already written, not against data
still in the page cache"). **It does not do step 5** — the containing directory is never fsynced.
For the thumbnail cache that is a defensible trade (a lost name means a re-render, not a corrupt
image), and it is worth being deliberate about rather than accidental. For a full-image cache the
same trade applies and is arguably better: a lost *name* costs a re-download, which is exactly what
the cache exists to avoid but is not a correctness failure, whereas a lost *fsync of the data* is a
correctness failure that serves a broken photo for ever. If the cache is content-addressed, a
missing-name failure is self-correcting on the next sync pass and a corrupt-content failure is
detectable by rehash — which is a second argument for 1.2.

Batching matters more than avoiding fsync: Ts'o's point that fsync "doesn't create any extra I/O"
means one fsync per downloaded photo is not a wear problem (the photo's bytes are being written
either way). One fsync per *index update* across a whole sync pass, rather than per photo, is the
economy worth having.

---

## 4. Cache eviction and sizing for a fixed-size media cache that survives restarts

### 4.1 Policies, as documented by things that ship them

Squid's `cache_replacement_policy` is a compact catalogue of what production caches actually offer
([Squid docs](http://www.squid-cache.org/Doc/config/cache_replacement_policy/)):

- `lru` (default): "Squid's original list based LRU policy."
- `heap GDSF` (Greedy-Dual Size Frequency): "optimizes object hit rate by keeping smaller popular
  objects in cache so it has a better chance of getting a hit", at the cost of byte hit rate.
- `heap LFUDA` (LFU with Dynamic Aging): "keeps popular objects in cache regardless of their size
  and thus optimizes byte hit rate at the expense of hit rate."
- `heap LRU`: LRU on a heap.

The dynamic-aging detail is the generalisable lesson: both frequency-based policies carry an aging
term because pure LFU suffers cache pollution — an object that was popular once stays resident for
ever.

Mapping onto this frame: the access pattern is a slideshow, i.e. **a cyclic scan over a working set
of ~209 items**. This is the pathological case for LRU specifically — if the cache is smaller than
the cycle, LRU evicts exactly the item that will be needed next (sequential flooding). That is a
well-known property of LRU under cyclic access and follows directly from its definition; I did not
find a single canonical primary citation for it in this pass, so treat the *name* as informal but
the reasoning as sound. CLOCK (second-chance) does not fix cyclic flooding either; it is an
approximation of LRU chosen for cheap reference tracking, which matters for page tables and not for
209 files. ARC and LFUDA-style aging address scan resistance.

For a cache of ~209 fixed-identity items, the honest observation is that **eviction policy is nearly
irrelevant if the cache can hold the whole set**, and nearly hopeless if it cannot. 209 JPEGs at a
few MB each is well under 2 GB; an SD card in a photo frame is 16 GB or more. The real sizing
question is a budget cap, not a replacement algorithm — and the right policy for a bounded-budget
slideshow is age/position-aware (keep what the shuffle will reach next, drop what is no longer in
the remote listing) rather than recency-aware.

### 4.2 SD-card wear: what is measured, and what is repeated

**Measured (primary).** Arnd Bergmann's flash-device survey is the best primary measurement
available and is a kernel developer's own analysis ([LWN, *Optimizing Linux with cheap flash
drives*](https://lwn.net/Articles/428584/), with the tool at
[flashbench](https://github.com/bradfa/flashbench)):

- Segment/erase-block size: "The most common size for these segments is 4MB", and the drive never
  erases anything smaller than one segment.
- Open segments are scarce on cheap media: on one SDHC card "alternating between two segments will
  cause garbage collection at every access", measured at about **33 KB/s**; better drives keep "five
  or more segments" open.
- Small writes amplify: a write smaller than the drive's minimum block size "makes it write the same
  block twice, which forces an immediate garbage collection".
- Out-of-order writes on linear-access drives can force copying almost an entire segment per
  misplaced block.
- The FAT area is often special-cased, "enabling small writes to be done efficiently there".
- Alignment: partition tools should default to "a much larger alignment", such as 4 MB.
- Filesystem fit: drives built for linear access are "basically useless for ext3 and most other
  Linux filesystems"; repeatedly rewriting one block "still triggers a full garbage collection,
  which makes these drives a poor fit for an ext3 journal". In a comment Bergmann reports that
  4 MB alignment plus btrfs fixed freezing on one CompactFlash card — **the article does not
  establish this as a general fix**, and I am flagging it as a single anecdote.

Caveat on applicability: this survey is from 2011. Cards shipped since then have larger segments and
more capable controllers, and I found no comparably rigorous recent primary survey. The
*mechanisms* (erase blocks, limited open AUs, read-modify-write amplification) are architectural and
still apply; the *numbers* should not be quoted as current.

**Filesystem choice, ext4 vs f2fs.** The [kernel f2fs
documentation](https://www.kernel.org/doc/html/latest/filesystems/f2fs.html) states the design
intent: NAND devices "are known to have different characteristics from the conventional rotating
disks", so a filesystem "should adapt to the changes"; f2fs "is based on Log-structured File System
(LFS)" and targets LFS's "snowball effect of wandering tree and high cleaning overhead"; it aims to
"Align FS data structures to the operational units in FTL as best efforts" and expects "that the
section size would be the same as the unit size of garbage collection in FTL" — which is precisely
Bergmann's segment. **But note what the documentation does *not* say:** it "never discusses flash
wear, wear leveling, or endurance in comparison with traditional file systems". The only
write-reduction claim is scoped to compression ("reduce data writes to flash disk as much as
possible, resulting in extending disk life time"). **So "f2fs extends SD card life" is not supported
by f2fs's own documentation.** The defensible claim is alignment with FTL erase units and avoidance
of in-place metadata rewrite, which is a plausible mechanism, not a measured endurance result.

**`atime` / `relatime`.** Per [mount(8)](https://man7.org/linux/man-pages/man8/mount.8.html):
`noatime` means "Do not update inode access times on this filesystem" and implies `nodiratime`;
`relatime` updates atime only when the previous atime is older than mtime/ctime; and critically
"Since Linux 2.6.30, the kernel defaults to the behavior provided by this option" — i.e. **relatime
is already the default**. `lazytime` keeps timestamp updates "on the in-memory version of the file
inode", written out only under specific conditions such as "an fsync(2) call or after 24 hours".

**Conclusion on atime: largely folklore for this workload.** "Mount noatime to save your SD card" is
advice calibrated for `strictatime`, which has not been the default since 2.6.30. Under relatime, a
read-only cache file whose mtime never changes gets at most one atime write after each modification,
not one per read. On a slideshow re-reading the same 209 files, that is a handful of inode writes
total, not a per-display write. It costs nothing to set `noatime`, and claiming a meaningful
endurance gain from it is unsupported.

**Write amplification, generally.** The mechanism is documented by Bergmann above: writes smaller
than the device's internal unit, or scattered across more segments than the controller can hold
open, turn into whole-segment read-modify-erase-write cycles. The practical corollaries that follow
from his measurements, rather than from folklore: write each file once, sequentially, in reasonably
large chunks; do not rewrite a file in place to update a small part of it; keep the number of
concurrently-written regions small (one data stream plus one index, not many interleaved); and
prefer append-and-replace over in-place mutation.

### What this means for this frame

Sizing beats eviction. A cap expressed in bytes with a simple, explainable policy — keep everything
currently in the remote listing, evict by "not seen in the listing" first and only then by age — is
better matched to a 209-item slideshow than LRU, which is actively wrong under cyclic access. If a
fixed cap is wanted anyway, LFUDA-style aging or plain age-based eviction is scan-resistant where
LRU is not.

On wear, the decision-relevant findings are: (a) one full-file sequential write per photo is the
cheapest possible shape and is what a download naturally produces; (b) the thing to avoid is
rewriting a *small index* on every photo — batch index updates into one write per sync pass, which
is also what section 3 recommends for fsync; (c) `noatime` is free but not a win worth citing;
(d) switching the frame's filesystem to f2fs is **not** supported by primary evidence as an
endurance improvement and should not be done on that basis; (e) Bergmann's 33 KB/s figure for
two-segment alternation is the reason not to interleave writes to many files at once — a single
in-flight download at a time is the right shape, which is also what the single-threaded fetcher
already does.

---

## 5. Deletion and tombstones in one-way mirrors

### 5.1 rsync's safeguard, which is the citation that matters

rsync's `--delete` documentation contains the rule this whole section is about:

> "If the sending side detects any I/O errors, then the deletion of any files at the destination
> will be automatically disabled."
> — [rsync(1)](https://download.samba.org/pub/rsync/rsync.1)

The man page gives the reason: deletion is disabled as a safeguard against temporary failures (it
cites NFS errors) causing mass deletions. `--ignore-errors` is the explicit opt-out — "Tells
--delete to go ahead and delete files even when there are I/O errors" — and the existence of a flag
whose entire purpose is to disable the safeguard is itself evidence of how deliberate the default
is.

The supporting controls:

- **`--max-delete=NUM`**: "This tells rsync not to delete more than NUM files or directories."
  Exceeding the limit produces a warning and exit code 25. `--max-delete=0` reports extraneous files
  without deleting any — a dry-run mode for deletion specifically.
- **`--delete-after` / `--delete-delay`**: "Request that the file-deletions on the receiving side be
  done after the transfer has completed" and, for `--delete-delay`, compute deletions during the
  transfer but apply them at the end. Deleting last means a transfer that dies halfway has deleted
  nothing.
- **`--delete-during`** is the default delete mode, for compatibility with incremental recursion.
- **`--partial` / `--partial-dir`**: "By default, rsync will delete any partially transferred file
  if the transfer is interrupted" — the inverse safeguard, on the write side: an interrupted
  download must not be mistaken for a complete file. `--partial-dir` keeps partials somewhere that
  is not the destination path, which is the same separation as a `.partial` temp name.

### 5.2 Tombstones, where deletion must be recorded rather than performed

Where a deletion has to propagate through a system that cannot distinguish "deleted" from "not
seen", the established answer is to write a deletion marker and keep it for a grace period.
Cassandra is the canonical documented case: deletes are written as tombstones, and the grace period
is a table property, `gc_grace_seconds`, defaulting to 864000 seconds (ten days), after which a
tombstone may be dropped during compaction ([Apache Cassandra —
tombstones](https://cassandra.apache.org/doc/latest/cassandra/managing/operating/compaction/tombstones.html)).
The failure mode when the grace period is too short is **data resurrection**: a replica that missed
the deletion marker and comes back after the tombstone is gone reintroduces the deleted data as if
it were new. The general principle transfers directly: *absence of evidence is not evidence of
deletion*, and the remedy is to record the deletion explicitly, with a time bound, rather than to
infer it from a listing.

### 5.3 The general pattern

Stated as a rule, supported by the above: **a destructive action may only be taken on a listing that
is known to be complete and error-free.** Operationally that decomposes into:

1. Treat a listing as a transaction: either it completed without error, or it is not a listing at
   all and no deletion may be derived from it. (rsync's I/O-error rule.)
2. Order deletions last, after the additive work has succeeded. (`--delete-after`.)
3. Bound the blast radius with a sanity limit, and fail loudly rather than proceed when it trips.
   (`--max-delete`, exit 25.)
4. Where available, require corroboration over time before deleting — mark absent, delete after the
   item has been missing from N consecutive good listings, which is the tombstone grace period
   turned inside out.
5. Never let a partial transfer occupy the final name. (`--partial-dir`.)

### What this means for this frame

Rule 1 is the whole answer, and it is already half-implemented by accident: the frame's FTP
transport propagates listing errors rather than returning a short list, so a failed `MLSD`/`LIST`
produces an `Err`, not an empty directory. That property is load-bearing for any future eviction
logic and should be stated as an invariant rather than left as an emergent one — the dangerous
refactor is the one that turns "listing failed" into "listing returned nothing".

The hour-long silent NAS makes rule 4 worth the complexity here rather than just rule 1. The
specific hazard: a listing that *succeeds* while the NAS is under load but returns a partial
directory would be indistinguishable from a real deletion of the missing photos. Nothing in RFC 3659
lets a client detect a truncated-but-successful `MLSD` (§7.2's format rigidity constrains each line,
not the completeness of the set). A grace period — a photo must be absent from several successive
successful listings, over a span of days, before its cached copy is evicted — costs a counter and a
timestamp per entry in an index that already exists, and converts an unrecoverable mistake (the
photo is gone from the cache *and* the NAS is unreachable, so the frame has nothing to show) into a
delay. Given that the cache's entire purpose is to be the last copy standing when the source is
away, erring toward keeping bytes is the cheap direction: SD space is abundant (section 4) and a
re-download is not.

Rule 5 has an FTP-specific edge here: MEMORY records that this NAS double-answers `ABOR`, so a
partial download can drop the session mid-file. A transfer that ends early must never be renamed
onto the final name, and the download size should be checked against the `SIZE` reply (exact in
binary mode, §4) — not against the `MLSD` `size` fact, which is approximate by specification
(§7.5.7).

---

## 6. Staleness signalling in offline-first clients

### 6.1 Protocol-level prior art: the field for this was standardized, then removed

HTTP once had an explicit staleness signal and the IETF took it away. [RFC
9111](https://www.rfc-editor.org/rfc/rfc9111.html) obsoletes the `Warning` header field (which
carried warn-code 110, "Response is Stale"), with the stated reason that it "is not widely generated
or surfaced to users" and that its information "can be gleaned from examining other header fields,
such as Age". The `Age` field remains and is mandatory when serving from cache: a cache answering
without validation "MUST generate an Age header field", and "The presence of an Age header field
implies that the response was not generated or validated by the origin server for this request."

Two lessons, both useful. First, **the industry's considered judgement was that a dedicated
"this is stale" flag was not worth maintaining, while an age in seconds was** — the quantity
survived, the alarm did not. Second, serving stale is a *deliberate, bounded* decision:
[RFC 5861](https://www.rfc-editor.org/rfc/rfc5861.html) defines `stale-while-revalidate` (serve
stale immediately while revalidating in the background, "thereby hiding latency ... from clients")
and `stale-if-error` (on an error that would yield 500/502/503/504, "a cached stale response MAY be
used to satisfy the request, regardless of other freshness information"), with the explicit
motivation that this "improves availability". RFC 9111 §4.2.4 confirms the default posture: "A cache
MUST NOT generate a stale response unless it is disconnected or doing so is explicitly permitted".

The frame is permanently in `stale-if-error` mode, by design.

### 6.2 UX guidance

Google's [offline UX design guidelines](https://web.dev/articles/offline-ux-design-guidelines) are
the most directly applicable published guidance:

- "Clearly inform the user when a change in state occurs as soon as possible."
- Make clear "whether a connection problem is on your end" or the user's.
- "Using only color to show state can be hard for users to notice" — pair colour with text or a
  shape.
- "using grayed UI to show that your app is offline can cause confusion" about what still works.
- On timestamps: "Some apps can always show the last time they were updated." Apps needing current
  data should auto-update; apps where stale content is still useful should not interrupt.
- On tone: use reassuring wording and "try not to overwhelm them with too much messaging."
- Mechanism suggestions are lightweight: a "widget that provides brief feedback" at the bottom of
  the screen, or "a visual cue like a Material Design toast element" for background updates.

Note what it does *not* do: **it never compares a banner with a subtle indicator, and gives no
threshold for escalating between them.** That is the question we most wanted answered and the source
does not answer it.

Nielsen Norman Group owns the underlying heuristic. [Visibility of system
status](https://www.nngroup.com/articles/visibility-system-status/): a system "should always keep
users informed about what is going on"; "no action with consequences to users should be taken
without informing them"; "A lack of information often equates to a lack of control"; feedback
should arrive "within reasonable time". On intrusiveness the article is directional rather than
quantitative: lighter feedback (a colour change, a progress indicator) is usually enough,
notifications or indicators suit backstage events, and modal dialogs are reserved for "state changes
that can significantly affect them".

The nearest thing to published thresholds is NN/g's [response-time
limits](https://www.nngroup.com/articles/response-times-3-important-limits/) — 0.1 s (feels
instantaneous, no feedback needed), 1.0 s (flow of thought uninterrupted; beyond this, show the
system is working), 10 s (attention limit; show a percent-done indicator and a clearly marked way to
interrupt). **These are limits for a user *waiting on an operation they initiated*, and the frame's
viewer initiates nothing and waits for nothing.** Applying them to a slideshow's sync staleness is a
category error, and they are cited here to say so.

### 6.3 Where the sources are thin

**This is the weakest of the six questions.** I found no primary source giving a threshold at which
a passive staleness indicator stops being sufficient. The guidance that exists is qualitative
(inform promptly, do not alarm, reserve modals for significant consequences), and the one place the
industry made a formal decision about staleness signalling, it chose to publish a *quantity* (`Age`)
and retire the *warning* (RFC 9111). Anything more specific — "warn after 24 hours", "escalate at
three failed syncs" — that I could offer would be invention dressed as research, so I am not
offering it.

What can be said from the sources with a straight face: the escalation trigger that the sources do
support is not elapsed time at all, but **consequence**. NN/g's line is that modals are for state
changes that "can significantly affect" the user, and that "no action with consequences to users
should be taken without informing them". For this frame, the consequential state is not "the sync is
old" — nobody is harmed by seeing a photo fetched yesterday — it is "the frame can no longer show
you new photos, and you may need to do something about it" (NAS down, credentials stale, Wi-Fi gone).
That is the thing worth escalating for, and it is a *diagnosis*, not a duration.

### What this means for this frame

The preference for a small corner overlay is well supported and the fallback full-screen notice
should be rare. Concretely, from the sources:

- Show a quantity, not an alarm: "last synced 3h ago" follows `Age`'s survival over `Warning`'s
  retirement, and web.dev's "Some apps can always show the last time they were updated". A quiet,
  always-present age indicator is defensible as the *normal* state, since for this frame stale is
  normal.
- Do not signal with colour alone and do not grey out the image — both are named failure modes in
  web.dev's guidance, and greying a photo is precisely the "confusion about what still works"
  problem.
- Reserve the full-screen notice for the case where the frame has nothing to show or cannot recover
  without a human (empty cache, authentication failure) — the "significantly affect them" bar from
  NN/g — rather than for a duration threshold, which no source supports.
- This sits well with the existing ADR
  ([0001](../adr/0001-report-hangs-rather-than-restarting.md)): the frame's documented stance is to
  make a stuck state *visible and diagnosable* rather than to paper over it. A staleness indicator
  that distinguishes "cached, source quiet" (expected, quiet) from "cached, source rejecting us"
  (actionable, loud) is the same principle applied to the viewer rather than to the operator, and
  `/api/status` already carries the information needed to tell them apart.

---

## Summary: where the sources are solid, thin, or contradictory

| Q | State of evidence |
|---|---|
| 1 Mirror designs | **Solid.** Git, OSTree, SQLite, rsync all document their own mechanisms. Costs on a Zero 2 W are inferred by arithmetic, not measured. |
| 2 FTP change detection | **RFC side solid, Synology side thin.** RFC 3659 is unambiguous and more pessimistic than usually assumed. Synology's daemon identity and `FEAT` content are unconfirmed by any Synology document — resolve by probing the NAS. |
| 3 fsync / rename / ext4 | **Solid, and the layers disagree in an important way.** POSIX guarantees far less than ext4 delivers; relying on ext4's `data=ordered` ordering is a choice. Several widely repeated beliefs are folklore. |
| 4 Eviction and SD wear | **Policies solid, wear partly dated, f2fs claim unsupported.** Bergmann's measurements are primary but from 2011. "f2fs extends card life" and "noatime saves your card" are not supported by the primary sources. |
| 5 Deletion safeguards | **Solid.** rsync's man page states the rule verbatim; Cassandra documents the grace-period pattern. |
| 6 Staleness signalling | **Thinnest.** No primary source gives an escalation threshold. The strongest finding is a negative one: HTTP retired its staleness *warning* and kept its staleness *quantity*. |

---

## Addendum: what this NAS actually supports (measured, not inferred)

Section 2 flagged Synology's FTP capabilities as the thinnest factual gap, with the "it is vsftpd,
so no MLSD" attribution labelled folklore. That is now settled by asking the server, on
2026-11-15, with an unauthenticated `FEAT` against `192.168.178.24:21`:

```
220 nas FTP server ready.
211- Extensions supported:
 AUTH TLS, PBSZ, PROT, CCC
 SIZE
 MDTM
 REST STREAM
 MFMT
 TVFS
 MLST modify*;type*;unique*;size*;UNIX.mode*;UNIX.owner*;UNIX.group*;
 MLSD modify*;type*;unique*;size*;UNIX.mode*;UNIX.owner*;UNIX.group*;
 UTF8
211 End.
```

**The folklore was wrong for this server.** `MLSD` is supported, so the design is not forced onto
`LIST` parsing. Four consequences for the sections above:

- **`unique` is advertised.** RFC 3659 §7.5.2 defines it as an identifier that is the same for two
  pathnames if and only if they refer to the same underlying file, and that must change when the
  contents change in a way the server knows about. That is the closest thing to an ETag available
  here, and section 2 concluded none existed. It is still weaker than a digest -- the RFC makes no
  promise it changes on every content edit -- so it strengthens change *detection* without
  replacing local integrity verification.
- **`modify` and `size` are present as facts**, with the caveats section 2 established: `size` here
  is the MLSX approximate size and must not be used as an integrity check, while the separate
  `SIZE` command is exact in binary mode.
- **`REST STREAM` is supported**, so an interrupted download can be resumed from an offset rather
  than restarted. That bears directly on the known `ABOR` truncation behaviour of this NAS
  recorded elsewhere in this repo, and on transfers over a flaky link.
- **`AUTH TLS` is supported**, which is orthogonal to caching but worth knowing: the frame's own
  startup warning about an unencrypted web app has a counterpart here, and FTPS is available
  without a server change.

What remains unverified is whether the `modify` values are *trustworthy* under this server's load,
which no `FEAT` output can answer; section 2's argument for verifying integrity locally stands
unchanged.
