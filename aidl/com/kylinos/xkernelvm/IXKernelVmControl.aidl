/*
 * Copyright 2026 KylinSoft Co., Ltd.
 * SPDX-License-Identifier: Apache-2.0
 */

package com.kylinos.xkernelvm;

import com.kylinos.xkernelvm.ConsoleOutput;
import com.kylinos.xkernelvm.IXKernelVmCallback;
import com.kylinos.xkernelvm.IXKernelVmConsoleCallback;
import com.kylinos.xkernelvm.VmStatus;

/**
 * Control of the one x-kernel VM instance, served by the native control service, which is the
 * only owner of the VM. A client never holds the VM itself, so a client that exits or dies
 * leaves the VM as it is.
 *
 * Callers need uid 0 or 1000, or the permission com.kylinos.xkernelvm.permission.CONTROL_VM.
 */
interface IXKernelVmControl {
    /** Service-specific error: a start precondition fails; the status reason says which. */
    const int ERROR_UNAVAILABLE = 1;
    /** Service-specific error: the opposite operation is in progress. */
    const int ERROR_BUSY = 2;
    /** Service-specific error: the last instance is not confirmed gone; request a stop first. */
    const int ERROR_RECLAIM_PENDING = 3;
    /** Service-specific error: console input needs a running instance. */
    const int ERROR_NOT_RUNNING = 4;

    /** The current snapshot. Never creates or restarts the VM. */
    VmStatus getStatus();

    /**
     * Asks for the VM to be started and returns the operation that serves the request at once;
     * the outcome arrives through the callbacks. A request while the VM starts or runs returns
     * the current operation and creates nothing.
     */
    long requestStart();

    /**
     * Asks for the VM to be stopped and its resources confirmed gone, and returns the operation
     * that serves the request (0 when nothing runs). In RECLAIM_UNKNOWN it re-checks whether the
     * last instance is gone.
     */
    long requestStop();

    /**
     * Registers a callback and returns the snapshot it starts from; every later change is
     * delivered to it, so nothing falls between the two. A dead callback is dropped.
     */
    VmStatus registerCallback(IXKernelVmCallback callback);

    void unregisterCallback(IXKernelVmCallback callback);

    /**
     * Attaches to the serial console of the latest instance: returns what the service kept of
     * its output (the last 64 KiB) and delivers every later piece to the callback, so nothing
     * falls between the two. Attaching a callback that is attached already resynchronises it.
     * The console of an instance stays readable after it stopped, until the next start.
     */
    ConsoleOutput attachConsole(IXKernelVmConsoleCallback callback);

    void detachConsole(IXKernelVmConsoleCallback callback);

    /**
     * Types into the serial console of the running instance; the guest echoes what it reads.
     * At most 4096 bytes. ERROR_NOT_RUNNING when no instance runs, ERROR_BUSY when the guest has
     * not read the earlier input (nothing of this input, or only a part, was taken).
     */
    void writeConsole(in byte[] data);
}
