/*
 * Copyright 2026 KylinSoft Co., Ltd.
 * SPDX-License-Identifier: Apache-2.0
 */

package com.kylinos.xkernelvm;

import com.kylinos.xkernelvm.VmState;

/** A consistent snapshot of the x-kernel VM instance. */
@RustDerive(Clone=true, PartialEq=true)
parcelable VmStatus {
    VmState state = VmState.STOPPED;
    /** Identifies one run of the control service; a new value means the service restarted. */
    @utf8InCpp String sessionId;
    /** Increases with every visible change within a session; drop snapshots with a lower seq. */
    long seq;
    /** The operation in progress, or the last one. */
    long operationId;
    /** Numbers the instances of a session: the current one, or the last (0 before the first). */
    long instance;
    /** Detail for UNAVAILABLE, FAILED, RECLAIM_UNKNOWN, and a timed-out readiness handshake. */
    @utf8InCpp String reason;
    /** The last instance is confirmed gone (true in STOPPED). */
    boolean reclaimConfirmed;
    /** The VM's vsock CID while it runs, else -1. */
    int cid = -1;
    /** CLOCK_BOOTTIME milliseconds (SystemClock.elapsedRealtime) when AVF started the VM, 0 if none runs. */
    long startedAtElapsedMs;
    /** How the last instance ended; empty before the first one ended. */
    @utf8InCpp String lastExit;

    /** Configured guest memory, not measured use. */
    int memoryMib;
    /** Configured vCPU count. */
    int cpuCount;
    /** The instance is a protected VM. */
    boolean protectedVm;
    /** The protected VM runs without guest firmware: the image is not authenticated by pvmfw. */
    boolean withoutFirmware;
    /** A DICE chain is handed to the guest. Always false in this release. */
    boolean diceAvailable;
    /** The instance has a readiness handshake; without one it never reaches READY. */
    boolean readinessConfigured;
    /** AVF runs the VM; false when the service runs crosvm itself (the direct backend). */
    boolean viaAvf;
}
