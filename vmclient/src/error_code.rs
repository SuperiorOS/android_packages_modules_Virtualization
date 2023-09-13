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

use android_system_virtualizationcommon::aidl::android::system::virtualizationcommon::ErrorCode::ErrorCode as AidlErrorCode;

/// Errors reported from within a VM.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorCode {
    /// Error code for all other errors not listed below.
    Unknown,

    /// Error code indicating that the payload can't be verified due to various reasons (e.g
    /// invalid merkle tree, invalid formats, etc).
    PayloadVerificationFailed,

    /// Error code indicating that the payload is verified, but has changed since the last boot.
    PayloadChanged,

    /// Error code indicating that the payload config is invalid.
    PayloadConfigInvalid,

    /// Payload sent a death reason which was not recognised by the client library.
    Unrecognised(AidlErrorCode),
}

impl From<AidlErrorCode> for ErrorCode {
    fn from(error_code: AidlErrorCode) -> Self {
        match error_code {
            AidlErrorCode::UNKNOWN => Self::Unknown,
            AidlErrorCode::PAYLOAD_VERIFICATION_FAILED => Self::PayloadVerificationFailed,
            AidlErrorCode::PAYLOAD_CHANGED => Self::PayloadChanged,
            AidlErrorCode::PAYLOAD_CONFIG_INVALID => Self::PayloadConfigInvalid,
            _ => Self::Unrecognised(error_code),
        }
    }
}
