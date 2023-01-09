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

package com.android.microdroid.test;

import static com.android.microdroid.test.host.CommandResultSubject.command_results;
import static com.android.tradefed.device.TestDevice.MicrodroidBuilder;
import static com.android.tradefed.testtype.DeviceJUnit4ClassRunner.TestLogData;

import static com.google.common.truth.Truth.assertThat;
import static com.google.common.truth.Truth.assertWithMessage;

import static org.hamcrest.CoreMatchers.containsString;
import static org.hamcrest.CoreMatchers.is;
import static org.junit.Assert.assertThat;
import static org.junit.Assume.assumeFalse;
import static org.junit.Assume.assumeTrue;

import static java.util.stream.Collectors.toList;

import android.cts.statsdatom.lib.ConfigUtils;
import android.cts.statsdatom.lib.ReportUtils;

import com.android.compatibility.common.util.CddTest;
import com.android.microdroid.test.common.ProcessUtil;
import com.android.microdroid.test.host.CommandRunner;
import com.android.microdroid.test.host.MicrodroidHostTestCaseBase;
import com.android.os.AtomsProto;
import com.android.os.StatsLog;
import com.android.tradefed.device.DeviceNotAvailableException;
import com.android.tradefed.device.ITestDevice;
import com.android.tradefed.device.TestDevice;
import com.android.tradefed.log.LogUtil.CLog;
import com.android.tradefed.testtype.DeviceJUnit4ClassRunner;
import com.android.tradefed.testtype.DeviceJUnit4ClassRunner.TestMetrics;
import com.android.tradefed.util.CommandResult;
import com.android.tradefed.util.FileUtil;
import com.android.tradefed.util.RunUtil;
import com.android.tradefed.util.xml.AbstractXmlParser;

import org.json.JSONArray;
import org.json.JSONObject;
import org.junit.After;
import org.junit.Before;
import org.junit.Ignore;
import org.junit.Rule;
import org.junit.Test;
import org.junit.rules.TestName;
import org.junit.runner.RunWith;
import org.xml.sax.Attributes;
import org.xml.sax.helpers.DefaultHandler;

import java.io.BufferedReader;
import java.io.ByteArrayInputStream;
import java.io.File;
import java.io.IOException;
import java.io.InputStream;
import java.io.InputStreamReader;
import java.io.PipedInputStream;
import java.io.PipedOutputStream;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.Map;
import java.util.concurrent.Callable;
import java.util.concurrent.TimeUnit;
import java.util.function.Function;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

@RunWith(DeviceJUnit4ClassRunner.class)
public class MicrodroidHostTests extends MicrodroidHostTestCaseBase {
    private static final String APK_NAME = "MicrodroidTestApp.apk";
    private static final String PACKAGE_NAME = "com.android.microdroid.test";
    private static final String SHELL_PACKAGE_NAME = "com.android.shell";
    private static final String VIRT_APEX = "/apex/com.android.virt/";

    private static final int MIN_MEM_ARM64 = 145;
    private static final int MIN_MEM_X86_64 = 196;

    // Number of vCPUs for testing purpose
    private static final int NUM_VCPUS = 3;

    private static final int BOOT_COMPLETE_TIMEOUT = 30000; // 30 seconds

    private static class VmInfo {
        final Process mProcess;
        final String mCid;

        VmInfo(Process process, String cid) {
            mProcess = process;
            mCid = cid;
        }
    }

    @Rule public TestLogData mTestLogs = new TestLogData();
    @Rule public TestName mTestName = new TestName();
    @Rule public TestMetrics mMetrics = new TestMetrics();

    private String mMetricPrefix;

    private ITestDevice mMicrodroidDevice;

    private int minMemorySize() throws DeviceNotAvailableException {
        CommandRunner android = new CommandRunner(getDevice());
        String abi = android.run("getprop", "ro.product.cpu.abi");
        assertThat(abi).isNotEmpty();
        if (abi.startsWith("arm64")) {
            return MIN_MEM_ARM64;
        } else if (abi.startsWith("x86_64")) {
            return MIN_MEM_X86_64;
        }
        throw new AssertionError("Unsupported ABI: " + abi);
    }

    private static JSONObject newPartition(String label, String path) {
        return new JSONObject(Map.of("label", label, "path", path));
    }

