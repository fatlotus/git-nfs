# git-nfs: NFS to Git Pack Proxy Server for macOS (Read-Write)

`git-nfs` is a high-performance userspace NFSv3 server written in Rust that allows you to mount, browse, and **modify** the contents of any remote Git repository (including massive repositories like the Linux kernel) natively on macOS without cloning or downloading the full repository.

When you modify files or directories over NFS, `git-nfs` records changes in a local staging store. When you shut down the server (`Ctrl+C`), it automatically rebuilds the Git tree hierarchy, synthesizes a new Git commit, and packages all your modifications into a standard Git Packfile (`pack-<sha>.pack`) and index (`pack-<sha>.idx`), ready for pushing back to the remote repository.

---

## Key Features

1. **Zero-Clone Browsing**:
   - Fetches only lightweight tree metadata (~3MB for the entire Linux kernel) on mount.
   - Files are lazily fetched over Git Wire Protocol v2 only when read or edited.
2. **Full Read-Write Support via NFS**:
   - Modify existing files, create new files, create subdirectories, delete, and rename.
   - Changes are staged in `~/.cache/git-nfs/<repo>/staging/` and immediately visible over NFS.
3. **macOS Finder Friendly**:
   - Intercepts and filters Apple metadata (`.DS_Store`, `._*`, `.Spotlight-V100`) so your Git tree is never polluted with macOS junk.
4. **Automatic Commits & Git Packfile Generation**:
   - Automatically commits changes after 30 seconds of write inactivity and upon unmount (`Ctrl+C`).
   - Dynamically auto-generates informative commit messages based on the set of changed files and directories (no manual commit message needed!).
   - Automatically builds canonical Git trees, synthesizes a commit pointing to the base commit, and writes a valid Git Packfile v2 (`.pack`) and index (`.idx`).
   - Verified with standard `git verify-pack -v`.
5. **Zero Kernel Extensions or Sudo**:
   - Runs purely in userspace on unprivileged localhost ports using native `/sbin/mount_nfs`.

---

## Quick Start

### 1. Build
```bash
cargo build --release
```

### 2. Mount and Edit

#### Option A: GCS Rapid Bucket (Full Remote Git Repository)
```bash
# Mount a Git repository directly hosted on GCS Rapid:
./target/release/git-nfs -m /tmp/linux-kernel gs://git-on-gcs-rapid/linux/
```

#### Option B: Remote Git Repository via HTTPS
```bash
# Mount from GitHub / remote Git server:
./target/release/git-nfs -m /tmp/linux-kernel https://github.com/torvalds/linux.git
```

In another terminal or in Finder:
```bash
# Open in macOS Finder:
open /tmp/linux-kernel

# Or edit in Terminal:
echo "# My custom patch" >> /tmp/linux-kernel/Makefile
echo "Hello from git-nfs" > /tmp/linux-kernel/HELLO.txt
mkdir /tmp/linux-kernel/my_feature && echo "int test() { return 1; }" > /tmp/linux-kernel/my_feature/test.c
```

### 3. Generate Packfile & Publish Commit on Shutdown
Press `Ctrl+C` in the terminal where `git-nfs` is running, or send a termination signal (`kill <pid>` or `kill -INT <pid>`):

```text
Shutting down Git NFS server...
Successfully unmounted /tmp/linux-kernel

Rebuilding packfile from WAL using crash recovery engine...
Rebuilt Git state: New commit 200beb3... with 6 objects (pruned unchanged trees)
Successfully reconstructed Git pack (pack-84dc504...) from WAL
Writing completed commit and packfile to underlying GCS Git repository...
Uploading standard packfile to GCS Rapid: linux/objects/pack/pack-84dc504....pack
Uploading index file to GCS Rapid: linux/objects/pack/pack-84dc504....idx
Successfully updated HEAD and linux/refs/heads/master to commit 200beb3... on GCS Rapid

===============================================================
🎉 Successfully generated and published Git commit from WAL!
===============================================================
📌 New Commit SHA : 200beb3478283f13699959698fd78023db34cc03
📌 Base Commit SHA: c8f1e399d8f374ac45457ff21311ad5640875b66
📦 Packfile       : ./pack-84dc504....pack
📄 Index file     : ./pack-84dc504....idx
📊 Total objects  : 6
☁️  GCS Storage   : Uploaded pack-84dc504....pack & .idx, updated refs on GCS
===============================================================
```

---

## CLI Options

