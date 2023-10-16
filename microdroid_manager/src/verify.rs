// Copyright 2023 The Android Open Source Project
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::instance::{ApkData, MicrodroidData, RootHash};
use crate::payload::get_apex_data_from_payload;
use crate::{is_strict_boot, is_verified_boot, write_apex_payload_data, MicrodroidError};
use anyhow::{anyhow, ensure, Context, Result};
use apkmanifest::get_manifest_info;
use apkverify::{get_public_key_der, verify, V4Signature};
use glob::glob;
use itertools::sorted;
use log::{info, warn};
use microdroid_metadata::Metadata;
use rand::Fill;
use rustutils::system_properties;
use std::path::Path;
use std::process::{Child, Command};
use std::str;
use std::time::SystemTime;

pub const DM_MOUNTED_APK_PATH: &str = "/dev/block/mapper/microdroid-apk";

const MAIN_APK_PATH: &str = "/dev/block/by-name/microdroid-apk";
const MAIN_APK_IDSIG_PATH: &str = "/dev/block/by-name/microdroid-apk-idsig";
const MAIN_APK_DEVICE_NAME: &str = "microdroid-apk";
const EXTRA_APK_PATH_PATTERN: &str = "/dev/block/by-name/extra-apk-*";
const EXTRA_IDSIG_PATH_PATTERN: &str = "/dev/block/by-name/extra-idsig-*";

const APKDMVERITY_BIN: &str = "/system/bin/apkdmverity";

/// Verify payload before executing it. For APK payload, Full verification (which is slow) is done
/// when the root_hash values from the idsig file and the instance disk are different. This function
/// returns the verified root hash (for APK payload) and pubkeys (for APEX payloads) that can be
/// saved to the instance disk.
pub fn verify_payload(
    metadata: &Metadata,
    saved_data: Option<&MicrodroidData>,
) -> Result<MicrodroidData> {
    let start_time = SystemTime::now();

    // Verify main APK
    let root_hash_from_idsig = get_apk_root_hash_from_idsig(MAIN_APK_IDSIG_PATH)?;
    let root_hash_trustful =
        saved_data.map(|d| d.apk_data.root_hash_eq(root_hash_from_idsig.as_ref())).unwrap_or(false);

    // If root_hash can be trusted, pass it to apkdmverity so that it uses the passed root_hash
    // instead of the value read from the idsig file.
    let main_apk_argument = {
        ApkDmverityArgument {
            apk: MAIN_APK_PATH,
            idsig: MAIN_APK_IDSIG_PATH,
            name: MAIN_APK_DEVICE_NAME,
            saved_root_hash: if root_hash_trustful {
                Some(root_hash_from_idsig.as_ref())
            } else {
                None
            },
        }
    };
    let mut apkdmverity_arguments = vec![main_apk_argument];

    // Verify extra APKs
    // For now, we can't read the payload config, so glob APKs and idsigs.
    // Later, we'll see if it matches with the payload config.

    // sort globbed paths to match apks (extra-apk-{idx}) and idsigs (extra-idsig-{idx})
    // e.g. "extra-apk-0" corresponds to "extra-idsig-0"
    let extra_apks =
        sorted(glob(EXTRA_APK_PATH_PATTERN)?.collect::<Result<Vec<_>, _>>()?).collect::<Vec<_>>();
    let extra_idsigs =
        sorted(glob(EXTRA_IDSIG_PATH_PATTERN)?.collect::<Result<Vec<_>, _>>()?).collect::<Vec<_>>();
    ensure!(
        extra_apks.len() == extra_idsigs.len(),
        "Extra apks/idsigs mismatch: {} apks but {} idsigs",
        extra_apks.len(),
        extra_idsigs.len()
    );

    let extra_root_hashes_from_idsig: Vec<_> = extra_idsigs
        .iter()
        .map(|idsig| {
            get_apk_root_hash_from_idsig(idsig).expect("Can't find root hash from extra idsig")
        })
        .collect();

    let extra_root_hashes_trustful: Vec<_> = if let Some(data) = saved_data {
        extra_root_hashes_from_idsig
            .iter()
            .enumerate()
            .map(|(i, root_hash)| data.extra_apk_root_hash_eq(i, root_hash))
            .collect()
    } else {
        vec![false; extra_root_hashes_from_idsig.len()]
    };
    let extra_apk_names: Vec<_> =
        (0..extra_apks.len()).map(|i| format!("extra-apk-{}", i)).collect();

    for (i, extra_apk) in extra_apks.iter().enumerate() {
        apkdmverity_arguments.push({
            ApkDmverityArgument {
                apk: extra_apk.to_str().unwrap(),
                idsig: extra_idsigs[i].to_str().unwrap(),
                name: &extra_apk_names[i],
                saved_root_hash: if extra_root_hashes_trustful[i] {
                    Some(&extra_root_hashes_from_idsig[i])
                } else {
                    None
                },
            }
        });
    }

    // Start apkdmverity and wait for the dm-verify block
    let mut apkdmverity_child = run_apkdmverity(&apkdmverity_arguments)?;

    // While waiting for apkdmverity to mount APK, gathers public keys and root digests from
    // APEX payload.
    let apex_data_from_payload = get_apex_data_from_payload(metadata)?;

    // Writing /apex/vm-payload-metadata is to verify that the payload isn't changed.
    // Skip writing it if the debug policy ignoring identity is on
    if is_verified_boot() {
        write_apex_payload_data(saved_data, &apex_data_from_payload)?;
    }

    // Start apexd to activate APEXes
    system_properties::write("ctl.start", "apexd-vm")?;

    // TODO(inseob): add timeout
    apkdmverity_child.wait()?;

    // Do the full verification if the root_hash is un-trustful. This requires the full scanning of
    // the APK file and therefore can be very slow if the APK is large. Note that this step is
    // taken only when the root_hash is un-trustful which can be either when this is the first boot
    // of the VM or APK was updated in the host.
    // TODO(jooyung): consider multithreading to make this faster
    let main_apk_pubkey = get_public_key_from_apk(DM_MOUNTED_APK_PATH, root_hash_trustful)?;
    let extra_apks_data = extra_root_hashes_from_idsig
        .into_iter()
        .enumerate()
        .map(|(i, extra_root_hash)| {
            let mount_path = format!("/dev/block/mapper/{}", &extra_apk_names[i]);
            let apk_pubkey = get_public_key_from_apk(&mount_path, extra_root_hashes_trustful[i])?;
            Ok(ApkData { root_hash: extra_root_hash, pubkey: apk_pubkey })
        })
        .collect::<Result<Vec<_>>>()?;

    info!("payload verification successful. took {:#?}", start_time.elapsed().unwrap());

    // Use the salt from a verified instance, or generate a salt for a new instance.
    let salt = if let Some(saved_data) = saved_data {
        saved_data.salt.clone()
    } else if is_strict_boot() {
        // No need to add more entropy as a previous stage must have used a new, random salt.
        vec![0u8; 64]
    } else {
        let mut salt = vec![0u8; 64];
        salt.as_mut_slice().try_fill(&mut rand::thread_rng())?;
        salt
    };

    // At this point, we can ensure that the root_hash from the idsig file is trusted, either by
    // fully verifying the APK or by comparing it with the saved root_hash.
    Ok(MicrodroidData {
        salt,
        apk_data: ApkData { root_hash: root_hash_from_idsig, pubkey: main_apk_pubkey },
        extra_apks_data,
        apex_data: apex_data_from_payload,
    })
}