    private void createPayloadMetadata(List<ActiveApexInfo> apexes, File payloadMetadata)
            throws Exception {
        // mk_payload's config
        File configFile = new File(payloadMetadata.getParentFile(), "payload_config.json");
        JSONObject config = new JSONObject();
        config.put(
                "apk",
                new JSONObject(Map.of("name", "microdroid-apk", "path", "", "idsig_path", "")));
        config.put("payload_config_path", "/mnt/apk/assets/vm_config.json");
        config.put(
                "apexes",
                new JSONArray(
                        apexes.stream()
                                .map(apex -> new JSONObject(Map.of("name", apex.name, "path", "")))
                                .collect(toList())));
        FileUtil.writeToFile(config.toString(), configFile);

        RunUtil runUtil = new RunUtil();
        String command =
                String.join(
                        " ",
                        findTestFile("mk_payload").getAbsolutePath(),
                        "--metadata-only",
                        configFile.getAbsolutePath(),
                        payloadMetadata.getAbsolutePath());
        // mk_payload should run fast enough
        CommandResult result = runUtil.runTimedCmd(5000, "/bin/bash", "-c", command);
        String out = result.getStdout();
        String err = result.getStderr();
        assertWithMessage(
                        "creating payload metadata failed:\n\tout: "
                                + out
                                + "\n\terr: "
                                + err
                                + "\n")
                .about(command_results())
                .that(result)
                .isSuccess();
    }

    private void resignVirtApex(File virtApexDir, File signingKey, Map<String, File> keyOverrides) {
        File signVirtApex = findTestFile("sign_virt_apex");

        RunUtil runUtil = new RunUtil();
        // Set the parent dir on the PATH (e.g. <workdir>/bin)
        String separator = System.getProperty("path.separator");
        String path = signVirtApex.getParentFile().getPath() + separator + System.getenv("PATH");
        runUtil.setEnvVariable("PATH", path);

        List<String> command = new ArrayList<>();
        command.add(signVirtApex.getAbsolutePath());
        keyOverrides.forEach(
                (filename, keyFile) ->
                        command.add("--key_override " + filename + "=" + keyFile.getPath()));
        command.add(signingKey.getPath());
        command.add(virtApexDir.getPath());

        CommandResult result =
                runUtil.runTimedCmd(
                        // sign_virt_apex is so slow on CI server that this often times
                        // out. Until we can make it fast, use 50s for timeout
                        50 * 1000, "/bin/bash", "-c", String.join(" ", command));
        String out = result.getStdout();
        String err = result.getStderr();
        assertWithMessage(
                        "resigning the Virt APEX failed:\n\tout: " + out + "\n\terr: " + err + "\n")
                .about(command_results())
                .that(result)
                .isSuccess();
    }

    private static <T> void assertThatEventually(
            long timeoutMillis, Callable<T> callable, org.hamcrest.Matcher<T> matcher)
            throws Exception {
        long start = System.currentTimeMillis();
        while ((System.currentTimeMillis() - start < timeoutMillis)
                && !matcher.matches(callable.call())) {
            Thread.sleep(500);
        }
        assertThat(callable.call(), matcher);
    }

    static class ActiveApexInfo {
        public String name;
        public String path;
        public boolean provideSharedApexLibs;

        ActiveApexInfo(String name, String path, boolean provideSharedApexLibs) {
            this.name = name;
            this.path = path;
            this.provideSharedApexLibs = provideSharedApexLibs;
        }
    }

    static class ActiveApexInfoList {
        private List<ActiveApexInfo> mList;

        ActiveApexInfoList(List<ActiveApexInfo> list) {
            this.mList = list;
        }

        ActiveApexInfo get(String apexName) {
            return mList.stream()
                    .filter(info -> apexName.equals(info.name))
                    .findFirst()
                    .orElse(null);
        }

        List<ActiveApexInfo> getSharedLibApexes() {
            return mList.stream().filter(info -> info.provideSharedApexLibs).collect(toList());
        }
    }

    private ActiveApexInfoList getActiveApexInfoList() throws Exception {
        String apexInfoListXml = getDevice().pullFileContents("/apex/apex-info-list.xml");
        List<ActiveApexInfo> list = new ArrayList<>();
        new AbstractXmlParser() {
            @Override
            protected DefaultHandler createXmlHandler() {
                return new DefaultHandler() {
                    @Override
                    public void startElement(
                            String uri, String localName, String qName, Attributes attributes) {
                        if (localName.equals("apex-info")
                                && attributes.getValue("isActive").equals("true")) {
                            String name = attributes.getValue("moduleName");
                            String path = attributes.getValue("modulePath");
                            String sharedApex = attributes.getValue("provideSharedApexLibs");
                            list.add(new ActiveApexInfo(name, path, "true".equals(sharedApex)));
                        }
                    }
                };
            }
        }.parse(new ByteArrayInputStream(apexInfoListXml.getBytes()));
        return new ActiveApexInfoList(list);
    }

