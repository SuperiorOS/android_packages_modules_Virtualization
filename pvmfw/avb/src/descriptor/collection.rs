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

//! Structs and functions relating to the descriptor collection.

use super::common::get_valid_descriptor;
use super::hash::HashDescriptor;
use super::property::PropertyDescriptor;
use crate::error::{AvbIOError, AvbSlotVerifyError};
use crate::partition::PartitionName;
use crate::utils::{self, is_not_null, to_usize, usize_checked_add};
use avb_bindgen::{
    avb_descriptor_foreach, avb_descriptor_validate_and_byteswap, AvbDescriptor, AvbDescriptorTag,
    AvbVBMetaData,
};
use core::{ffi::c_void, mem::size_of, slice};
use tinyvec::ArrayVec;

/// `Descriptors` can have at most one `HashDescriptor` per known partition and at most one
/// `PropertyDescriptor`.
#[derive(Default)]
pub(crate) struct Descriptors<'a> {
    hash_descriptors: ArrayVec<[HashDescriptor<'a>; PartitionName::NUM_OF_KNOWN_PARTITIONS]>,
    prop_descriptor: Option<PropertyDescriptor<'a>>,
}

impl<'a> Descriptors<'a> {
    /// Builds `Descriptors` from `AvbVBMetaData`.
    /// Returns an error if the given `AvbVBMetaData` contains non-hash descriptor, hash
    /// descriptor of unknown `PartitionName` or duplicated hash descriptors.
    ///
    /// # Safety
    ///
    /// Behavior is undefined if any of the following conditions are violated:
    /// * `vbmeta.vbmeta_data` must be non-null and points to a valid VBMeta.
    /// * `vbmeta.vbmeta_data` must be valid for reading `vbmeta.vbmeta_size` bytes.
    pub(crate) unsafe fn from_vbmeta(vbmeta: AvbVBMetaData) -> Result<Self, AvbSlotVerifyError> {
        is_not_null(vbmeta.vbmeta_data).map_err(|_| AvbSlotVerifyError::Io)?;
        let mut res: Result<Self, AvbIOError> = Ok(Self::default());
        // SAFETY: It is safe as the raw pointer `vbmeta.vbmeta_data` is a non-null pointer and
        // points to a valid VBMeta structure.
        let output = unsafe {
            avb_descriptor_foreach(
                vbmeta.vbmeta_data,
                vbmeta.vbmeta_size,
                Some(check_and_save_descriptor),
                &mut res as *mut _ as *mut c_void,
            )
        };
        if output == res.is_ok() {
            res.map_err(AvbSlotVerifyError::InvalidDescriptors)
        } else {
            Err(AvbSlotVerifyError::InvalidMetadata)
        }
    }

    pub(crate) fn num_hash_descriptor(&self) -> usize {
        self.hash_descriptors.len()
    }

    /// Finds the `HashDescriptor` for the given `PartitionName`.
    /// Throws an error if no corresponding descriptor found.
    pub(crate) fn find_hash_descriptor(
        &self,
        partition_name: PartitionName,
    ) -> Result<&HashDescriptor, AvbSlotVerifyError> {
        self.hash_descriptors
            .iter()
            .find(|d| d.partition_name == partition_name)
            .ok_or(AvbSlotVerifyError::InvalidMetadata)
    }

    pub(crate) fn has_property_descriptor(&self) -> bool {
        self.prop_descriptor.is_some()
    }

    pub(crate) fn find_property_value(&self, key: &[u8]) -> Option<&[u8]> {
        self.prop_descriptor.as_ref().filter(|desc| desc.key == key).map(|desc| desc.value)
    }

    fn push(&mut self, descriptor: Descriptor<'a>) -> utils::Result<()> {
        match descriptor {
            Descriptor::Hash(d) => self.push_hash_descriptor(d),
            Descriptor::Property(d) => self.push_property_descriptor(d),
        }
    }

    fn push_hash_descriptor(&mut self, descriptor: HashDescriptor<'a>) -> utils::Result<()> {
        if self.hash_descriptors.iter().any(|d| d.partition_name == descriptor.partition_name) {
            return Err(AvbIOError::Io);
        }
        self.hash_descriptors.push(descriptor);
        Ok(())
    }

