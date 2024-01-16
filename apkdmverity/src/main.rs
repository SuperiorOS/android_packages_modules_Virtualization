/*
 * Copyright (C) 2021 The Android Open Source Project
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

//! `apkdmverity` is a program that protects a signed APK file using dm-verity. The APK is assumed
//! to be signed using APK signature scheme V4. The idsig file generated by the signing scheme is
//! also used as an input to provide the merkle tree. This program is currently intended to be used
//! to securely mount the APK inside Microdroid. Since the APK is physically stored in the file
//! system managed by the host Android which is assumed to be compromisable, it is important to
//! keep the integrity of the file "inside" Microdroid.

#![cfg_attr(test, allow(unused))]

use anyhow::{bail, Context, Result};
use apkverify::{HashAlgorithm, V4Signature};
use clap::{arg, Arg, ArgAction, Command};
use dm::loopdevice;
use dm::util;
use dm::verity::{DmVerityHashAlgorithm, DmVerityTargetBuilder};
use itertools::Itertools;
use std::fmt::Debug;
use std::fs;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};

#[cfg(not(test))]
fn main() -> Result<()> {
    let matches = clap_command().get_matches();

    let apks = matches.get_many::<String>("apk").unwrap();
    assert!(apks.len() % 4 == 0);

    let verbose = matches.get_flag("verbose");

    for (apk, idsig, name, roothash) in apks.tuples() {
        let roothash = if roothash != "none" {
            Some(hex::decode(roothash).expect("failed to parse roothash"))
        } else {
            None
        };
        let ret = enable_verity(apk, idsig, name, roothash.as_deref())?;
        if verbose {
            println!(
                "data_device: {:?}, hash_device: {:?}, mapper_device: {:?}",
                ret.data_device, ret.hash_device, ret.mapper_device
            );
        }
    }
    Ok(())
}

fn clap_command() -> Command {
    Command::new("apkdmverity")
        .about("Creates a dm-verity block device out of APK signed with APK signature scheme V4.")
        .arg(
            arg!(--apk ...
                "Input APK file, idsig file, name of the block device, and root hash. \
                The APK file must be signed using the APK signature scheme 4. The \
                block device is created at \"/dev/mapper/<name>\".' root_hash is \
                optional; idsig file's root hash will be used if specified as \"none\"."
            )
            .action(ArgAction::Append)
            .value_names(["apk_path", "idsig_path", "name", "root_hash"]),
        )
        .arg(
            Arg::new("verbose")
                .short('v')
                .long("verbose")
                .action(ArgAction::SetTrue)
                .help("Shows verbose output"),
        )
}

struct VerityResult {
    data_device: PathBuf,
    hash_device: PathBuf,
    mapper_device: PathBuf,
}

const BLOCK_SIZE: u64 = 4096;

// Makes a dm-verity block device out of `apk` and its accompanying `idsig` files.
fn enable_verity<P: AsRef<Path> + Debug>(
    apk: P,
    idsig: P,
    name: &str,
    roothash: Option<&[u8]>,
) -> Result<VerityResult> {
    // Attach the apk file to a loop device if the apk file is a regular file. If not (i.e. block
    // device), we only need to get the size and use the block device as it is.
    let (data_device, apk_size) = if fs::metadata(&apk)?.file_type().is_block_device() {
        (apk.as_ref().to_path_buf(), util::blkgetsize64(apk.as_ref())?)
    } else {
        let apk_size = fs::metadata(&apk)?.len();
        if apk_size % BLOCK_SIZE != 0 {
            bail!("The size of {:?} is not multiple of {}.", &apk, BLOCK_SIZE)
        }
        (
            loopdevice::attach(
                &apk, 0, apk_size, /* direct_io */ true, /* writable */ false,
            )
            .context("Failed to attach APK to a loop device")?,
            apk_size,
        )
    };

    // Parse the idsig file to locate the merkle tree in it, then attach the file to a loop device
    // with the offset so that the start of the merkle tree becomes the beginning of the loop
    // device.
    let sig = V4Signature::from_idsig_path(&idsig)?;
    let offset = sig.merkle_tree_offset;
    let size = sig.merkle_tree_size as u64;
    // Due to unknown reason(b/191344832), we can't enable "direct IO" for the IDSIG file (backing
    // the hash). For now we don't use "direct IO" but it seems OK since the IDSIG file is very
    // small and the benefit of direct-IO would be negliable.
    let hash_device = loopdevice::attach(
        &idsig, offset, size, /* direct_io */ false, /* writable */ false,
    )
    .context("Failed to attach idsig to a loop device")?;

    // Build a dm-verity target spec from the information from the idsig file. The apk and the
    // idsig files are used as the data device and the hash device, respectively.
    let target = DmVerityTargetBuilder::default()
        .data_device(&data_device, apk_size)
        .hash_device(&hash_device)
        .root_digest(if let Some(roothash) = roothash {
            roothash
        } else {
            &sig.hashing_info.raw_root_hash
        })
        .hash_algorithm(match sig.hashing_info.hash_algorithm {
            HashAlgorithm::SHA256 => DmVerityHashAlgorithm::SHA256,
        })
        .salt(&sig.hashing_info.salt)
        .build()
        .context(format!("Merkle tree in {:?} is not compatible with dm-verity", &idsig))?;

    // Actually create a dm-verity block device using the spec.
    let dm = dm::DeviceMapper::new()?;
    let mapper_device =
        dm.create_verity_device(name, &target).context("Failed to create dm-verity device")?;

    Ok(VerityResult { data_device, hash_device, mapper_device })
}

