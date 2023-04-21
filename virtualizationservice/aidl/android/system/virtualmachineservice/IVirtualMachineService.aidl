/*
 * Copyright 2021 The Android Open Source Project
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
package android.system.virtualmachineservice;

import android.system.virtualizationcommon.ErrorCode;

/** {@hide} */
interface IVirtualMachineService {
    /**
     * Port number that VirtualMachineService listens on connections from the guest VMs for the
     * tombtones
     */
    const int VM_TOMBSTONES_SERVICE_PORT = 2000;

    /**
     * Notifies that the payload has started.
     */
    void notifyPayloadStarted();

    /**
     * Notifies that the payload is ready to serve.
     */
    void notifyPayloadReady();

    /**
     * Notifies that the payload has finished.
     */
    void notifyPayloadFinished(int exitCode);

    /**
     * Notifies that an error has occurred inside the VM.
     */
    void notifyError(ErrorCode errorCode, in String message);

    /**
     * Requests a certificate using the provided certificate signing request (CSR).
     *
     * @param csr the certificate signing request.
     * @return the X.509 encoded certificate.
     */
    byte[] requestCertificate(in byte[] csr);
}