    fn push_property_descriptor(
        &mut self,
        descriptor: PropertyDescriptor<'a>,
    ) -> utils::Result<()> {
        if self.prop_descriptor.is_some() {
            return Err(AvbIOError::Io);
        }
        self.prop_descriptor.replace(descriptor);
        Ok(())
    }
}

/// # Safety
///
/// Behavior is undefined if any of the following conditions are violated:
/// * The `descriptor` pointer must be non-null and points to a valid `AvbDescriptor` struct.
/// * The `user_data` pointer must be non-null, points to a valid `Result<Descriptors, AvbIOError>`
///  struct and is initialized.
unsafe extern "C" fn check_and_save_descriptor(
    descriptor: *const AvbDescriptor,
    user_data: *mut c_void,
) -> bool {
    // SAFETY: It is safe because the caller ensures that `user_data` points to a valid struct and
    // is initialized.
    let Some(res) = (unsafe { (user_data as *mut Result<Descriptors, AvbIOError>).as_mut() }) else {
        return false;
    };
    let Ok(descriptors) = res else {
        return false;
    };
    // SAFETY: It is safe because the caller ensures that the `descriptor` pointer is non-null
    // and valid.
    unsafe { try_check_and_save_descriptor(descriptor, descriptors) }.map_or_else(
        |e| {
            *res = Err(e);
            false
        },
        |_| true,
    )
}

/// # Safety
///
/// Behavior is undefined if any of the following conditions are violated:
/// * The `descriptor` pointer must be non-null and points to a valid `AvbDescriptor` struct.
unsafe fn try_check_and_save_descriptor(
    descriptor: *const AvbDescriptor,
    descriptors: &mut Descriptors,
) -> utils::Result<()> {
    // SAFETY: It is safe because the caller ensures that `descriptor` is a non-null pointer
    // pointing to a valid struct.
    let descriptor = unsafe { Descriptor::from_descriptor_ptr(descriptor)? };
    descriptors.push(descriptor)
}

enum Descriptor<'a> {
    Hash(HashDescriptor<'a>),
    Property(PropertyDescriptor<'a>),
}

impl<'a> Descriptor<'a> {
    /// # Safety
    ///
    /// Behavior is undefined if any of the following conditions are violated:
    /// * The `descriptor` pointer must be non-null and point to a valid `AvbDescriptor`.
    unsafe fn from_descriptor_ptr(descriptor: *const AvbDescriptor) -> utils::Result<Self> {
        // SAFETY: It is safe as the raw pointer `descriptor` is non-null and points to
        // a valid `AvbDescriptor`.
        let avb_descriptor =
            unsafe { get_valid_descriptor(descriptor, avb_descriptor_validate_and_byteswap)? };
        let len = usize_checked_add(
            size_of::<AvbDescriptor>(),
            to_usize(avb_descriptor.num_bytes_following)?,
        )?;
        // SAFETY: It is safe because the caller ensures that `descriptor` is a non-null pointer
        // pointing to a valid struct.
        let data = unsafe { slice::from_raw_parts(descriptor as *const u8, len) };
        match avb_descriptor.tag.try_into() {
            Ok(AvbDescriptorTag::AVB_DESCRIPTOR_TAG_HASH) => {
                // SAFETY: It is safe because the caller ensures that `descriptor` is a non-null
                // pointer pointing to a valid struct.
                let descriptor = unsafe { HashDescriptor::from_descriptor_ptr(descriptor, data)? };
                Ok(Self::Hash(descriptor))
            }
            Ok(AvbDescriptorTag::AVB_DESCRIPTOR_TAG_PROPERTY) => {
                // SAFETY: It is safe because the caller ensures that `descriptor` is a non-null
                // pointer pointing to a valid struct.
                let descriptor =
                    unsafe { PropertyDescriptor::from_descriptor_ptr(descriptor, data)? };
                Ok(Self::Property(descriptor))
            }
            _ => Err(AvbIOError::NoSuchValue),
        }
    }
}
