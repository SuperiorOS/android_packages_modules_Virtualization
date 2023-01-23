/*
 * Copyright (C) 2023 The Android Open Source Project
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

//! Utility functions used by API tests.

use anyhow::Result;
use avb_bindgen::{
    avb_footer_validate_and_byteswap, avb_vbmeta_image_header_to_host_byte_order, AvbFooter,
    AvbVBMetaImageHeader,
};
use pvmfw_avb::{verify_payload, AvbSlotVerifyError, DebugLevel};
use std::{
    fs,
    mem::{size_of, transmute, MaybeUninit},
};

const MICRODROID_KERNEL_IMG_PATH: &str = "microdroid_kernel";
const INITRD_NORMAL_IMG_PATH: &str = "microdroid_initrd_normal.img";
const INITRD_DEBUG_IMG_PATH: &str = "microdroid_initrd_debuggable.img";
const PUBLIC_KEY_RSA4096_PATH: &str = "data/testkey_rsa4096_pub.bin";

pub const PUBLIC_KEY_RSA2048_PATH: &str = "data/testkey_rsa2048_pub.bin";

pub fn assert_payload_verification_with_initrd_eq(
    kernel: &[u8],
    initrd: &[u8],
    trusted_public_key: &[u8],
    expected_result: Result<DebugLevel, AvbSlotVerifyError>,
) -> Result<()> {
    assert_payload_verification_eq(kernel, Some(initrd), trusted_public_key, expected_result)
}

pub fn assert_payload_verification_eq(
    kernel: &[u8],
    initrd: Option<&[u8]>,
    trusted_public_key: &[u8],
    expected_result: Result<DebugLevel, AvbSlotVerifyError>,
) -> Result<()> {
    assert_eq!(expected_result, verify_payload(kernel, initrd, trusted_public_key));
    Ok(())
}

pub fn load_latest_signed_kernel() -> Result<Vec<u8>> {
    Ok(fs::read(MICRODROID_KERNEL_IMG_PATH)?)
}

pub fn load_latest_initrd_normal() -> Result<Vec<u8>> {
    Ok(fs::read(INITRD_NORMAL_IMG_PATH)?)
}

pub fn load_latest_initrd_debug() -> Result<Vec<u8>> {
    Ok(fs::read(INITRD_DEBUG_IMG_PATH)?)
}

pub fn load_trusted_public_key() -> Result<Vec<u8>> {
    Ok(fs::read(PUBLIC_KEY_RSA4096_PATH)?)
}

pub fn extract_avb_footer(kernel: &[u8]) -> Result<AvbFooter> {
    let footer_start = kernel.len() - size_of::<AvbFooter>();
    // SAFETY: The slice is the same size as the struct which only contains simple data types.
    let mut footer = unsafe {
        transmute::<[u8; size_of::<AvbFooter>()], AvbFooter>(kernel[footer_start..].try_into()?)
    };
    // SAFETY: The function updates the struct in-place.
    unsafe {
        avb_footer_validate_and_byteswap(&footer, &mut footer);
    }
    Ok(footer)
}

pub fn extract_vbmeta_header(kernel: &[u8], footer: &AvbFooter) -> Result<AvbVBMetaImageHeader> {
    let vbmeta_offset: usize = footer.vbmeta_offset.try_into()?;
    let vbmeta_size: usize = footer.vbmeta_size.try_into()?;
    let vbmeta_src = &kernel[vbmeta_offset..(vbmeta_offset + vbmeta_size)];
    // SAFETY: The latest kernel has a valid VBMeta header at the position specified in footer.
    let vbmeta_header = unsafe {
        let mut header = MaybeUninit::uninit();
        let src = vbmeta_src.as_ptr() as *const _ as *const AvbVBMetaImageHeader;
        avb_vbmeta_image_header_to_host_byte_order(src, header.as_mut_ptr());
        header.assume_init()
    };
    Ok(vbmeta_header)
}