    private VmInfo runMicrodroidWithResignedImages(
            File key, Map<String, File> keyOverrides, boolean isProtected) throws Exception {
        CommandRunner android = new CommandRunner(getDevice());

        File virtApexDir = FileUtil.createTempDir("virt_apex");

        // Pull the virt apex's etc/ directory (which contains images and microdroid.json)
        File virtApexEtcDir = new File(virtApexDir, "etc");
        // We need only etc/ directory for images
        assertWithMessage("Failed to mkdir " + virtApexEtcDir)
                .that(virtApexEtcDir.mkdirs()).isTrue();
        assertWithMessage("Failed to pull " + VIRT_APEX + "etc")
                .that(getDevice().pullDir(VIRT_APEX + "etc", virtApexEtcDir)).isTrue();

        resignVirtApex(virtApexDir, key, keyOverrides);

        // Push back re-signed virt APEX contents and updated microdroid.json
        getDevice().pushDir(virtApexDir, TEST_ROOT);

        // Create the idsig file for the APK
        final String apkPath = getPathForPackage(PACKAGE_NAME);
        final String idSigPath = TEST_ROOT + "idsig";
        android.run(VIRT_APEX + "bin/vm", "create-idsig", apkPath, idSigPath);

        // Create the instance image for the VM
        final String instanceImgPath = TEST_ROOT + "instance.img";
        android.run(
                VIRT_APEX + "bin/vm",
                "create-partition",
                "--type instance",
                instanceImgPath,
                Integer.toString(10 * 1024 * 1024));

        // payload-metadata is created on device
        final String payloadMetadataPath = TEST_ROOT + "payload-metadata.img";

        // Load /apex/apex-info-list.xml to get paths to APEXes required for the VM.
        ActiveApexInfoList list = getActiveApexInfoList();

        // Since Java APP can't start a VM with a custom image, here, we start a VM using `vm run`
        // command with a VM Raw config which is equiv. to what virtualizationservice creates with
        // a VM App config.
        //
        // 1. use etc/microdroid.json as base
        // 2. add partitions: bootconfig, vbmeta, instance image
        // 3. add a payload image disk with
        //   - payload-metadata
        //   - apexes
        //   - test apk
        //   - its idsig

        // Load etc/microdroid.json
        File microdroidConfigFile = new File(virtApexEtcDir, "microdroid.json");
        JSONObject config = new JSONObject(FileUtil.readStringFromFile(microdroidConfigFile));

        // Replace paths so that the config uses re-signed images from TEST_ROOT
        config.put("kernel", config.getString("kernel").replace(VIRT_APEX, TEST_ROOT));
        JSONArray disks = config.getJSONArray("disks");
        for (int diskIndex = 0; diskIndex < disks.length(); diskIndex++) {
            JSONObject disk = disks.getJSONObject(diskIndex);
            JSONArray partitions = disk.getJSONArray("partitions");
            for (int partIndex = 0; partIndex < partitions.length(); partIndex++) {
                JSONObject part = partitions.getJSONObject(partIndex);
                part.put("path", part.getString("path").replace(VIRT_APEX, TEST_ROOT));
            }
        }

        // Add partitions to the second disk
        final String initrdPath = TEST_ROOT + "etc/microdroid_initrd_debuggable.img";
        config.put("initrd", initrdPath);
        // Add instance image as a partition in disks[1]
        disks.put(new JSONObject()
                .put("writable", true)
                .put("partitions",
                        new JSONArray().put(newPartition("vm-instance", instanceImgPath))));
        // Add payload image disk with partitions:
        // - payload-metadata
        // - apexes: com.android.os.statsd, com.android.adbd, [sharedlib apex](optional)
        // - apk and idsig
        List<ActiveApexInfo> apexesForVm = new ArrayList<>();
        apexesForVm.add(list.get("com.android.os.statsd"));
        apexesForVm.add(list.get("com.android.adbd"));
        apexesForVm.addAll(list.getSharedLibApexes());

        final JSONArray partitions = new JSONArray();
        partitions.put(newPartition("payload-metadata", payloadMetadataPath));
        for (ActiveApexInfo apex : apexesForVm) {
            partitions.put(newPartition(apex.name, apex.path));
        }
        partitions
                .put(newPartition("microdroid-apk", apkPath))
                .put(newPartition("microdroid-apk-idsig", idSigPath));
        disks.put(new JSONObject().put("writable", false).put("partitions", partitions));

        final File localPayloadMetadata = new File(virtApexDir, "payload-metadata.img");
        createPayloadMetadata(apexesForVm, localPayloadMetadata);
        getDevice().pushFile(localPayloadMetadata, payloadMetadataPath);

        config.put("protected", isProtected);

        // Write updated raw config
        final String configPath = TEST_ROOT + "raw_config.json";
        getDevice().pushString(config.toString(), configPath);

        List<String> args =
                Arrays.asList(
                        "adb",
                        "-s",
                        getDevice().getSerialNumber(),
                        "shell",
                        VIRT_APEX + "bin/vm run",
                        "--console " + CONSOLE_PATH,
                        "--log " + LOG_PATH,
                        configPath);

        PipedInputStream pis = new PipedInputStream();
        Process process = RunUtil.getDefault().runCmdInBackground(args, new PipedOutputStream(pis));
        return new VmInfo(process, extractCidFrom(pis));
    }

