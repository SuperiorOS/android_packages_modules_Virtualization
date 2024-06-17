/*
 * Copyright 2024 The Android Open Source Project
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

#define LOG_TAG "VirtualizationSystemService"

#include <android/avf_cc_flags.h>
#include <jni.h>
#include <log/log.h>

extern "C" JNIEXPORT jboolean JNICALL
Java_com_android_system_virtualmachine_VirtualizationSystemService_nativeIsNetworkFlagEnabled(
        [[maybe_unused]] JNIEnv* env, [[maybe_unused]] jobject obj) {
    return android::virtualization::IsNetworkFlagEnabled();
}
