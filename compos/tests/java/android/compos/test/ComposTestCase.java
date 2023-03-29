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

package android.compos.test;

import static com.android.microdroid.test.host.CommandResultSubject.assertThat;
import static com.android.microdroid.test.host.CommandResultSubject.command_results;
import static com.android.tradefed.testtype.DeviceJUnit4ClassRunner.TestLogData;

import static com.google.common.truth.Truth.assertThat;
import static com.google.common.truth.Truth.assertWithMessage;

import android.platform.test.annotations.RootPermissionTest;

import com.android.microdroid.test.host.CommandRunner;
import com.android.microdroid.test.host.MicrodroidHostTestCaseBase;
import com.android.tradefed.log.LogUtil.CLog;
import com.android.tradefed.result.FileInputStreamSource;
import com.android.tradefed.result.LogDataType;
import com.android.tradefed.testtype.DeviceJUnit4ClassRunner;
import com.android.tradefed.util.CommandResult;
import com.android.tradefed.util.RunUtil;

import org.junit.After;
import org.junit.Before;
import org.junit.Rule;
import org.junit.Test;
import org.junit.rules.TestName;
import org.junit.runner.RunWith;

import java.io.File;

@RootPermissionTest
@RunWith(DeviceJUnit4ClassRunner.class)
public final class ComposTestCase extends MicrodroidHostTestCaseBase {

    // Binaries used in test. (These paths are valid both in host and Microdroid.)
    private static final String ODREFRESH_BIN = "/apex/com.android.art/bin/odrefresh";
    private static final String COMPOSD_CMD_BIN = "/apex/com.android.compos/bin/composd_cmd";
    private static final String COMPOS_VERIFY_BIN =
            "/apex/com.android.compos/bin/compos_verify";

    private static final String COMPOS_APEXDATA_DIR = "/data/misc/apexdata/com.android.compos";

    /** Output directory of odrefresh */
    private static final String TEST_ARTIFACTS_DIR = "test-artifacts";

    private static final String ODREFRESH_OUTPUT_DIR =
            "/data/misc/apexdata/com.android.art/" + TEST_ARTIFACTS_DIR;

    /** Timeout of odrefresh to finish */
    private static final int ODREFRESH_TIMEOUT_MS = 10 * 60 * 1000; // 10 minutes

    // ExitCode expanded from art/odrefresh/include/odrefresh/odrefresh.h.
    private static final int OKAY = 0;
    private static final int COMPILATION_SUCCESS = 80;

    // Files that define the "test" instance of CompOS
    private static final String COMPOS_TEST_ROOT = "/data/misc/apexdata/com.android.compos/test/";

    private static final String SYSTEM_SERVER_COMPILER_FILTER_PROP_NAME =
            "dalvik.vm.systemservercompilerfilter";
    private String mBackupSystemServerCompilerFilter;

    @Rule public TestLogData mTestLogs = new TestLogData();
    @Rule public TestName mTestName = new TestName();

    @Before
    public void setUp() throws Exception {
        testIfDeviceIsCapable(getDevice());

        String value = getDevice().getProperty(SYSTEM_SERVER_COMPILER_FILTER_PROP_NAME);
        if (value == null) {
            mBackupSystemServerCompilerFilter = "";
        } else {
            mBackupSystemServerCompilerFilter = value;
        }
    }

    @After
    public void tearDown() throws Exception {
        killVmAndReconnectAdb();

        CommandRunner android = new CommandRunner(getDevice());

        // Clear up any CompOS instance files we created
        android.tryRun("rm", "-rf", COMPOS_TEST_ROOT);

        // And any artifacts generated by odrefresh
        android.tryRun("rm", "-rf", ODREFRESH_OUTPUT_DIR);

        if (mBackupSystemServerCompilerFilter != null) {
            CLog.d("Restore dalvik.vm.systemservercompilerfilter to "
                    + mBackupSystemServerCompilerFilter);
            getDevice().setProperty(SYSTEM_SERVER_COMPILER_FILTER_PROP_NAME,
                    mBackupSystemServerCompilerFilter);
        }
    }

    @Test
    public void testOdrefreshSpeed() throws Exception {
        getDevice().setProperty(SYSTEM_SERVER_COMPILER_FILTER_PROP_NAME, "speed");
        testOdrefresh();
    }