    private static String extractCidFrom(InputStream input) throws IOException {
        String cid = null;
        Pattern pattern = Pattern.compile("with CID (\\d+)");
        String line;
        try (BufferedReader out = new BufferedReader(new InputStreamReader(input))) {
            while ((line = out.readLine()) != null) {
                CLog.i("VM output: " + line);
                Matcher matcher = pattern.matcher(line);
                if (matcher.find()) {
                    cid = matcher.group(1);
                    break;
                }
            }
        }
        assertWithMessage("The output does not contain the expected pattern for CID.")
                .that(cid)
                .isNotNull();
        return cid;
    }

    @Test
    @CddTest(requirements = {"9.17/C-2-1", "9.17/C-2-2", "9.17/C-2-6"})
    public void protectedVmRunsPvmfw() throws Exception {
        // Arrange
        boolean protectedVm = true;
        assumeTrue(
                "Skip if protected VMs are not supported",
                getAndroidDevice().supportsMicrodroid(protectedVm));
        final String configPath = "assets/vm_config_apex.json";

        // Act
        mMicrodroidDevice =
                MicrodroidBuilder.fromDevicePath(getPathForPackage(PACKAGE_NAME), configPath)
                        .debugLevel("full")
                        .memoryMib(minMemorySize())
                        .numCpus(NUM_VCPUS)
                        .protectedVm(protectedVm)
                        .build(getAndroidDevice());

        // Assert
        mMicrodroidDevice.waitForBootComplete(BOOT_COMPLETE_TIMEOUT);
        String consoleLog = getDevice().pullFileContents(CONSOLE_PATH);
        assertWithMessage("Failed to verify that pvmfw started")
                .that(consoleLog)
                .contains("pVM firmware");
        assertWithMessage("pvmfw failed to start kernel")
                .that(consoleLog)
                .contains("Starting payload...");
        // TODO(b/260994818): Investigate the feasibility of checking DeathReason.
    }

    @Test
    @CddTest(requirements = {"9.17/C-2-1", "9.17/C-2-2", "9.17/C-2-6"})
    public void protectedVmWithImageSignedWithDifferentKeyRunsPvmfw() throws Exception {
        // Arrange
        boolean protectedVm = true;
        assumeTrue(
                "Skip if protected VMs are not supported",
                getAndroidDevice().supportsMicrodroid(protectedVm));
        File key = findTestFile("test.com.android.virt.pem");

        // Act
        VmInfo vmInfo =
                runMicrodroidWithResignedImages(key, /*keyOverrides=*/ Map.of(), protectedVm);

        // Assert
        vmInfo.mProcess.waitFor(5L, TimeUnit.SECONDS);
        String consoleLog = getDevice().pullFileContents(CONSOLE_PATH);
        assertWithMessage("pvmfw should start").that(consoleLog).contains("pVM firmware");
        // TODO(b/256148034): Asserts that pvmfw run fails when this verification is implemented.
        // Also rename the test.
        vmInfo.mProcess.destroy();
    }

