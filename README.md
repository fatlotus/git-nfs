# git-nfs: NFS to Git Pack Proxy Server for macOS (Read-Write)

`git-nfs` is a high-performance userspace NFSv3 server written in Rust that allows you to mount, browse, and **modify** the contents of any remote Git repository (including massive repositories like the Linux kernel) natively on macOS without cloning or downloading the full repository.

When you modify files or directories over NFS, `git-nfs` records changes in a local staging store. When you shut down the server (`Ctrl+C`), it automatically rebuilds the Git tree hierarchy, synthesizes a new Git commit, and packages all your modifications into a standard Git Packfile (`changes-<sha>.pack`) and index (`changes-<sha>.idx`), ready for pushing back to the remote repository.

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
4. **Automatic Git Packfile Generation**:
   - On `Ctrl+C`, automatically builds canonical Git trees, synthesizes a commit pointing to the original remote commit, and writes a valid Git Packfile v2 (`.pack`) and index (`.idx`).
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
```bash
# Mount the Linux kernel repository (or any Git repo):
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

### 3. Generate Packfile on Shutdown
Press `Ctrl+C` in the terminal where `git-nfs` is running:

```text
Shutting down Git NFS server...
Successfully unmounted /tmp/linux-kernel

Changes detected! Generating Git commit and packfile...
Rebuilt Git state: New commit cf2d7b1... with 6283 objects
Generated Git packfile: ./changes-cf2d7b1....pack (3,245,617 bytes)

===============================================================
🎉 Successfully generated Git packfile containing your changes!
===============================================================
📌 New Commit SHA : cf2d7b1bc7afa5f57cd7756c3bad93afa7be0387
📌 Base Commit SHA: 40288c9206c17eb66a603262e06a58d300d0f279
📦 Packfile       : ./changes-cf2d7b1....pack
📄 Index file     : ./changes-cf2d7b1....idx
📊 Total objects  : 6283

To inspect your packfile:
  git verify-pack -v ./changes-cf2d7b1....pack
===============================================================
```

---

## CLI Options

```text
Usage: git-nfs [OPTIONS] [REPO_URL]

Arguments:
  [REPO_URL]  Remote Git repository URL [default: https://github.com/torvalds/linux.git]

Options:
  -m, --mountpoint <MOUNTPOINT>        Local mountpoint path [default: /tmp/git-nfs-mount]
  -b, --branch <BRANCH>                Branch or tag name to browse (defaults to HEAD / default branch)
  -p, --port <PORT>                    Local TCP port to bind the NFS server [default: 20490]
      --cache-dir <CACHE_DIR>          Custom blob cache and staging directory
      --output-pack <OUTPUT_PACK>      Custom path for generated changes packfile
      --commit-message <MSG>           Commit message for generated commit [default: "Changes made via git-nfs proxy"]
      --author <AUTHOR>                Author signature (e.g. "Name <email>") [defaults to git config]
      --no-mount                       Run server only without executing mount_nfs
  -h, --help                           Print help
```

---

## Architecture

- **`src/staging/`**: Local transactional staging engine for tracking file modifications, creates, truncations, and deletes.
- **`src/git/builder.rs`**: Canonical Git tree rebuilder and commit object synthesizer.
- **`src/git/pack_writer.rs`**: Generator for Git Packfile v2 (`.pack`) and index (`.idx` v2) with CRC32 and fanout tables.
- **`src/git/`**: Git Protocol v2 client, pkt-line codec, packfile decoder, delta resolver, and tree parser.
- **`src/vfs/`**: Dynamic 64-bit inode manager and Apple metadata noise filter.
- **`src/nfs/`**: Read-Write NFSv3 server implementing `nfsserve::vfs::NFSFileSystem`.
- **`src/mount/`**: Wraps macOS `/sbin/mount_nfs` and `/sbin/umount`.