```text
Usage: git-nfs [OPTIONS] [REPO_URL]

Arguments:
  [REPO_URL]  Remote Git repository URL or GCS URI (e.g. gs://git-on-gcs-rapid/linux/) [default: https://github.com/torvalds/linux.git]

Options:
  -m, --mountpoint <MOUNTPOINT>        Local mountpoint path [default: /tmp/git-nfs-mount]
  -b, --branch <BRANCH>                Branch or tag name to browse (defaults to HEAD / default branch)
  -p, --port <PORT>                    Local TCP port to bind the NFS server [default: 20490]
      --repo-bucket <REPO_BUCKET>      Optional explicit GCS bucket for Git repository (e.g. git-on-gcs-rapid)
      --repo-prefix <REPO_PREFIX>      Optional explicit object prefix for Git repository on GCS (e.g. linux/)
      --cache-dir <CACHE_DIR>          Custom blob cache and staging directory
      --output-pack <OUTPUT_PACK>      Custom path for generated changes packfile
      --author <AUTHOR>                Author signature (e.g. "Name <email>") [defaults to git config]
      --no-mount                       Run server only without executing mount_nfs
      --wal-bucket <WAL_BUCKET>        GCS Rapid Bucket for appendable Write-Ahead Logging (defaults to repo bucket for GCS repos)
      --wal-prefix <WAL_PREFIX>        Prefix for WAL objects in GCS Rapid Bucket (defaults to <prefix>wal/)
  -h, --help                           Print help
```

---

## Crash Recovery & GCS Rapid Bucket WAL

`git-nfs` includes built-in crash resilience powered by appendable objects in Google Cloud Storage (GCS) Rapid Buckets:

1. **Write-Ahead Logging (WAL)**:
   - Every filesystem mutation (`create`, `mkdir`, `write`, `truncate`, `remove`, `rename`) is encoded as a Protocol Buffers record (`proto/wal.proto`) and appended to an appendable WAL stream (`0.log`, `1.log`, `2.log`, ...).
   - Each record is durably flushed to the Rapid Bucket with sub-millisecond persistence.
2. **Zombie Server Lockout**:
   - When a server boots up or recovers, it discovers all prior WAL files and calls `.finalize()` on them.
   - This seals the prior logs: if an old, partitioned, or zombie NFS server attempts any further append operations, GCS immediately rejects them with precondition errors.
3. **Automated Crash Recovery on Startup**:
   - If an NFS server crashed midway through a session (before clean unmount/finalization), the next server startup detects uncommitted WALs and replays all mutations against the base Git tree into a new packfile (`pack-<sha>.pack`).
   - The recovered packfile and index are saved locally and uploaded to the Rapid Bucket alongside `<N>.done`, `<N>.pack`, and `<N>.idx`.
4. **Clean Shutdown Exercising Recovery Engine**:
   - On clean unmount/shutdown, `git-nfs` finalizes the active WAL and executes the **exact same crash recovery code** to rebuild the packfile from the log, ensuring the recovery pathway is exercised continuously.
5. **Local Mode / Offline Fallback**:
   - When `--wal-bucket` is omitted, `git-nfs` automatically uses a local filesystem WAL directory in `~/.cache/git-nfs/<repo>/wal/` with identical framing, sequence ordering, and recovery behavior.

---

## Architecture

- **`src/git/gcs_storage.rs`**: Full Git repository backend for Google Cloud Storage (GCS) Rapid buckets; reads commit references, packs, indices, unpacks OFS/REF deltas, and publishes new packfiles and updated refs.
- **`src/git/idx.rs`**: Git Pack Index v2 parser supporting fanout binary lookup, SHA matching, and pack offset calculation.
- **`src/wal/`**: Write-Ahead Logging engine:
  - `src/wal/proto.rs`: Protocol Buffers schema, length-delimited framing, and crash truncation detection.
  - `src/wal/gcs.rs`: GCS Rapid Bucket appendable object streaming, zombie lockout, and remote packfile sync.
  - `src/wal/local.rs`: Local filesystem WAL storage implementation.
  - `src/wal/recovery.rs`: WAL replay engine and packfile generation.
- **`src/staging/`**: Local transactional staging engine for tracking file modifications, creates, truncations, and deletes.
- **`src/git/builder.rs`**: Canonical Git tree rebuilder and commit object synthesizer.
- **`src/git/pack_writer.rs`**: Generator for Git Packfile v2 (`.pack`) and index (`.idx` v2) with CRC32 and fanout tables.
- **`src/git/`**: Git Protocol v2 client, pkt-line codec, packfile decoder, delta resolver, and tree parser.
- **`src/vfs/`**: Dynamic 64-bit inode manager, path resolution, and Apple metadata noise filter.
- **`src/nfs/`**: Read-Write NFSv3 server implementing `nfsserve::vfs::NFSFileSystem` with synchronous WAL mutation logging.
- **`src/mount/`**: Wraps macOS `/sbin/mount_nfs` and `/sbin/umount`.
