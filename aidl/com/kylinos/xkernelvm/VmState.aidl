/*
 * Copyright 2026 KylinSoft Co., Ltd.
 * SPDX-License-Identifier: Apache-2.0
 */

package com.kylinos.xkernelvm;

/**
 * The visible state of the x-kernel VM instance. See VmStatus for the fields each state fills.
 */
@Backing(type="int")
enum VmState {
    /** No instance, and a start precondition fails (VmStatus.reason says which). */
    UNAVAILABLE = 0,
    /** No instance, and the last one is confirmed gone. */
    STOPPED = 1,
    /** The instance is being created and started through AVF. */
    STARTING = 2,
    /** AVF runs the VM; the guest has not completed the readiness handshake. */
    RUNNING_NOT_READY = 3,
    /** The guest completed the readiness handshake. */
    READY = 4,
    /** A stop, or the confirmation that the instance is gone, is in progress. */
    STOPPING = 5,
    /** The instance did not go away in time, or a process of it is still there. No start is accepted. */
    RECLAIM_UNKNOWN = 6,
    /** The instance failed or died unexpectedly, and is confirmed gone. A start may follow. */
    FAILED = 7,
}
