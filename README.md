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
Press `Ctrl+C` in the terminal where `git-nfs` is running, or send a termination signal (`kill <pid>` or `kill -INT <pid>`):

```text
Shutting down Git NFS server...
Successfully unmounted /tmp/linux-kernel

Changes detected! Generating Git commit and packfile...
Rebuilt Git state: New commit c8f1e39... with 3 objects (pruned unchanged trees)
Generated Git packfile: ./changes-c8f1e39....pack (3,475 bytes) and index: ./changes-c8f1e39....idx

===============================================================
🎉 Successfully generated Git packfile containing your changes!
===============================================================
📌 New Commit SHA : c8f1e399d8f374ac45457ff21311ad5640875b66
📌 Base Commit SHA: 518e5b794c06c0f0eb40df3e202274a66202c137
📦 Packfile       : ./changes-c8f1e39....pack
📄 Index file     : ./changes-c8f1e39....idx
📊 Total objects  : 3

To inspect your packfile:
  git verify-pack -v ./changes-c8f1e39....pack
===============================================================
```

### 4. Import Changes into a Local Git Repository
To use or inspect your newly generated commit inside a local clone of the repository:

```bash
# 1. Copy the generated packfile and index into your repository's objects/pack directory:
cp changes-<sha>.pack changes-<sha>.idx /path/to/repo/.git/objects/pack/

# 2. Checkout the newly synthesized commit:
git -C /path/to/repo checkout <sha>

# 3. View your modifications:
git -C /path/to/repo show HEAD
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