    // TODO(b/245277660): Resigning the system/vendor image changes the vbmeta hash.
    // So, unless vbmeta related bootconfigs are updated the following test will fail
    @Test
    @Ignore("b/245277660")
    @CddTest(requirements = {"9.17/C-2-2", "9.17/C-2-6"})
    public void testBootSucceedsWhenNonProtectedVmStartsWithImagesSignedWithDifferentKey()
            throws Exception {
        File key = findTestFile("test.com.android.virt.pem");
        Map<String, File> keyOverrides = Map.of();
        VmInfo vmInfo = runMicrodroidWithResignedImages(key, keyOverrides, /*isProtected=*/ false);
        // Device online means that boot must have succeeded.
        adbConnectToMicrodroid(getDevice(), vmInfo.mCid);
        vmInfo.mProcess.destroy();
    }

    @Test
    @CddTest(requirements = {"9.17/C-2-2", "9.17/C-2-6"})
    public void testBootFailsWhenVbMetaDigestDoesNotMatchBootconfig() throws Exception {
        // Sign everything with key1 except vbmeta
        File key = findTestFile("test.com.android.virt.pem");
        File key2 = findTestFile("test2.com.android.virt.pem");
        Map<String, File> keyOverrides = Map.of("microdroid_vbmeta.img", key2);
        // To be able to stop it, it should be a daemon.
        VmInfo vmInfo = runMicrodroidWithResignedImages(key, keyOverrides, /*isProtected=*/ false);
        // Wait so that init can print errors to console (time in cuttlefish >> in real device)
        assertThatEventually(
                100000,
                () -> getDevice().pullFileContents(CONSOLE_PATH),
                containsString("init: [libfs_avb]Failed to verify vbmeta digest"));
        vmInfo.mProcess.destroy();
    }

    private boolean isTombstoneGenerated(String configPath, String... crashCommand)
            throws Exception {
        // Note this test relies on logcat values being printed by tombstone_transmit on
        // and the reeceiver on host (virtualization_service)
        mMicrodroidDevice =
                MicrodroidBuilder.fromDevicePath(getPathForPackage(PACKAGE_NAME), configPath)
                        .debugLevel("full")
                        .memoryMib(minMemorySize())
                        .numCpus(NUM_VCPUS)
                        .build(getAndroidDevice());
        mMicrodroidDevice.waitForBootComplete(BOOT_COMPLETE_TIMEOUT);
        mMicrodroidDevice.enableAdbRoot();

        CommandRunner microdroid = new CommandRunner(mMicrodroidDevice);
        microdroid.run(crashCommand);

        // check until microdroid is shut down
        CommandRunner android = new CommandRunner(getDevice());
        // TODO: improve crosvm exit check. b/258848245
        android.runWithTimeout(
                15000,
                "logcat",
                "-m",
                "1",
                "-e",
                "'virtualizationservice::crosvm.*exited with status exit status:'");

        // Check that tombstone is received (from host logcat)
        String ramdumpRegex =
                "Received [0-9]+ bytes from guest & wrote to tombstone file|"
                        + "Ramdump \"[^ ]+/ramdump\" sent to tombstoned";

        String result =
                tryRunOnHost(
                        "timeout",
                        "10s",
                        "adb",
                        "-s",
                        getDevice().getSerialNumber(),
                        "logcat",
                        "-m",
                        "1",
                        "-e",
                        ramdumpRegex);
        return !result.trim().isEmpty();
    }

    @Test
    public void testTombstonesAreGeneratedUponUserspaceCrash() throws Exception {
        assertThat(
                        isTombstoneGenerated(
                                "assets/vm_config_crash.json",
                                "kill",
                                "-SIGSEGV",
                                "$(pidof microdroid_launcher)"))
                .isTrue();
    }

    @Test
    public void testTombstonesAreNotGeneratedIfNotExportedUponUserspaceCrash() throws Exception {
        assertThat(
                        isTombstoneGenerated(
                                "assets/vm_config_crash_no_tombstone.json",
                                "kill",
                                "-SIGSEGV",
                                "$(pidof microdroid_launcher)"))
                .isFalse();
    }

    @Test
    public void testTombstonesAreGeneratedUponKernelCrash() throws Exception {
        assumeFalse("Cuttlefish is not supported", isCuttlefish());
        assertThat(
                        isTombstoneGenerated(
                                "assets/vm_config_crash.json",
                                "echo",
                                "c",
                                ">",
                                "/proc/sysrq-trigger"))
                .isTrue();
    }

