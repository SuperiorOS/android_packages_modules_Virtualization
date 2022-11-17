// Copyright 2021, The Android Open Source Project
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

//! Implementation of the AIDL interface of the VirtualizationService.

use crate::atom::{write_vm_booted_stats, write_vm_creation_stats};
use crate::composite::make_composite_image;
use crate::crosvm::{CrosvmConfig, DiskFile, PayloadState, VmInstance, VmState};
use crate::payload::{add_microdroid_payload_images, add_microdroid_system_images};
use crate::selinux::{getfilecon, SeContext};
use android_os_permissions_aidl::aidl::android::os::IPermissionController;
use android_system_virtualizationcommon::aidl::android::system::virtualizationcommon::ErrorCode::ErrorCode;
use android_system_virtualizationservice::aidl::android::system::virtualizationservice::{
    DeathReason::DeathReason,
    DiskImage::DiskImage,
    IVirtualMachine::{BnVirtualMachine, IVirtualMachine},
    IVirtualMachineCallback::IVirtualMachineCallback,
    IVirtualizationService::IVirtualizationService,
    Partition::Partition,
    PartitionType::PartitionType,
    VirtualMachineAppConfig::{Payload::Payload, VirtualMachineAppConfig},
    VirtualMachineConfig::VirtualMachineConfig,
    VirtualMachineDebugInfo::VirtualMachineDebugInfo,
    VirtualMachinePayloadConfig::VirtualMachinePayloadConfig,
    VirtualMachineRawConfig::VirtualMachineRawConfig,
    VirtualMachineState::VirtualMachineState,
};
use android_system_virtualmachineservice::aidl::android::system::virtualmachineservice::IVirtualMachineService::{
        BnVirtualMachineService, IVirtualMachineService, VM_BINDER_SERVICE_PORT,
        VM_TOMBSTONES_SERVICE_PORT,
};
use anyhow::{anyhow, bail, Context, Result};
use apkverify::{HashAlgorithm, V4Signature};
use binder::{
    self, BinderFeatures, ExceptionCode, Interface, LazyServiceGuard, ParcelFileDescriptor,
    SpIBinder, Status, StatusCode, Strong, ThreadState,
};
use disk::QcowFile;
use libc::VMADDR_CID_HOST;
use log::{debug, error, info, warn};
use microdroid_payload_config::{OsConfig, Task, TaskType, VmPayloadConfig};
use rpcbinder::run_vsock_rpc_server_with_factory;
use rustutils::system_properties;
use semver::VersionReq;
use std::convert::TryInto;
use std::ffi::CStr;
use std::fs::{create_dir, File, OpenOptions};
use std::io::{Error, ErrorKind, Read, Write};
use std::num::NonZeroU32;
use std::os::unix::io::{FromRawFd, IntoRawFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use tombstoned_client::{DebuggerdDumpType, TombstonedConnection};
use vmconfig::VmConfig;
use vsock::{VsockListener, VsockStream};
use zip::ZipArchive;

/// The unique ID of a VM used (together with a port number) for vsock communication.
pub type Cid = u32;

pub const BINDER_SERVICE_IDENTIFIER: &str = "android.system.virtualizationservice";

/// Directory in which to write disk image files used while running VMs.
pub const TEMPORARY_DIRECTORY: &str = "/data/misc/virtualizationservice";

/// The first CID to assign to a guest VM managed by the VirtualizationService. CIDs lower than this
/// are reserved for the host or other usage.
const FIRST_GUEST_CID: Cid = 10;

const SYSPROP_LAST_CID: &str = "virtualizationservice.state.last_cid";

/// The size of zero.img.
/// Gaps in composite disk images are filled with a shared zero.img.
const ZERO_FILLER_SIZE: u64 = 4096;

/// Magic string for the instance image
const ANDROID_VM_INSTANCE_MAGIC: &str = "Android-VM-instance";

/// Version of the instance image format
const ANDROID_VM_INSTANCE_VERSION: u16 = 1;

const CHUNK_RECV_MAX_LEN: usize = 1024;

const MICRODROID_OS_NAME: &str = "microdroid";

/// Implementation of `IVirtualizationService`, the entry point of the AIDL service.
#[derive(Debug, Default)]
pub struct VirtualizationService {
    state: Arc<Mutex<State>>,
}

impl Interface for VirtualizationService {
    fn dump(&self, mut file: &File, _args: &[&CStr]) -> Result<(), StatusCode> {
        check_permission("android.permission.DUMP").or(Err(StatusCode::PERMISSION_DENIED))?;
        let state = &mut *self.state.lock().unwrap();
        let vms = state.vms();
        writeln!(file, "Running {0} VMs:", vms.len()).or(Err(StatusCode::UNKNOWN_ERROR))?;
        for vm in vms {
            writeln!(file, "VM CID: {}", vm.cid).or(Err(StatusCode::UNKNOWN_ERROR))?;
            writeln!(file, "\tState: {:?}", vm.vm_state.lock().unwrap())
                .or(Err(StatusCode::UNKNOWN_ERROR))?;
            writeln!(file, "\tPayload state {:?}", vm.payload_state())
                .or(Err(StatusCode::UNKNOWN_ERROR))?;
            writeln!(file, "\tProtected: {}", vm.protected).or(Err(StatusCode::UNKNOWN_ERROR))?;
            writeln!(file, "\ttemporary_directory: {}", vm.temporary_directory.to_string_lossy())
                .or(Err(StatusCode::UNKNOWN_ERROR))?;
            writeln!(file, "\trequester_uid: {}", vm.requester_uid)
                .or(Err(StatusCode::UNKNOWN_ERROR))?;
            writeln!(file, "\trequester_debug_pid: {}", vm.requester_debug_pid)
                .or(Err(StatusCode::UNKNOWN_ERROR))?;
        }
        Ok(())
    }
}

impl IVirtualizationService for VirtualizationService {
    /// Creates (but does not start) a new VM with the given configuration, assigning it the next
    /// available CID.
    ///
    /// Returns a binder `IVirtualMachine` object referring to it, as a handle for the client.
    fn createVm(
        &self,
        config: &VirtualMachineConfig,
        console_fd: Option<&ParcelFileDescriptor>,
        log_fd: Option<&ParcelFileDescriptor>,
    ) -> binder::Result<Strong<dyn IVirtualMachine>> {
        let mut is_protected = false;
        let ret = self.create_vm_internal(config, console_fd, log_fd, &mut is_protected);
        write_vm_creation_stats(config, is_protected, &ret);
        ret
    }

    /// Initialise an empty partition image of the given size to be used as a writable partition.
    fn initializeWritablePartition(
        &self,
        image_fd: &ParcelFileDescriptor,
        size: i64,
        partition_type: PartitionType,
    ) -> binder::Result<()> {
        check_manage_access()?;
        let size = size.try_into().map_err(|e| {
            Status::new_exception_str(
                ExceptionCode::ILLEGAL_ARGUMENT,
                Some(format!("Invalid size {}: {:?}", size, e)),
            )
        })?;
        let image = clone_file(image_fd)?;
        // initialize the file. Any data in the file will be erased.
        image.set_len(0).map_err(|e| {
            Status::new_service_specific_error_str(
                -1,
                Some(format!("Failed to reset a file: {:?}", e)),
            )
        })?;
        let mut part = QcowFile::new(image, size).map_err(|e| {
            Status::new_service_specific_error_str(
                -1,
                Some(format!("Failed to create QCOW2 image: {:?}", e)),
            )
        })?;

        match partition_type {
            PartitionType::RAW => Ok(()),
            PartitionType::ANDROID_VM_INSTANCE => format_as_android_vm_instance(&mut part),
            _ => Err(Error::new(
                ErrorKind::Unsupported,
                format!("Unsupported partition type {:?}", partition_type),
            )),
        }
        .map_err(|e| {
            Status::new_service_specific_error_str(
                -1,
                Some(format!("Failed to initialize partition as {:?}: {:?}", partition_type, e)),
            )
        })?;

        Ok(())
    }

    /// Creates or update the idsig file by digesting the input APK file.
    fn createOrUpdateIdsigFile(
        &self,
        input_fd: &ParcelFileDescriptor,
        idsig_fd: &ParcelFileDescriptor,
    ) -> binder::Result<()> {
        // TODO(b/193504400): do this only when (1) idsig_fd is empty or (2) the APK digest in
        // idsig_fd is different from APK digest in input_fd

        check_manage_access()?;

        let mut input = clone_file(input_fd)?;
        let mut sig = V4Signature::create(&mut input, 4096, &[], HashAlgorithm::SHA256).unwrap();

        let mut output = clone_file(idsig_fd)?;
        output.set_len(0).unwrap();
        sig.write_into(&mut output).unwrap();
        Ok(())
    }

    /// Get a list of all currently running VMs. This method is only intended for debug purposes,
    /// and as such is only permitted from the shell user.
    fn debugListVms(&self) -> binder::Result<Vec<VirtualMachineDebugInfo>> {
        check_debug_access()?;

        let state = &mut *self.state.lock().unwrap();
        let vms = state.vms();
        let cids = vms
            .into_iter()
            .map(|vm| VirtualMachineDebugInfo {
                cid: vm.cid as i32,
                temporaryDirectory: vm.temporary_directory.to_string_lossy().to_string(),
                requesterUid: vm.requester_uid as i32,
                requesterPid: vm.requester_debug_pid,
                state: get_state(&vm),
            })
            .collect();
        Ok(cids)
    }

    /// Hold a strong reference to a VM in VirtualizationService. This method is only intended for
    /// debug purposes, and as such is only permitted from the shell user.
    fn debugHoldVmRef(&self, vmref: &Strong<dyn IVirtualMachine>) -> binder::Result<()> {
        check_debug_access()?;

        let state = &mut *self.state.lock().unwrap();
        state.debug_hold_vm(vmref.clone());
        Ok(())
    }

    /// Drop reference to a VM that is being held by VirtualizationService. Returns the reference if
    /// the VM was found and None otherwise. This method is only intended for debug purposes, and as
    /// such is only permitted from the shell user.
    fn debugDropVmRef(&self, cid: i32) -> binder::Result<Option<Strong<dyn IVirtualMachine>>> {
        check_debug_access()?;

        let state = &mut *self.state.lock().unwrap();
        Ok(state.debug_drop_vm(cid))
    }
}

fn handle_stream_connection_tombstoned() -> Result<()> {
    let listener =
        VsockListener::bind_with_cid_port(VMADDR_CID_HOST, VM_TOMBSTONES_SERVICE_PORT as u32)?;
    for incoming_stream in listener.incoming() {
        let mut incoming_stream = match incoming_stream {
            Err(e) => {
                warn!("invalid incoming connection: {:?}", e);
                continue;
            }
            Ok(s) => s,
        };
        std::thread::spawn(move || {
            if let Err(e) = handle_tombstone(&mut incoming_stream) {
                error!("Failed to write tombstone- {:?}", e);
            }
        });
    }
    Ok(())
}

fn handle_tombstone(stream: &mut VsockStream) -> Result<()> {
    if let Ok(addr) = stream.peer_addr() {
        info!("Vsock Stream connected to cid={} for tombstones", addr.cid());
    }
    let tb_connection =
        TombstonedConnection::connect(std::process::id() as i32, DebuggerdDumpType::Tombstone)
            .context("Failed to connect to tombstoned")?;
    let mut text_output = tb_connection
        .text_output
        .as_ref()
        .ok_or_else(|| anyhow!("Could not get file to write the tombstones on"))?;
    let mut num_bytes_read = 0;
    loop {
        let mut chunk_recv = [0; CHUNK_RECV_MAX_LEN];
        let n = stream
            .read(&mut chunk_recv)
            .context("Failed to read tombstone data from Vsock stream")?;
        if n == 0 {
            break;
        }
        num_bytes_read += n;
        text_output.write_all(&chunk_recv[0..n]).context("Failed to write guests tombstones")?;
    }
    info!("Received {} bytes from guest & wrote to tombstone file", num_bytes_read);
    tb_connection.notify_completion()?;
    Ok(())
}

impl VirtualizationService {
    pub fn init() -> VirtualizationService {
        let service = VirtualizationService::default();

        std::thread::spawn(|| {
            if let Err(e) = handle_stream_connection_tombstoned() {
                warn!("Error receiving tombstone from guest or writing them. Error: {:?}", e);
            }
        });

        // binder server for vm
        // reference to state (not the state itself) is copied
        let state = service.state.clone();
        std::thread::spawn(move || {
            debug!("VirtualMachineService is starting as an RPC service.");
            if run_vsock_rpc_server_with_factory(VM_BINDER_SERVICE_PORT as u32, |cid| {
                VirtualMachineService::factory(cid, &state)
            }) {
                debug!("RPC server has shut down gracefully");
            } else {
                panic!("Premature termination of RPC server");
            }
        });
        service
    }

    fn create_vm_internal(
        &self,
        config: &VirtualMachineConfig,
        console_fd: Option<&ParcelFileDescriptor>,
        log_fd: Option<&ParcelFileDescriptor>,
        is_protected: &mut bool,
    ) -> binder::Result<Strong<dyn IVirtualMachine>> {
        check_manage_access()?;

        let is_custom = match config {
            VirtualMachineConfig::RawConfig(_) => true,
            VirtualMachineConfig::AppConfig(config) => {
                // Some features are reserved for platform apps only, even when using
                // VirtualMachineAppConfig:
                // - controlling CPUs;
                // - specifying a config file in the APK.
                !config.taskProfiles.is_empty() || matches!(config.payload, Payload::ConfigPath(_))
            }
        };
        if is_custom {
            check_use_custom_virtual_machine()?;
        }

        let state = &mut *self.state.lock().unwrap();
        let console_fd = console_fd.map(clone_file).transpose()?;
        let log_fd = log_fd.map(clone_file).transpose()?;
        let requester_uid = ThreadState::get_calling_uid();
        let requester_debug_pid = ThreadState::get_calling_pid();
        let cid = state.next_cid().or(Err(ExceptionCode::ILLEGAL_STATE))?;

        // Counter to generate unique IDs for temporary image files.
        let mut next_temporary_image_id = 0;
        // Files which are referred to from composite images. These must be mapped to the crosvm
        // child process, and not closed before it is started.
        let mut indirect_files = vec![];

        // Make directory for temporary files.
        let temporary_directory: PathBuf = format!("{}/{}", TEMPORARY_DIRECTORY, cid).into();
        create_dir(&temporary_directory).map_err(|e| {
            // At this point, we do not know the protected status of Vm
            // setting it to false, though this may not be correct.
            error!(
                "Failed to create temporary directory {:?} for VM files: {:?}",
                temporary_directory, e
            );
            Status::new_service_specific_error_str(
                -1,
                Some(format!(
                    "Failed to create temporary directory {:?} for VM files: {:?}",
                    temporary_directory, e
                )),
            )
        })?;

        let (is_app_config, config) = match config {
            VirtualMachineConfig::RawConfig(config) => (false, BorrowedOrOwned::Borrowed(config)),
            VirtualMachineConfig::AppConfig(config) => {
                let config = load_app_config(config, &temporary_directory).map_err(|e| {
                    *is_protected = config.protectedVm;
                    let message = format!("Failed to load app config: {:?}", e);
                    error!("{}", message);
                    Status::new_service_specific_error_str(-1, Some(message))
                })?;
                (true, BorrowedOrOwned::Owned(config))
            }
        };
        let config = config.as_ref();
        *is_protected = config.protectedVm;

        // Check if partition images are labeled incorrectly. This is to prevent random images
        // which are not protected by the Android Verified Boot (e.g. bits downloaded by apps) from
        // being loaded in a pVM. This applies to everything in the raw config, and everything but
        // the non-executable, generated partitions in the app config.
        config
            .disks
            .iter()
            .flat_map(|disk| disk.partitions.iter())
            .filter(|partition| {
                if is_app_config {
                    !is_safe_app_partition(&partition.label)
                } else {
                    true // all partitions are checked
                }
            })
            .try_for_each(check_label_for_partition)
            .map_err(|e| Status::new_service_specific_error_str(-1, Some(format!("{:?}", e))))?;

        let zero_filler_path = temporary_directory.join("zero.img");
        write_zero_filler(&zero_filler_path).map_err(|e| {
            error!("Failed to make composite image: {:?}", e);
            Status::new_service_specific_error_str(
                -1,
                Some(format!("Failed to make composite image: {:?}", e)),
            )
        })?;

        // Assemble disk images if needed.
        let disks = config
            .disks
            .iter()
            .map(|disk| {
                assemble_disk_image(
                    disk,
                    &zero_filler_path,
                    &temporary_directory,
                    &mut next_temporary_image_id,
                    &mut indirect_files,
                )
            })
            .collect::<Result<Vec<DiskFile>, _>>()?;

        // Creating this ramdump file unconditionally is not harmful as ramdump will be created
        // only when the VM is configured as such. `ramdump_write` is sent to crosvm and will
        // be the backing store for the /dev/hvc1 where VM will emit ramdump to. `ramdump_read`
        // will be sent back to the client (i.e. the VM owner) for readout.
        let ramdump_path = temporary_directory.join("ramdump");
        let ramdump = prepare_ramdump_file(&ramdump_path).map_err(|e| {
            error!("Failed to prepare ramdump file: {:?}", e);
            Status::new_service_specific_error_str(
                -1,
                Some(format!("Failed to prepare ramdump file: {:?}", e)),
            )
        })?;

        // Actually start the VM.
        let crosvm_config = CrosvmConfig {
            cid,
            name: config.name.clone(),
            bootloader: maybe_clone_file(&config.bootloader)?,
            kernel: maybe_clone_file(&config.kernel)?,
            initrd: maybe_clone_file(&config.initrd)?,
            disks,
            params: config.params.to_owned(),
            protected: *is_protected,
            memory_mib: config.memoryMib.try_into().ok().and_then(NonZeroU32::new),
            cpus: config.numCpus.try_into().ok().and_then(NonZeroU32::new),
            task_profiles: config.taskProfiles.clone(),
            console_fd,
            log_fd,
            ramdump: Some(ramdump),
            indirect_files,
            platform_version: parse_platform_version_req(&config.platformVersion)?,
            detect_hangup: is_app_config,
        };
        let instance = Arc::new(
            VmInstance::new(crosvm_config, temporary_directory, requester_uid, requester_debug_pid)
                .map_err(|e| {
                    error!("Failed to create VM with config {:?}: {:?}", config, e);
                    Status::new_service_specific_error_str(
                        -1,
                        Some(format!("Failed to create VM: {:?}", e)),
                    )
                })?,
        );
        state.add_vm(Arc::downgrade(&instance));
        Ok(VirtualMachine::create(instance))
    }
}

fn write_zero_filler(zero_filler_path: &Path) -> Result<()> {
    let file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(zero_filler_path)
        .with_context(|| "Failed to create zero.img")?;
    file.set_len(ZERO_FILLER_SIZE)?;
    Ok(())
}

fn format_as_android_vm_instance(part: &mut dyn Write) -> std::io::Result<()> {
    part.write_all(ANDROID_VM_INSTANCE_MAGIC.as_bytes())?;
    part.write_all(&ANDROID_VM_INSTANCE_VERSION.to_le_bytes())?;
    part.flush()
}

fn prepare_ramdump_file(ramdump_path: &Path) -> Result<File> {
    File::create(ramdump_path).context(format!("Failed to create ramdump file {:?}", &ramdump_path))
}

/// Given the configuration for a disk image, assembles the `DiskFile` to pass to crosvm.
///
/// This may involve assembling a composite disk from a set of partition images.
fn assemble_disk_image(
    disk: &DiskImage,
    zero_filler_path: &Path,
    temporary_directory: &Path,
    next_temporary_image_id: &mut u64,
    indirect_files: &mut Vec<File>,
) -> Result<DiskFile, Status> {
    let image = if !disk.partitions.is_empty() {
        if disk.image.is_some() {
            warn!("DiskImage {:?} contains both image and partitions.", disk);
            return Err(Status::new_exception_str(
                ExceptionCode::ILLEGAL_ARGUMENT,
                Some("DiskImage contains both image and partitions."),
            ));
        }

        let composite_image_filenames =
            make_composite_image_filenames(temporary_directory, next_temporary_image_id);
        let (image, partition_files) = make_composite_image(
            &disk.partitions,
            zero_filler_path,
            &composite_image_filenames.composite,
            &composite_image_filenames.header,
            &composite_image_filenames.footer,
        )
        .map_err(|e| {
            error!("Failed to make composite image with config {:?}: {:?}", disk, e);
            Status::new_service_specific_error_str(
                -1,
                Some(format!("Failed to make composite image: {:?}", e)),
            )
        })?;

        // Pass the file descriptors for the various partition files to crosvm when it
        // is run.
        indirect_files.extend(partition_files);

        image
    } else if let Some(image) = &disk.image {
        clone_file(image)?
    } else {
        warn!("DiskImage {:?} didn't contain image or partitions.", disk);
        return Err(Status::new_exception_str(
            ExceptionCode::ILLEGAL_ARGUMENT,
            Some("DiskImage didn't contain image or partitions."),
        ));
    };

    Ok(DiskFile { image, writable: disk.writable })
}

fn load_app_config(
    config: &VirtualMachineAppConfig,
    temporary_directory: &Path,
) -> Result<VirtualMachineRawConfig> {
    let apk_file = clone_file(config.apk.as_ref().unwrap())?;
    let idsig_file = clone_file(config.idsig.as_ref().unwrap())?;
    let instance_file = clone_file(config.instanceImage.as_ref().unwrap())?;

    let storage_image = if let Some(file) = config.encryptedStorageImage.as_ref() {
        Some(clone_file(file)?)
    } else {
        None
    };

    let vm_payload_config = match &config.payload {
        Payload::ConfigPath(config_path) => {
            load_vm_payload_config_from_file(&apk_file, config_path.as_str())
                .with_context(|| format!("Couldn't read config from {}", config_path))?
        }
        Payload::PayloadConfig(payload_config) => create_vm_payload_config(payload_config),
    };

    // For now, the only supported OS is Microdroid
    let os_name = vm_payload_config.os.name.as_str();
    if os_name != MICRODROID_OS_NAME {
        bail!("Unknown OS \"{}\"", os_name);
    }

    // It is safe to construct a filename based on the os_name because we've already checked that it
    // is one of the allowed values.
    let vm_config_path = PathBuf::from(format!("/apex/com.android.virt/etc/{}.json", os_name));
    let vm_config_file = File::open(vm_config_path)?;
    let mut vm_config = VmConfig::load(&vm_config_file)?.to_parcelable()?;

    if config.memoryMib > 0 {
        vm_config.memoryMib = config.memoryMib;
    }

    vm_config.name = config.name.clone();
    vm_config.protectedVm = config.protectedVm;
    vm_config.numCpus = config.numCpus;
    vm_config.taskProfiles = config.taskProfiles.clone();

    // Microdroid takes additional init ramdisk & (optionally) storage image
    add_microdroid_system_images(config, instance_file, storage_image, &mut vm_config)?;

    // Include Microdroid payload disk (contains apks, idsigs) in vm config
    add_microdroid_payload_images(
        config,
        temporary_directory,
        apk_file,
        idsig_file,
        &vm_payload_config,
        &mut vm_config,
    )?;

    Ok(vm_config)
}

fn load_vm_payload_config_from_file(apk_file: &File, config_path: &str) -> Result<VmPayloadConfig> {
    let mut apk_zip = ZipArchive::new(apk_file)?;
    let config_file = apk_zip.by_name(config_path)?;
    Ok(serde_json::from_reader(config_file)?)
}

fn create_vm_payload_config(payload_config: &VirtualMachinePayloadConfig) -> VmPayloadConfig {
    // There isn't an actual config file. Construct a synthetic VmPayloadConfig from the explicit
    // parameters we've been given. Microdroid will do something equivalent inside the VM using the
    // payload config that we send it via the metadata file.
    let task =
        Task { type_: TaskType::MicrodroidLauncher, command: payload_config.payloadPath.clone() };
    VmPayloadConfig {
        os: OsConfig { name: MICRODROID_OS_NAME.to_owned() },
        task: Some(task),
        apexes: vec![],
        extra_apks: vec![],
        prefer_staged: false,
        export_tombstones: false,
        enable_authfs: false,
    }
}

/// Generates a unique filename to use for a composite disk image.
fn make_composite_image_filenames(
    temporary_directory: &Path,
    next_temporary_image_id: &mut u64,
) -> CompositeImageFilenames {
    let id = *next_temporary_image_id;
    *next_temporary_image_id += 1;
    CompositeImageFilenames {
        composite: temporary_directory.join(format!("composite-{}.img", id)),
        header: temporary_directory.join(format!("composite-{}-header.img", id)),
        footer: temporary_directory.join(format!("composite-{}-footer.img", id)),
    }
}

/// Filenames for a composite disk image, including header and footer partitions.
#[derive(Clone, Debug, Eq, PartialEq)]
struct CompositeImageFilenames {
    /// The composite disk image itself.
    composite: PathBuf,
    /// The header partition image.
    header: PathBuf,
    /// The footer partition image.
    footer: PathBuf,
}

/// Checks whether the caller has a specific permission
fn check_permission(perm: &str) -> binder::Result<()> {
    let calling_pid = ThreadState::get_calling_pid();
    let calling_uid = ThreadState::get_calling_uid();
    // Root can do anything
    if calling_uid == 0 {
        return Ok(());
    }
    let perm_svc: Strong<dyn IPermissionController::IPermissionController> =
        binder::get_interface("permission")?;
    if perm_svc.checkPermission(perm, calling_pid, calling_uid as i32)? {
        Ok(())
    } else {
        Err(Status::new_exception_str(
            ExceptionCode::SECURITY,
            Some(format!("does not have the {} permission", perm)),
        ))
    }
}

/// Check whether the caller of the current Binder method is allowed to call debug methods.
fn check_debug_access() -> binder::Result<()> {
    check_permission("android.permission.DEBUG_VIRTUAL_MACHINE")
}

/// Check whether the caller of the current Binder method is allowed to manage VMs
fn check_manage_access() -> binder::Result<()> {
    check_permission("android.permission.MANAGE_VIRTUAL_MACHINE")
}

/// Check whether the caller of the current Binder method is allowed to create custom VMs
fn check_use_custom_virtual_machine() -> binder::Result<()> {
    check_permission("android.permission.USE_CUSTOM_VIRTUAL_MACHINE")
}

/// Check if a partition has selinux labels that are not allowed
fn check_label_for_partition(partition: &Partition) -> Result<()> {
    let ctx = getfilecon(partition.image.as_ref().unwrap().as_ref())?;
    check_label_is_allowed(&ctx).with_context(|| format!("Partition {} invalid", &partition.label))
}

// Return whether a partition is exempt from selinux label checks, because we know that it does
// not contain code and is likely to be generated in an app-writable directory.
fn is_safe_app_partition(label: &str) -> bool {
    // See make_payload_disk in payload.rs.
    label == "vm-instance"
        || label == "microdroid-apk-idsig"
        || label == "payload-metadata"
        || label.starts_with("extra-idsig-")
}

fn check_label_is_allowed(ctx: &SeContext) -> Result<()> {
    // We only want to allow code in a VM payload to be sourced from places that apps, and the
    // system, do not have write access to.
    // (Note that sepolicy must also grant read access for these types to both virtualization
    // service and crosvm.)
    // App private data files are deliberately excluded, to avoid arbitrary payloads being run on
    // user devices (W^X).
    match ctx.selinux_type()? {
        | "system_file" // immutable dm-verity protected partition
        | "apk_data_file" // APKs of an installed app
        | "staging_data_file" // updated/staged APEX imagess
        | "shell_data_file" // test files created via adb shell
         => Ok(()),
        _ => bail!("Label {} is not allowed", ctx),
    }
}

/// Implementation of the AIDL `IVirtualMachine` interface. Used as a handle to a VM.
#[derive(Debug)]
struct VirtualMachine {
    instance: Arc<VmInstance>,
    /// Keeps our service process running as long as this VM instance exists.
    #[allow(dead_code)]
    lazy_service_guard: LazyServiceGuard,
}

impl VirtualMachine {
    fn create(instance: Arc<VmInstance>) -> Strong<dyn IVirtualMachine> {
        let binder = VirtualMachine { instance, lazy_service_guard: Default::default() };
        BnVirtualMachine::new_binder(binder, BinderFeatures::default())
    }
}

impl Interface for VirtualMachine {}

impl IVirtualMachine for VirtualMachine {
    fn getCid(&self) -> binder::Result<i32> {
        // Don't check permission. The owner of the VM might have passed this binder object to
        // others.
        Ok(self.instance.cid as i32)
    }

    fn getState(&self) -> binder::Result<VirtualMachineState> {
        // Don't check permission. The owner of the VM might have passed this binder object to
        // others.
        Ok(get_state(&self.instance))
    }

    fn registerCallback(
        &self,
        callback: &Strong<dyn IVirtualMachineCallback>,
    ) -> binder::Result<()> {
        // Don't check permission. The owner of the VM might have passed this binder object to
        // others.
        //
        // TODO: Should this give an error if the VM is already dead?
        self.instance.callbacks.add(callback.clone());
        Ok(())
    }

    fn start(&self) -> binder::Result<()> {
        self.instance.start().map_err(|e| {
            error!("Error starting VM with CID {}: {:?}", self.instance.cid, e);
            Status::new_service_specific_error_str(-1, Some(e.to_string()))
        })
    }

    fn stop(&self) -> binder::Result<()> {
        self.instance.kill().map_err(|e| {
            error!("Error stopping VM with CID {}: {:?}", self.instance.cid, e);
            Status::new_service_specific_error_str(-1, Some(e.to_string()))
        })
    }

    fn connectVsock(&self, port: i32) -> binder::Result<ParcelFileDescriptor> {
        if !matches!(&*self.instance.vm_state.lock().unwrap(), VmState::Running { .. }) {
            return Err(Status::new_service_specific_error_str(-1, Some("VM is not running")));
        }
        let stream =
            VsockStream::connect_with_cid_port(self.instance.cid, port as u32).map_err(|e| {
                Status::new_service_specific_error_str(
                    -1,
                    Some(format!("Failed to connect: {:?}", e)),
                )
            })?;
        Ok(vsock_stream_to_pfd(stream))
    }
}

impl Drop for VirtualMachine {
    fn drop(&mut self) {
        debug!("Dropping {:?}", self);
        if let Err(e) = self.instance.kill() {
            debug!("Error stopping dropped VM with CID {}: {:?}", self.instance.cid, e);
        }
    }
}

/// A set of Binders to be called back in response to various events on the VM, such as when it
/// dies.
#[derive(Debug, Default)]
pub struct VirtualMachineCallbacks(Mutex<Vec<Strong<dyn IVirtualMachineCallback>>>);

impl VirtualMachineCallbacks {
    /// Call all registered callbacks to notify that the payload has started.
    pub fn notify_payload_started(&self, cid: Cid) {
        let callbacks = &*self.0.lock().unwrap();
        for callback in callbacks {
            if let Err(e) = callback.onPayloadStarted(cid as i32) {
                error!("Error notifying payload start event from VM CID {}: {:?}", cid, e);
            }
        }
    }

    /// Call all registered callbacks to notify that the payload is ready to serve.
    pub fn notify_payload_ready(&self, cid: Cid) {
        let callbacks = &*self.0.lock().unwrap();
        for callback in callbacks {
            if let Err(e) = callback.onPayloadReady(cid as i32) {
                error!("Error notifying payload ready event from VM CID {}: {:?}", cid, e);
            }
        }
    }

    /// Call all registered callbacks to notify that the payload has finished.
    pub fn notify_payload_finished(&self, cid: Cid, exit_code: i32) {
        let callbacks = &*self.0.lock().unwrap();
        for callback in callbacks {
            if let Err(e) = callback.onPayloadFinished(cid as i32, exit_code) {
                error!("Error notifying payload finish event from VM CID {}: {:?}", cid, e);
            }
        }
    }

    /// Call all registered callbacks to say that the VM encountered an error.
    pub fn notify_error(&self, cid: Cid, error_code: ErrorCode, message: &str) {
        let callbacks = &*self.0.lock().unwrap();
        for callback in callbacks {
            if let Err(e) = callback.onError(cid as i32, error_code, message) {
                error!("Error notifying error event from VM CID {}: {:?}", cid, e);
            }
        }
    }

    /// Call all registered callbacks to notify that the payload has provided a standard I/O proxy.
    pub fn notify_payload_stdio(&self, cid: Cid, fd: ParcelFileDescriptor) {
        let callbacks = &*self.0.lock().unwrap();
        for callback in callbacks {
            if let Err(e) = callback.onPayloadStdio(cid as i32, &fd) {
                error!("Error notifying payload stdio event from VM CID {}: {:?}", cid, e);
            }
        }
    }

    /// Call all registered callbacks to say that the VM has died.
    pub fn callback_on_died(&self, cid: Cid, reason: DeathReason) {
        let callbacks = &*self.0.lock().unwrap();
        for callback in callbacks {
            if let Err(e) = callback.onDied(cid as i32, reason) {
                error!("Error notifying exit of VM CID {}: {:?}", cid, e);
            }
        }
    }

    /// Call all registered callbacks to say that there was a ramdump to download.
    pub fn callback_on_ramdump(&self, cid: Cid, ramdump: File) {
        let callbacks = &*self.0.lock().unwrap();
        let pfd = ParcelFileDescriptor::new(ramdump);
        for callback in callbacks {
            if let Err(e) = callback.onRamdump(cid as i32, &pfd) {
                error!("Error notifying ramdump of VM CID {}: {:?}", cid, e);
            }
        }
    }

    /// Add a new callback to the set.
    fn add(&self, callback: Strong<dyn IVirtualMachineCallback>) {
        self.0.lock().unwrap().push(callback);
    }
}

/// The mutable state of the VirtualizationService. There should only be one instance of this
/// struct.
#[derive(Debug, Default)]
struct State {
    /// The VMs which have been started. When VMs are started a weak reference is added to this list
    /// while a strong reference is returned to the caller over Binder. Once all copies of the
    /// Binder client are dropped the weak reference here will become invalid, and will be removed
    /// from the list opportunistically the next time `add_vm` is called.
    vms: Vec<Weak<VmInstance>>,

    /// Vector of strong VM references held on behalf of users that cannot hold them themselves.
    /// This is only used for debugging purposes.
    debug_held_vms: Vec<Strong<dyn IVirtualMachine>>,
}

impl State {
    /// Get a list of VMs which still have Binder references to them.
    fn vms(&self) -> Vec<Arc<VmInstance>> {
        // Attempt to upgrade the weak pointers to strong pointers.
        self.vms.iter().filter_map(Weak::upgrade).collect()
    }

    /// Add a new VM to the list.
    fn add_vm(&mut self, vm: Weak<VmInstance>) {
        // Garbage collect any entries from the stored list which no longer exist.
        self.vms.retain(|vm| vm.strong_count() > 0);

        // Actually add the new VM.
        self.vms.push(vm);
    }

    /// Get a VM that corresponds to the given cid
    fn get_vm(&self, cid: Cid) -> Option<Arc<VmInstance>> {
        self.vms().into_iter().find(|vm| vm.cid == cid)
    }

    /// Store a strong VM reference.
    fn debug_hold_vm(&mut self, vm: Strong<dyn IVirtualMachine>) {
        self.debug_held_vms.push(vm);
    }

    /// Retrieve and remove a strong VM reference.
    fn debug_drop_vm(&mut self, cid: i32) -> Option<Strong<dyn IVirtualMachine>> {
        let pos = self.debug_held_vms.iter().position(|vm| vm.getCid() == Ok(cid))?;
        let vm = self.debug_held_vms.swap_remove(pos);
        Some(vm)
    }

    /// Get the next available CID, or an error if we have run out. The last CID used is stored in
    /// a system property so that restart of virtualizationservice doesn't reuse CID while the host
    /// Android is up.
    fn next_cid(&mut self) -> Result<Cid> {
        let next = if let Some(val) = system_properties::read(SYSPROP_LAST_CID)? {
            if let Ok(num) = val.parse::<u32>() {
                num.checked_add(1).ok_or_else(|| anyhow!("run out of CID"))?
            } else {
                error!("Invalid last CID {}. Using {}", &val, FIRST_GUEST_CID);
                FIRST_GUEST_CID
            }
        } else {
            // First VM since the boot
            FIRST_GUEST_CID
        };
        // Persist the last value for next use
        let str_val = format!("{}", next);
        system_properties::write(SYSPROP_LAST_CID, &str_val)?;
        Ok(next)
    }
}

/// Gets the `VirtualMachineState` of the given `VmInstance`.
fn get_state(instance: &VmInstance) -> VirtualMachineState {
    match &*instance.vm_state.lock().unwrap() {
        VmState::NotStarted { .. } => VirtualMachineState::NOT_STARTED,
        VmState::Running { .. } => match instance.payload_state() {
            PayloadState::Starting => VirtualMachineState::STARTING,
            PayloadState::Started => VirtualMachineState::STARTED,
            PayloadState::Ready => VirtualMachineState::READY,
            PayloadState::Finished => VirtualMachineState::FINISHED,
            PayloadState::Hangup => VirtualMachineState::DEAD,
        },
        VmState::Dead => VirtualMachineState::DEAD,
        VmState::Failed => VirtualMachineState::DEAD,
    }
}

/// Converts a `&ParcelFileDescriptor` to a `File` by cloning the file.
pub fn clone_file(file: &ParcelFileDescriptor) -> Result<File, Status> {
    file.as_ref().try_clone().map_err(|e| {
        Status::new_exception_str(
            ExceptionCode::BAD_PARCELABLE,
            Some(format!("Failed to clone File from ParcelFileDescriptor: {:?}", e)),
        )
    })
}

/// Converts an `&Option<ParcelFileDescriptor>` to an `Option<File>` by cloning the file.
fn maybe_clone_file(file: &Option<ParcelFileDescriptor>) -> Result<Option<File>, Status> {
    file.as_ref().map(clone_file).transpose()
}

/// Converts a `VsockStream` to a `ParcelFileDescriptor`.
fn vsock_stream_to_pfd(stream: VsockStream) -> ParcelFileDescriptor {
    // SAFETY: ownership is transferred from stream to f
    let f = unsafe { File::from_raw_fd(stream.into_raw_fd()) };
    ParcelFileDescriptor::new(f)
}

/// Parses the platform version requirement string.
fn parse_platform_version_req(s: &str) -> Result<VersionReq, Status> {
    VersionReq::parse(s).map_err(|e| {
        Status::new_exception_str(
            ExceptionCode::BAD_PARCELABLE,
            Some(format!("Invalid platform version requirement {}: {:?}", s, e)),
        )
    })
}

/// Simple utility for referencing Borrowed or Owned. Similar to std::borrow::Cow, but
/// it doesn't require that T implements Clone.
enum BorrowedOrOwned<'a, T> {
    Borrowed(&'a T),
    Owned(T),
}

impl<'a, T> AsRef<T> for BorrowedOrOwned<'a, T> {
    fn as_ref(&self) -> &T {
        match self {
            Self::Borrowed(b) => b,
            Self::Owned(o) => o,
        }
    }
}

/// Implementation of `IVirtualMachineService`, the entry point of the AIDL service.
#[derive(Debug, Default)]
struct VirtualMachineService {
    state: Arc<Mutex<State>>,
    cid: Cid,
}

impl Interface for VirtualMachineService {}

impl IVirtualMachineService for VirtualMachineService {
    fn notifyPayloadStarted(&self) -> binder::Result<()> {
        let cid = self.cid;
        if let Some(vm) = self.state.lock().unwrap().get_vm(cid) {
            info!("VM with CID {} started payload", cid);
            vm.update_payload_state(PayloadState::Started).map_err(|e| {
                Status::new_exception_str(ExceptionCode::ILLEGAL_STATE, Some(e.to_string()))
            })?;
            vm.callbacks.notify_payload_started(cid);

            let vm_start_timestamp = vm.vm_start_timestamp.lock().unwrap();
            write_vm_booted_stats(vm.requester_uid as i32, &vm.name, *vm_start_timestamp);
            Ok(())
        } else {
            error!("notifyPayloadStarted is called from an unknown CID {}", cid);
            Err(Status::new_service_specific_error_str(
                -1,
                Some(format!("cannot find a VM with CID {}", cid)),
            ))
        }
    }

    fn notifyPayloadReady(&self) -> binder::Result<()> {
        let cid = self.cid;
        if let Some(vm) = self.state.lock().unwrap().get_vm(cid) {
            info!("VM with CID {} reported payload is ready", cid);
            vm.update_payload_state(PayloadState::Ready).map_err(|e| {
                Status::new_exception_str(ExceptionCode::ILLEGAL_STATE, Some(e.to_string()))
            })?;
            vm.callbacks.notify_payload_ready(cid);
            Ok(())
        } else {
            error!("notifyPayloadReady is called from an unknown CID {}", cid);
            Err(Status::new_service_specific_error_str(
                -1,
                Some(format!("cannot find a VM with CID {}", cid)),
            ))
        }
    }

    fn notifyPayloadFinished(&self, exit_code: i32) -> binder::Result<()> {
        let cid = self.cid;
        if let Some(vm) = self.state.lock().unwrap().get_vm(cid) {
            info!("VM with CID {} finished payload", cid);
            vm.update_payload_state(PayloadState::Finished).map_err(|e| {
                Status::new_exception_str(ExceptionCode::ILLEGAL_STATE, Some(e.to_string()))
            })?;
            vm.callbacks.notify_payload_finished(cid, exit_code);
            Ok(())
        } else {
            error!("notifyPayloadFinished is called from an unknown CID {}", cid);
            Err(Status::new_service_specific_error_str(
                -1,
                Some(format!("cannot find a VM with CID {}", cid)),
            ))
        }
    }

    fn notifyError(&self, error_code: ErrorCode, message: &str) -> binder::Result<()> {
        let cid = self.cid;
        if let Some(vm) = self.state.lock().unwrap().get_vm(cid) {
            info!("VM with CID {} encountered an error", cid);
            vm.update_payload_state(PayloadState::Finished).map_err(|e| {
                Status::new_exception_str(ExceptionCode::ILLEGAL_STATE, Some(e.to_string()))
            })?;
            vm.callbacks.notify_error(cid, error_code, message);
            Ok(())
        } else {
            error!("notifyError is called from an unknown CID {}", cid);
            Err(Status::new_service_specific_error_str(
                -1,
                Some(format!("cannot find a VM with CID {}", cid)),
            ))
        }
    }

    fn connectPayloadStdioProxy(&self, port: i32) -> binder::Result<()> {
        let cid = self.cid;
        if let Some(vm) = self.state.lock().unwrap().get_vm(cid) {
            info!("VM with CID {} started a stdio proxy", cid);
            let stream = VsockStream::connect_with_cid_port(cid, port as u32).map_err(|e| {
                Status::new_service_specific_error_str(
                    -1,
                    Some(format!("Failed to connect to guest stdio proxy: {:?}", e)),
                )
            })?;
            vm.callbacks.notify_payload_stdio(cid, vsock_stream_to_pfd(stream));
            Ok(())
        } else {
            error!("connectPayloadStdioProxy is called from an unknown CID {}", cid);
            Err(Status::new_service_specific_error_str(
                -1,
                Some(format!("cannot find a VM with CID {}", cid)),
            ))
        }
    }
}

impl VirtualMachineService {
    fn factory(cid: Cid, state: &Arc<Mutex<State>>) -> Option<SpIBinder> {
        if let Some(vm) = state.lock().unwrap().get_vm(cid) {
            let mut vm_service = vm.vm_service.lock().unwrap();
            let service = vm_service.get_or_insert_with(|| Self::new_binder(state.clone(), cid));
            Some(service.as_binder())
        } else {
            error!("connection from cid={} is not from a guest VM", cid);
            None
        }
    }

    fn new_binder(state: Arc<Mutex<State>>, cid: Cid) -> Strong<dyn IVirtualMachineService> {
        BnVirtualMachineService::new_binder(
            VirtualMachineService { state, cid },
            BinderFeatures::default(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_allowed_label_for_partition() -> Result<()> {
        let expected_results = vec![
            ("u:object_r:system_file:s0", true),
            ("u:object_r:apk_data_file:s0", true),
            ("u:object_r:app_data_file:s0", false),
            ("u:object_r:app_data_file:s0:c512,c768", false),
            ("u:object_r:privapp_data_file:s0:c512,c768", false),
            ("invalid", false),
            ("user:role:apk_data_file:severity:categories", true),
            ("user:role:apk_data_file:severity:categories:extraneous", false),
        ];

        for (label, expected_valid) in expected_results {
            let context = SeContext::new(label)?;
            let result = check_label_is_allowed(&context);
            if expected_valid {
                assert!(result.is_ok(), "Expected label {} to be allowed, got {:?}", label, result);
            } else if result.is_ok() {
                bail!("Expected label {} to be disallowed", label);
            }
        }
        Ok(())
    }
}