    @Test
    public void testOdrefreshSpeedProfile() throws Exception {
        getDevice().setProperty(SYSTEM_SERVER_COMPILER_FILTER_PROP_NAME, "speed-profile");
        testOdrefresh();
    }

    private void testOdrefresh() throws Exception {
        CommandRunner android = new CommandRunner(getDevice());

        // Prepare the groundtruth. The compilation on Android should finish successfully.
        {
            long start = System.currentTimeMillis();
            CommandResult result = runOdrefresh(android, "--force-compile");
            long elapsed = System.currentTimeMillis() - start;
            assertThat(result).exitCode().isEqualTo(COMPILATION_SUCCESS);
            CLog.i("Local compilation took " + elapsed + "ms");
        }

        // Save the expected checksum for the output directory.
        String expectedChecksumSnapshot = checksumDirectoryContentPartial(android,
                ODREFRESH_OUTPUT_DIR);

        // --check may delete the output.
        CommandResult result = runOdrefresh(android, "--check");
        assertThat(result).exitCode().isEqualTo(OKAY);

        // Expect the compilation in Compilation OS to finish successfully.
        {
            long start = System.currentTimeMillis();
            result =
                    android.runForResultWithTimeout(
                            ODREFRESH_TIMEOUT_MS, COMPOSD_CMD_BIN, "test-compile");
            long elapsed = System.currentTimeMillis() - start;
            assertThat(result).exitCode().isEqualTo(0);
            CLog.i("Comp OS compilation took " + elapsed + "ms");
        }
        killVmAndReconnectAdb();

        // Expect the BCC extracted from the BCC to be well-formed.
        assertVmBccIsValid();

        // Save the actual checksum for the output directory.
        String actualChecksumSnapshot = checksumDirectoryContentPartial(android,
                ODREFRESH_OUTPUT_DIR);

        // Expect the output of Comp OS to be the same as compiled on Android.
        assertThat(actualChecksumSnapshot).isEqualTo(expectedChecksumSnapshot);

        // Expect extra files generated by CompOS exist.
        android.run("test -f " + ODREFRESH_OUTPUT_DIR + "/compos.info");
        android.run("test -f " + ODREFRESH_OUTPUT_DIR + "/compos.info.signature");

        // Expect the CompOS signature to be valid
        android.run(COMPOS_VERIFY_BIN + " --debug --instance test");
    }

    private void assertVmBccIsValid() throws Exception {
        File bcc_file = getDevice().pullFile(COMPOS_APEXDATA_DIR + "/test/bcc");
        assertThat(bcc_file).isNotNull();

        // Add the BCC to test artifacts, in case it is ill-formed or otherwise interesting.
        mTestLogs.addTestLog(bcc_file.getPath(), LogDataType.UNKNOWN,
                new FileInputStreamSource(bcc_file));

        // Find the validator binary - note that it's specified as a dependency in our Android.bp.
        File validator = getTestInformation().getDependencyFile("hwtrust", /*targetFirst=*/ false);

        CommandResult result =
                new RunUtil()
                        .runTimedCmd(
                                10000,
                                validator.getAbsolutePath(),
                                "verify-dice-chain",
                                bcc_file.getAbsolutePath());
        assertWithMessage("hwtrust failed").about(command_results()).that(result).isSuccess();
    }

    private CommandResult runOdrefresh(CommandRunner android, String command) throws Exception {
        return android.runForResultWithTimeout(
                ODREFRESH_TIMEOUT_MS,
                ODREFRESH_BIN,
                "--dalvik-cache=" + TEST_ARTIFACTS_DIR,
                command);
    }

    private void killVmAndReconnectAdb() throws Exception {
        CommandRunner android = new CommandRunner(getDevice());

        android.tryRun("killall", "crosvm");
        android.tryRun("stop", "virtualizationservice");

        // Delete stale data
        android.tryRun("rm", "-rf", "/data/misc/virtualizationservice/*");
    }

    private String checksumDirectoryContentPartial(CommandRunner runner, String path)
            throws Exception {
        // Sort by filename (second column) to make comparison easier. Filter out compos.info and
        // compos.info.signature since it's only generated by CompOS.
        // TODO(b/211458160): Remove cache-info.xml once we can plumb timestamp and isFactory of
        // APEXes to the VM.
        return runner.run("cd " + path + "; find -type f -exec sha256sum {} \\;"
                + "| grep -v cache-info.xml | grep -v compos.info"
                + "| sort -k2");
    }
}