    @Test
    public void testTelemetryPushedAtoms() throws Exception {
        // Reset statsd config and report before the test
        ConfigUtils.removeConfig(getDevice());
        ReportUtils.clearReports(getDevice());

        // Setup statsd config
        int[] atomIds = {
            AtomsProto.Atom.VM_CREATION_REQUESTED_FIELD_NUMBER,
            AtomsProto.Atom.VM_BOOTED_FIELD_NUMBER,
            AtomsProto.Atom.VM_EXITED_FIELD_NUMBER,
        };
        ConfigUtils.uploadConfigForPushedAtoms(getDevice(), PACKAGE_NAME, atomIds);

        // Create VM with microdroid
        TestDevice device = getAndroidDevice();
        final String configPath = "assets/vm_config_apex.json"; // path inside the APK
        ITestDevice microdroid =
                MicrodroidBuilder.fromDevicePath(getPathForPackage(PACKAGE_NAME), configPath)
                        .debugLevel("full")
                        .memoryMib(minMemorySize())
                        .numCpus(NUM_VCPUS)
                        .build(device);
        microdroid.waitForBootComplete(BOOT_COMPLETE_TIMEOUT);
        device.shutdownMicrodroid(microdroid);

        List<StatsLog.EventMetricData> data = new ArrayList<>();
        assertThatEventually(
                10000,
                () -> {
                    data.addAll(ReportUtils.getEventMetricDataList(getDevice()));
                    return data.size();
                },
                is(3)
        );

        // Check VmCreationRequested atom
        assertThat(data.get(0).getAtom().getPushedCase().getNumber()).isEqualTo(
                AtomsProto.Atom.VM_CREATION_REQUESTED_FIELD_NUMBER);
        AtomsProto.VmCreationRequested atomVmCreationRequested =
                data.get(0).getAtom().getVmCreationRequested();
        assertThat(atomVmCreationRequested.getHypervisor())
                .isEqualTo(AtomsProto.VmCreationRequested.Hypervisor.PKVM);
        assertThat(atomVmCreationRequested.getIsProtected()).isFalse();
        assertThat(atomVmCreationRequested.getCreationSucceeded()).isTrue();
        assertThat(atomVmCreationRequested.getBinderExceptionCode()).isEqualTo(0);
        assertThat(atomVmCreationRequested.getVmIdentifier()).isEqualTo("VmRunApp");
        assertThat(atomVmCreationRequested.getConfigType())
                .isEqualTo(AtomsProto.VmCreationRequested.ConfigType.VIRTUAL_MACHINE_APP_CONFIG);
        assertThat(atomVmCreationRequested.getNumCpus()).isEqualTo(NUM_VCPUS);
        assertThat(atomVmCreationRequested.getMemoryMib()).isEqualTo(minMemorySize());
        assertThat(atomVmCreationRequested.getApexes())
                .isEqualTo("com.android.art:com.android.compos:com.android.sdkext");

        // Check VmBooted atom
        assertThat(data.get(1).getAtom().getPushedCase().getNumber())
                .isEqualTo(AtomsProto.Atom.VM_BOOTED_FIELD_NUMBER);
        AtomsProto.VmBooted atomVmBooted = data.get(1).getAtom().getVmBooted();
        assertThat(atomVmBooted.getVmIdentifier()).isEqualTo("VmRunApp");

        // Check VmExited atom
        assertThat(data.get(2).getAtom().getPushedCase().getNumber())
                .isEqualTo(AtomsProto.Atom.VM_EXITED_FIELD_NUMBER);
        AtomsProto.VmExited atomVmExited = data.get(2).getAtom().getVmExited();
        assertThat(atomVmExited.getVmIdentifier()).isEqualTo("VmRunApp");
        assertThat(atomVmExited.getDeathReason()).isEqualTo(AtomsProto.VmExited.DeathReason.KILLED);

        // Check UID and elapsed_time by comparing each other.
        assertThat(atomVmBooted.getUid()).isEqualTo(atomVmCreationRequested.getUid());
        assertThat(atomVmExited.getUid()).isEqualTo(atomVmCreationRequested.getUid());
        assertThat(atomVmBooted.getElapsedTimeMillis())
                .isLessThan(atomVmExited.getElapsedTimeMillis());
    }