#[cfg(test)]
rdroidtest::test_main!();

#[cfg(test)]
mod tests {
    use crate::*;
    use rdroidtest::{ignore_if, rdroidtest};
    use std::fs::{File, OpenOptions};
    use std::io::Write;
    use std::ops::Deref;
    use std::os::unix::fs::FileExt;

    struct TestContext<'a> {
        data_backing_file: &'a Path,
        hash_backing_file: &'a Path,
        result: &'a VerityResult,
    }

    // On Android, skip the test on devices that doesn't have the virt APEX
    // (b/193612136)
    #[cfg(target_os = "android")]
    fn should_skip() -> bool {
        !Path::new("/apex/com.android.virt").exists()
    }
    #[cfg(not(target_os = "android"))]
    fn should_skip() -> bool {
        false
    }

    fn create_block_aligned_file(path: &Path, data: &[u8]) {
        let mut f = File::create(path).unwrap();
        f.write_all(data).unwrap();

        // Add padding so that the size of the file is multiple of 4096.
        let aligned_size = (data.len() as u64 + BLOCK_SIZE - 1) & !(BLOCK_SIZE - 1);
        let padding = aligned_size - data.len() as u64;
        f.write_all(vec![0; padding as usize].as_slice()).unwrap();
    }

    fn prepare_inputs(test_dir: &Path, apk: &[u8], idsig: &[u8]) -> (PathBuf, PathBuf) {
        let apk_path = test_dir.join("test.apk");
        let idsig_path = test_dir.join("test.apk.idsig");
        create_block_aligned_file(&apk_path, apk);
        create_block_aligned_file(&idsig_path, idsig);
        (apk_path, idsig_path)
    }

    fn run_test(apk: &[u8], idsig: &[u8], name: &str, check: fn(TestContext)) {
        run_test_with_hash(apk, idsig, name, None, check);
    }

    fn run_test_with_hash(
        apk: &[u8],
        idsig: &[u8],
        name: &str,
        roothash: Option<&[u8]>,
        check: fn(TestContext),
    ) {
        let test_dir = tempfile::TempDir::new().unwrap();
        let (apk_path, idsig_path) = prepare_inputs(test_dir.path(), apk, idsig);

        // Run the program and register clean-ups.
        let ret = enable_verity(&apk_path, &idsig_path, name, roothash).unwrap();
        let ret = scopeguard::guard(ret, |ret| {
            loopdevice::detach(ret.data_device).unwrap();
            loopdevice::detach(ret.hash_device).unwrap();
            let dm = dm::DeviceMapper::new().unwrap();
            dm.delete_device_deferred(name).unwrap();
        });

        check(TestContext {
            data_backing_file: &apk_path,
            hash_backing_file: &idsig_path,
            result: &ret,
        });
    }

    #[rdroidtest]
    #[ignore_if(should_skip())]
    fn correct_inputs() {
        let apk = include_bytes!("../testdata/test.apk");
        let idsig = include_bytes!("../testdata/test.apk.idsig");
        run_test(apk.as_ref(), idsig.as_ref(), "correct", |ctx| {
            let verity = fs::read(&ctx.result.mapper_device).unwrap();
            let original = fs::read(&ctx.result.data_device).unwrap();
            assert_eq!(verity.len(), original.len()); // fail fast
            assert_eq!(verity.as_slice(), original.as_slice());
        });
    }

    // A single byte change in the APK file causes an IO error
    #[rdroidtest]
    #[ignore_if(should_skip())]
    fn incorrect_apk() {
        let apk = include_bytes!("../testdata/test.apk");
        let idsig = include_bytes!("../testdata/test.apk.idsig");

        let mut modified_apk = Vec::new();
        modified_apk.extend_from_slice(apk);
        if let Some(byte) = modified_apk.get_mut(100) {
            *byte = 1;
        }

        run_test(modified_apk.as_slice(), idsig.as_ref(), "incorrect_apk", |ctx| {
            fs::read(&ctx.result.mapper_device).expect_err("Should fail");
        });
    }

    // A single byte change in the merkle tree also causes an IO error
    #[rdroidtest]
    #[ignore_if(should_skip())]
    fn incorrect_merkle_tree() {
        let apk = include_bytes!("../testdata/test.apk");
        let idsig = include_bytes!("../testdata/test.apk.idsig");

        // Make a single-byte change to the merkle tree
        let offset = V4Signature::from_idsig_path("testdata/test.apk.idsig")
            .unwrap()
            .merkle_tree_offset as usize;

        let mut modified_idsig = Vec::new();
        modified_idsig.extend_from_slice(idsig);
        if let Some(byte) = modified_idsig.get_mut(offset + 10) {
            *byte = 1;
        }

        run_test(apk.as_ref(), modified_idsig.as_slice(), "incorrect_merkle_tree", |ctx| {
            fs::read(&ctx.result.mapper_device).expect_err("Should fail");
        });
    }

    // APK is not altered when the verity device is created, but later modified. IO error should
    // occur when trying to read the data around the modified location. This is the main scenario
    // that we'd like to protect.
    #[rdroidtest]
    #[ignore_if(should_skip())]
    fn tampered_apk() {
        let apk = include_bytes!("../testdata/test.apk");
        let idsig = include_bytes!("../testdata/test.apk.idsig");

        run_test(apk.as_ref(), idsig.as_ref(), "tampered_apk", |ctx| {
            // At this moment, the verity device is created. Then let's change 10 bytes in the
            // backing data file.
            const MODIFIED_OFFSET: u64 = 10000;
            let f = OpenOptions::new().read(true).write(true).open(ctx.data_backing_file).unwrap();
            f.write_at(&[0, 1], MODIFIED_OFFSET).unwrap();

            // Read around the modified location causes an error
            let f = File::open(&ctx.result.mapper_device).unwrap();
            let mut buf = vec![0; 10]; // just read 10 bytes
            f.read_at(&mut buf, MODIFIED_OFFSET).expect_err("Should fail");
        });
    }

    // idsig file is not alread when the verity device is created, but later modified. Unlike to
    // the APK case, this doesn't occur IO error because the merkle tree is already cached.
    #[rdroidtest]
    #[ignore_if(should_skip())]
    fn tampered_idsig() {
        let apk = include_bytes!("../testdata/test.apk");
        let idsig = include_bytes!("../testdata/test.apk.idsig");
        run_test(apk.as_ref(), idsig.as_ref(), "tampered_idsig", |ctx| {
            // Change 10 bytes in the merkle tree.
            let f = OpenOptions::new().read(true).write(true).open(ctx.hash_backing_file).unwrap();
            f.write_at(&[0, 10], 100).unwrap();

            let verity = fs::read(&ctx.result.mapper_device).unwrap();
            let original = fs::read(&ctx.result.data_device).unwrap();
            assert_eq!(verity.len(), original.len());
            assert_eq!(verity.as_slice(), original.as_slice());
        });
    }

    // test if both files are already block devices
    #[rdroidtest]
    #[ignore_if(should_skip())]
    fn inputs_are_block_devices() {
        let apk = include_bytes!("../testdata/test.apk");
        let idsig = include_bytes!("../testdata/test.apk.idsig");

        let test_dir = tempfile::TempDir::new().unwrap();
        let (apk_path, idsig_path) = prepare_inputs(test_dir.path(), apk, idsig);

        // attach the files to loop devices to make them block devices
        let apk_size = fs::metadata(&apk_path).unwrap().len();
        let idsig_size = fs::metadata(&idsig_path).unwrap().len();

        // Note that apk_loop_device is not detatched. This is because, when the apk file is
        // already a block device, `enable_verity` uses the block device as it is. The detatching
        // of the data device is done in the scopeguard for the return value of `enable_verity`
        // below. Only the idsig_loop_device needs detatching.
        let apk_loop_device = loopdevice::attach(
            &apk_path, 0, apk_size, /* direct_io */ true, /* writable */ false,
        )
        .unwrap();
        let idsig_loop_device = scopeguard::guard(
            loopdevice::attach(
                &idsig_path,
                0,
                idsig_size,
                /* direct_io */ false,
                /* writable */ false,
            )
            .unwrap(),
            |dev| loopdevice::detach(dev).unwrap(),
        );

        let name = "loop_as_input";
        // Run the program WITH the loop devices, not the regular files.
        let ret =
            enable_verity(apk_loop_device.deref(), idsig_loop_device.deref(), name, None).unwrap();
        let ret = scopeguard::guard(ret, |ret| {
            loopdevice::detach(ret.data_device).unwrap();
            loopdevice::detach(ret.hash_device).unwrap();
            let dm = dm::DeviceMapper::new().unwrap();
            dm.delete_device_deferred(name).unwrap();
        });

        let verity = fs::read(&ret.mapper_device).unwrap();
        let original = fs::read(&apk_path).unwrap();
        assert_eq!(verity.len(), original.len()); // fail fast
        assert_eq!(verity.as_slice(), original.as_slice());
    }

    // test with custom roothash
    #[rdroidtest]
    #[ignore_if(should_skip())]
    fn correct_custom_roothash() {
        let apk = include_bytes!("../testdata/test.apk");
        let idsig = include_bytes!("../testdata/test.apk.idsig");
        let roothash = V4Signature::from_idsig_path("testdata/test.apk.idsig")
            .unwrap()
            .hashing_info
            .raw_root_hash;
        run_test_with_hash(
            apk.as_ref(),
            idsig.as_ref(),
            "correct_custom_roothash",
            Some(&roothash),
            |ctx| {
                let verity = fs::read(&ctx.result.mapper_device).unwrap();
                let original = fs::read(&ctx.result.data_device).unwrap();
                assert_eq!(verity.len(), original.len()); // fail fast
                assert_eq!(verity.as_slice(), original.as_slice());
            },
        );
    }

    #[rdroidtest]
    fn verify_command() {
        // Check that the command parsing has been configured in a valid way.
        clap_command().debug_assert();
    }
}
