use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{anyhow, Context, Result};
use tracing::{info, warn};

pub struct NfsMounter {
    mountpoint: String,
    unmounted: AtomicBool,
}

impl NfsMounter {
    pub fn mount(ip: &str, port: u16, mountpoint: &Path) -> Result<Self> {
        let mount_str = mountpoint.to_string_lossy().to_string();

        // Ensure mount directory exists
        if !mountpoint.exists() {
            std::fs::create_dir_all(mountpoint)
                .with_context(|| format!("Creating mount directory {mount_str}"))?;
        }

        // On macOS: use mount_nfs
        #[cfg(target_os = "macos")]
        {
            let options = format!(
                "rw,tcp,noatime,nolocks,locallocks,port={port},mountport={port}"
            );
            let share = format!("{ip}:/");

            info!("Executing: /sbin/mount_nfs -o {options} {share} {mount_str}");
            let status = Command::new("/sbin/mount_nfs")
                .arg("-o")
                .arg(&options)
                .arg(&share)
                .arg(&mount_str)
                .status()
                .context("Executing /sbin/mount_nfs")?;

            if !status.success() {
                return Err(anyhow!("mount_nfs failed with exit code: {:?}", status.code()));
            }
        }

        #[cfg(not(target_os = "macos"))]
        {
            let options = format!("ro,tcp,port={port},mountport={port},nolock");
            let share = format!("{ip}:/");
            let status = Command::new("mount")
                .arg("-t")
                .arg("nfs")
                .arg("-o")
                .arg(&options)
                .arg(&share)
                .arg(&mount_str)
                .status()
                .context("Executing mount")?;

            if !status.success() {
                return Err(anyhow!("mount failed with exit code: {:?}", status.code()));
            }
        }

        info!("Successfully mounted repository to {mount_str}");
        Ok(Self {
            mountpoint: mount_str,
            unmounted: AtomicBool::new(false),
        })
    }

    pub fn unmount(&self) {
        if self.unmounted.swap(true, Ordering::SeqCst) {
            return;
        }
        info!("Unmounting {}...", self.mountpoint);
        #[cfg(target_os = "macos")]
        let cmd = "/sbin/umount";
        #[cfg(not(target_os = "macos"))]
        let cmd = "umount";

        match Command::new(cmd).arg(&self.mountpoint).status() {
            Ok(s) if s.success() => {
                info!("Successfully unmounted {}", self.mountpoint);
            }
            Ok(s) => {
                warn!("Unmount command returned non-zero exit code: {:?}", s.code());
            }
            Err(e) => {
                warn!("Failed to invoke unmount: {e}");
            }
        }
    }
}

impl Drop for NfsMounter {
    fn drop(&mut self) {
        self.unmount();
    }
}