    @Test
    @CddTest(requirements = {"9.17/C-1-1", "9.17/C-1-2", "9.17/C/1-3"})
    public void testMicrodroidBoots() throws Exception {
        final String configPath = "assets/vm_config.json"; // path inside the APK
        mMicrodroidDevice =
                MicrodroidBuilder.fromDevicePath(getPathForPackage(PACKAGE_NAME), configPath)
                        .debugLevel("full")
                        .memoryMib(minMemorySize())
                        .numCpus(NUM_VCPUS)
                        .build(getAndroidDevice());
        mMicrodroidDevice.waitForBootComplete(BOOT_COMPLETE_TIMEOUT);
        CommandRunner microdroid = new CommandRunner(mMicrodroidDevice);

        // Test writing to /data partition
        microdroid.run("echo MicrodroidTest > /data/local/tmp/test.txt");
        assertThat(microdroid.run("cat /data/local/tmp/test.txt")).isEqualTo("MicrodroidTest");

        // Check if the APK & its idsig partitions exist
        final String apkPartition = "/dev/block/by-name/microdroid-apk";
        assertThat(microdroid.run("ls", apkPartition)).isEqualTo(apkPartition);
        final String apkIdsigPartition = "/dev/block/by-name/microdroid-apk-idsig";
        assertThat(microdroid.run("ls", apkIdsigPartition)).isEqualTo(apkIdsigPartition);
        // Check the vm-instance partition as well
        final String vmInstancePartition = "/dev/block/by-name/vm-instance";
        assertThat(microdroid.run("ls", vmInstancePartition)).isEqualTo(vmInstancePartition);

        // Check if the native library in the APK is has correct filesystem info
        final String[] abis = microdroid.run("getprop", "ro.product.cpu.abilist").split(",");
        assertThat(abis).hasLength(1);
        final String testLib = "/mnt/apk/lib/" + abis[0] + "/MicrodroidTestNativeLib.so";
        final String label = "u:object_r:system_file:s0";
        assertThat(microdroid.run("ls", "-Z", testLib)).isEqualTo(label + " " + testLib);

        // Check that no denials have happened so far
        CommandRunner android = new CommandRunner(getDevice());
        assertThat(android.tryRun("egrep", "'avc:[[:space:]]{1,2}denied'", LOG_PATH)).isNull();
        assertThat(android.tryRun("egrep", "'avc:[[:space:]]{1,2}denied'", CONSOLE_PATH)).isNull();

        assertThat(microdroid.run("cat /proc/cpuinfo | grep processor | wc -l"))
                .isEqualTo(Integer.toString(NUM_VCPUS));

        // Check that selinux is enabled
        assertThat(microdroid.run("getenforce")).isEqualTo("Enforcing");

        // TODO(b/176805428): adb is broken for nested VM
        if (!isCuttlefish()) {
            // Check neverallow rules on microdroid
            File policyFile = mMicrodroidDevice.pullFile("/sys/fs/selinux/policy");
            File generalPolicyConfFile = findTestFile("microdroid_general_sepolicy.conf");
            File sepolicyAnalyzeBin = findTestFile("sepolicy-analyze");

            CommandResult result =
                    RunUtil.getDefault()
                            .runTimedCmd(
                                    10000,
                                    sepolicyAnalyzeBin.getPath(),
                                    policyFile.getPath(),
                                    "neverallow",
                                    "-w",
                                    "-f",
                                    generalPolicyConfFile.getPath());
            assertWithMessage("neverallow check failed: " + result.getStderr().trim())
                    .about(command_results())
                    .that(result)
                    .isSuccess();
        }
    }

    @Test
    public void testMicrodroidRamUsage() throws Exception {
        final String configPath = "assets/vm_config.json";
        mMicrodroidDevice =
                MicrodroidBuilder.fromDevicePath(getPathForPackage(PACKAGE_NAME), configPath)
                        .debugLevel("full")
                        .memoryMib(minMemorySize())
                        .numCpus(NUM_VCPUS)
                        .build(getAndroidDevice());
        mMicrodroidDevice.waitForBootComplete(BOOT_COMPLETE_TIMEOUT);
        mMicrodroidDevice.enableAdbRoot();

        CommandRunner microdroid = new CommandRunner(mMicrodroidDevice);
        Function<String, String> microdroidExec =
                (cmd) -> {
                    try {
                        return microdroid.run(cmd);
                    } catch (Exception ex) {
                        throw new IllegalStateException(ex);
                    }
                };

        for (Map.Entry<String, Long> stat :
                ProcessUtil.getProcessMemoryMap(microdroidExec).entrySet()) {
            mMetrics.addTestMetric(
                    mMetricPrefix + "meminfo/" + stat.getKey().toLowerCase(),
                    stat.getValue().toString());
        }

        for (Map.Entry<Integer, String> proc :
                ProcessUtil.getProcessMap(microdroidExec).entrySet()) {
            for (Map.Entry<String, Long> stat :
                    ProcessUtil.getProcessSmapsRollup(proc.getKey(), microdroidExec)
                            .entrySet()) {
                String name = stat.getKey().toLowerCase();
                mMetrics.addTestMetric(
                        mMetricPrefix + "smaps/" + name + "/" + proc.getValue(),
                        stat.getValue().toString());
            }
        }
    }

