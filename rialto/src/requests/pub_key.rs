// Copyright 2023, The Android Open Source Project
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Handles the construction of the MACed public key.

use alloc::vec;
use alloc::vec::Vec;
use bssl_avf::{self, ApiName};
use bssl_ffi::EVP_sha256;
use bssl_ffi::HMAC;
use core::result;
use coset::{iana, CborSerializable, CoseKey, CoseMac0, CoseMac0Builder, HeaderBuilder};
use service_vm_comm::RequestProcessingError;

type Result<T> = result::Result<T, RequestProcessingError>;

/// Verifies the MAC of the given public key.
pub fn validate_public_key(maced_public_key: &[u8], hmac_key: &[u8]) -> Result<CoseKey> {
    let cose_mac = CoseMac0::from_slice(maced_public_key)?;
    cose_mac.verify_tag(&[], |tag, data| verify_tag(tag, data, hmac_key))?;
    let payload = cose_mac.payload.ok_or(RequestProcessingError::KeyToSignHasEmptyPayload)?;
    Ok(CoseKey::from_slice(&payload)?)
}

fn verify_tag(tag: &[u8], data: &[u8], hmac_key: &[u8]) -> Result<()> {
    let computed_tag = hmac_sha256(hmac_key, data)?;
    if tag == computed_tag {
        Ok(())
    } else {
        Err(RequestProcessingError::InvalidMac)
    }
}

/// Returns the MACed public key.
pub fn build_maced_public_key(public_key: CoseKey, hmac_key: &[u8]) -> Result<Vec<u8>> {
    const ALGO: iana::Algorithm = iana::Algorithm::HMAC_256_256;

    let external_aad = &[];
    let protected = HeaderBuilder::new().algorithm(ALGO).build();
    let cose_mac = CoseMac0Builder::new()
        .protected(protected)
        .payload(public_key.to_vec()?)
        .try_create_tag(external_aad, |data| hmac_sha256(hmac_key, data))?
        .build();
    Ok(cose_mac.to_vec()?)
}

/// Computes the HMAC using SHA-256 for the given `data` with the given `key`.
fn hmac_sha256(key: &[u8], data: &[u8]) -> bssl_avf::Result<Vec<u8>> {
    const SHA256_HMAC_LEN: usize = 32;

    let mut out = vec![0u8; SHA256_HMAC_LEN];
    let mut out_len = 0;
    // SAFETY: The function shouldn't access any Rust variable and the returned value is accepted
    // as a potentially NULL pointer.
    let digester = unsafe { EVP_sha256() };
    if digester.is_null() {
        return Err(bssl_avf::Error::CallFailed(ApiName::EVP_sha256));
    }
    // SAFETY: Only reads from/writes to the provided slices and supports digester was checked not
    // be NULL.
    let ret = unsafe {
        HMAC(
            digester,
            key.as_ptr() as *const _,
            key.len(),
            data.as_ptr(),
            data.len(),
            out.as_mut_ptr(),
            &mut out_len,
        )
    };
    if !ret.is_null() && out_len == (out.len() as u32) {
        Ok(out)
    } else {
        Err(bssl_avf::Error::CallFailed(ApiName::HMAC))
    }
}
