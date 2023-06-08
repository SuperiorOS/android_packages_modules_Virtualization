// Copyright 2022, The Android Open Source Project
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

//! Functions to scan the PCI bus for VirtIO devices.

use super::hal::HalImpl;
use alloc::boxed::Box;
use core::fmt;
use fdtpci::PciInfo;
use log::debug;
use once_cell::race::OnceBox;
use virtio_drivers::{
    device::blk,
    transport::{
        pci::{
            bus::{BusDeviceIterator, PciRoot},
            virtio_device_type, PciTransport,
        },
        DeviceType, Transport,
    },
};
use vmbase::memory::{MemoryTracker, MemoryTrackerError};

pub(super) static PCI_INFO: OnceBox<PciInfo> = OnceBox::new();

/// PCI errors.
#[derive(Debug, Clone)]
pub enum PciError {
    /// Attempted to initialize the PCI more than once.
    DuplicateInitialization,
    /// Failed to map PCI CAM.
    CamMapFailed(MemoryTrackerError),
    /// Failed to map PCI BAR.
    BarMapFailed(MemoryTrackerError),
}

impl fmt::Display for PciError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::DuplicateInitialization => {
                write!(f, "Attempted to initialize the PCI more than once.")
            }
            Self::CamMapFailed(e) => write!(f, "Failed to map PCI CAM: {e}"),
            Self::BarMapFailed(e) => write!(f, "Failed to map PCI BAR: {e}"),
        }
    }
}

/// Prepares to use VirtIO PCI devices.
///
/// In particular:
///
/// 1. Maps the PCI CAM and BAR range in the page table and MMIO guard.
/// 2. Stores the `PciInfo` for the VirtIO HAL to use later.
/// 3. Creates and returns a `PciRoot`.
///
/// This must only be called once; it will panic if it is called a second time.
pub fn initialise(pci_info: PciInfo, memory: &mut MemoryTracker) -> Result<PciRoot, PciError> {
    PCI_INFO.set(Box::new(pci_info.clone())).map_err(|_| PciError::DuplicateInitialization)?;

    memory.map_mmio_range(pci_info.cam_range.clone()).map_err(PciError::CamMapFailed)?;
    let bar_range = pci_info.bar_range.start as usize..pci_info.bar_range.end as usize;
    memory.map_mmio_range(bar_range).map_err(PciError::BarMapFailed)?;

    // Safety: This is the only place where we call make_pci_root, and `PCI_INFO.set` above will
    // panic if it is called a second time.
    Ok(unsafe { pci_info.make_pci_root() })
}

pub type VirtIOBlk = blk::VirtIOBlk<HalImpl, PciTransport>;

pub struct VirtIOBlkIterator<'a> {
    pci_root: &'a mut PciRoot,
    bus: BusDeviceIterator,
}

impl<'a> VirtIOBlkIterator<'a> {
    pub fn new(pci_root: &'a mut PciRoot) -> Self {
        let bus = pci_root.enumerate_bus(0);
        Self { pci_root, bus }
    }
}

impl<'a> Iterator for VirtIOBlkIterator<'a> {
    type Item = VirtIOBlk;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let (device_function, info) = self.bus.next()?;
            let (status, command) = self.pci_root.get_status_command(device_function);
            debug!(
                "Found PCI device {} at {}, status {:?} command {:?}",
                info, device_function, status, command
            );

            let Some(virtio_type) = virtio_device_type(&info) else {
                continue;
            };
            debug!("  VirtIO {:?}", virtio_type);

            let mut transport =
                PciTransport::new::<HalImpl>(self.pci_root, device_function).unwrap();
            debug!(
                "Detected virtio PCI device with device type {:?}, features {:#018x}",
                transport.device_type(),
                transport.read_device_features(),
            );

            if virtio_type == DeviceType::Block {
                return Some(Self::Item::new(transport).expect("failed to create blk driver"));
            }
        }
    }
}