fn get_apk_root_hash_from_idsig<P: AsRef<Path>>(idsig_path: P) -> Result<Box<RootHash>> {
    Ok(V4Signature::from_idsig_path(idsig_path)?.hashing_info.raw_root_hash)
}

fn get_public_key_from_apk(apk: &str, root_hash_trustful: bool) -> Result<Box<[u8]>> {
    let current_sdk = get_current_sdk()?;

    let public_key_der = if !root_hash_trustful {
        verify(apk, current_sdk).context(MicrodroidError::PayloadVerificationFailed(format!(
            "failed to verify {}",
            apk
        )))?
    } else {
        get_public_key_der(apk, current_sdk)?
    };

    match get_manifest_info(apk) {
        Ok(manifest_info) => {
            // TODO (b/299591171): Do something with this info
            info!("Manifest info is {manifest_info:?}")
        }
        Err(e) => warn!("Failed to read manifest info from APK: {e:?}"),
    };

    Ok(public_key_der)
}

fn get_current_sdk() -> Result<u32> {
    let current_sdk = system_properties::read("ro.build.version.sdk")?;
    let current_sdk = current_sdk.ok_or_else(|| anyhow!("SDK version missing"))?;
    current_sdk.parse().context("Malformed SDK version")
}

struct ApkDmverityArgument<'a> {
    apk: &'a str,
    idsig: &'a str,
    name: &'a str,
    saved_root_hash: Option<&'a RootHash>,
}

fn run_apkdmverity(args: &[ApkDmverityArgument]) -> Result<Child> {
    let mut cmd = Command::new(APKDMVERITY_BIN);

    for argument in args {
        cmd.arg("--apk").arg(argument.apk).arg(argument.idsig).arg(argument.name);
        if let Some(root_hash) = argument.saved_root_hash {
            cmd.arg(&hex::encode(root_hash));
        } else {
            cmd.arg("none");
        }
    }

    cmd.spawn().context("Spawn apkdmverity")
}
