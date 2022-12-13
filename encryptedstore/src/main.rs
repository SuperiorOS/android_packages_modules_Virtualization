/*
 * Copyright (C) 2022 The Android Open Source Project
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *      http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

//! `encryptedstore` is a program that (as the name indicates) provides encrypted storage
//! solution in a VM. This is based on dm-crypt & requires the (64 bytes') key & the backing device.
//! It uses dm_rust lib.

use anyhow::{ensure, Context, Result};
use clap::{arg, App};
use dm::crypt::CipherType;
use dm::util;
use log::info;
use std::ffi::CString;
use std::fs::{create_dir_all, OpenOptions};
use std::io::{Error, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const MK2FS_BIN: &str = "/system/bin/mke2fs";
const UNFORMATTED_STORAGE_MAGIC: &str = "UNFORMATTED-STORAGE";

fn main() -> Result<()> {
    android_logger::init_once(
        android_logger::Config::default()
            .with_tag("encryptedstore")
            .with_min_level(log::Level::Info),
    );
    info!("Starting encryptedstore binary");

    let matches = App::new("encryptedstore")
        .args(&[
            arg!(--blkdevice <FILE> "the block device backing the encrypted storage")
                .required(true),
            arg!(--key <KEY> "key (in hex) equivalent to 32 bytes)").required(true),
            arg!(--mountpoint <MOUNTPOINT> "mount point for the storage").required(true),
        ])
        .get_matches();

    let blkdevice = Path::new(matches.value_of("blkdevice").unwrap());
    let key = matches.value_of("key").unwrap();
    let mountpoint = Path::new(matches.value_of("mountpoint").unwrap());
    encryptedstore_init(blkdevice, key, mountpoint).context(format!(
        "Unable to initialize encryptedstore on {:?} & mount at {:?}",
        blkdevice, mountpoint
    ))?;
    Ok(())
}

fn encryptedstore_init(blkdevice: &Path, key: &str, mountpoint: &Path) -> Result<()> {
    ensure!(
        std::fs::metadata(&blkdevice)
            .context(format!("Failed to get metadata of {:?}", blkdevice))?
            .file_type()
            .is_block_device(),
        "The path:{:?} is not of a block device",
        blkdevice
    );

    let needs_formatting =
        needs_formatting(blkdevice).context("Unable to check if formatting is required")?;
    let crypt_device =
        enable_crypt(blkdevice, key, "cryptdev").context("Unable to map crypt device")?;

    // We might need to format it with filesystem if this is a "seen-for-the-first-time" device.
    if needs_formatting {
        info!("Freshly formatting the crypt device");
        format_ext4(&crypt_device)?;
    }
    mount(&crypt_device, mountpoint).context(format!("Unable to mount {:?}", crypt_device))?;
    Ok(())
}

fn enable_crypt(data_device: &Path, key: &str, name: &str) -> Result<PathBuf> {
    let dev_size = util::blkgetsize64(data_device)?;
    let key = hex::decode(key).context("Unable to decode hex key")?;

    // Create the dm-crypt spec
    let target = dm::crypt::DmCryptTargetBuilder::default()
        .data_device(data_device, dev_size)
        .cipher(CipherType::AES256HCTR2)
        .key(&key)
        .build()
        .context("Couldn't build the DMCrypt target")?;
    let dm = dm::DeviceMapper::new()?;
    dm.create_crypt_device(name, &target).context("Failed to create dm-crypt device")
}

// The disk contains UNFORMATTED_STORAGE_MAGIC to indicate we need to format the crypt device.
// This function looks for it, zeroing it, if present.
fn needs_formatting(data_device: &Path) -> Result<bool> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(data_device)
        .with_context(|| format!("Failed to open {:?}", data_device))?;

    let mut buf = [0; UNFORMATTED_STORAGE_MAGIC.len()];
    file.read_exact(&mut buf)?;

    if buf == UNFORMATTED_STORAGE_MAGIC.as_bytes() {
        buf.fill(0);
        file.write_all(&buf)?;
        return Ok(true);
    }
    Ok(false)
}

fn format_ext4(device: &Path) -> Result<()> {
    let mkfs_options = [
        "-j",               // Create appropriate sized journal
        "-O metadata_csum", // Metadata checksum for filesystem integrity
    ];
    let mut cmd = Command::new(MK2FS_BIN);
    let status = cmd
        .args(mkfs_options)
        .arg(device)
        .status()
        .context(format!("failed to execute {}", MK2FS_BIN))?;
    ensure!(status.success(), "mkfs failed with {:?}", status);
    Ok(())
}

fn mount(source: &Path, mountpoint: &Path) -> Result<()> {
    create_dir_all(mountpoint).context(format!("Failed to create {:?}", &mountpoint))?;
    let mount_options = CString::new(
        "fscontext=u:object_r:encryptedstore_fs:s0,context=u:object_r:encryptedstore_file:s0",
    )
    .unwrap();
    let source = CString::new(source.as_os_str().as_bytes())?;
    let mountpoint = CString::new(mountpoint.as_os_str().as_bytes())?;
    let fstype = CString::new("ext4").unwrap();

    let ret = unsafe {
        libc::mount(
            source.as_ptr(),
            mountpoint.as_ptr(),
            fstype.as_ptr(),
            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
            mount_options.as_ptr() as *const std::ffi::c_void,
        )
    };
    if ret < 0 {
        Err(Error::last_os_error()).context("mount failed")
    } else {
        Ok(())
    }
}