    @Test
    public void testCustomVirtualMachinePermission() throws Exception {
        assumeTrue(
                "Protected VMs are not supported",
                getAndroidDevice().supportsMicrodroid(/*protectedVm=*/ true));
        CommandRunner android = new CommandRunner(getDevice());

        // Pull etc/microdroid.json
        File virtApexDir = FileUtil.createTempDir("virt_apex");
        File microdroidConfigFile = new File(virtApexDir, "microdroid.json");
        assertThat(getDevice().pullFile(VIRT_APEX + "etc/microdroid.json", microdroidConfigFile))
                .isTrue();
        JSONObject config = new JSONObject(FileUtil.readStringFromFile(microdroidConfigFile));

        // USE_CUSTOM_VIRTUAL_MACHINE is enforced only on protected mode
        config.put("protected", true);

        // Write updated config
        final String configPath = TEST_ROOT + "raw_config.json";
        getDevice().pushString(config.toString(), configPath);

        // temporarily revoke the permission
        android.run(
                "pm",
                "revoke",
                SHELL_PACKAGE_NAME,
                "android.permission.USE_CUSTOM_VIRTUAL_MACHINE");
        final String ret =
                android.runForResult(VIRT_APEX + "bin/vm run", configPath).getStderr().trim();

        assertThat(ret)
                .contains(
                        "does not have the android.permission.USE_CUSTOM_VIRTUAL_MACHINE"
                                + " permission");
    }

    @Test
    public void testPathToBinaryIsRejected() throws Exception {
        CommandRunner android = new CommandRunner(getDevice());

        // Create the idsig file for the APK
        final String apkPath = getPathForPackage(PACKAGE_NAME);
        final String idSigPath = TEST_ROOT + "idsig";
        android.run(VIRT_APEX + "bin/vm", "create-idsig", apkPath, idSigPath);

        // Create the instance image for the VM
        final String instanceImgPath = TEST_ROOT + "instance.img";
        android.run(
                VIRT_APEX + "bin/vm",
                "create-partition",
                "--type instance",
                instanceImgPath,
                Integer.toString(10 * 1024 * 1024));

        final String ret =
                android.runForResult(
                                VIRT_APEX + "bin/vm",
                                "run-app",
                                "--payload-binary-name",
                                "./MicrodroidTestNativeLib.so",
                                apkPath,
                                idSigPath,
                                instanceImgPath)
                        .getStderr()
                        .trim();

        assertThat(ret).contains("Payload binary name must not specify a path");
    }

    @Before
    public void setUp() throws Exception {
        testIfDeviceIsCapable(getDevice());
        mMetricPrefix = getMetricPrefix() + "microdroid/";
        mMicrodroidDevice = null;

        prepareVirtualizationTestSetup(getDevice());

        getDevice().installPackage(findTestFile(APK_NAME), /* reinstall */ false);

        // clear the log
        getDevice().executeShellV2Command("logcat -c");
    }

    @After
    public void shutdown() throws Exception {
        if (mMicrodroidDevice != null) {
            getAndroidDevice().shutdownMicrodroid(mMicrodroidDevice);
        }

        cleanUpVirtualizationTestSetup(getDevice());

        archiveLogThenDelete(
                mTestLogs, getDevice(), LOG_PATH, "vm.log-" + mTestName.getMethodName());

        getDevice().uninstallPackage(PACKAGE_NAME);

        // testCustomVirtualMachinePermission revokes this permission. Grant it again as cleanup
        new CommandRunner(getDevice())
                .tryRun(
                        "pm",
                        "grant",
                        SHELL_PACKAGE_NAME,
                        "android.permission.USE_CUSTOM_VIRTUAL_MACHINE");
    }

    private TestDevice getAndroidDevice() {
        TestDevice androidDevice = (TestDevice) getDevice();
        assertThat(androidDevice).isNotNull();
        return androidDevice;
    }
}
