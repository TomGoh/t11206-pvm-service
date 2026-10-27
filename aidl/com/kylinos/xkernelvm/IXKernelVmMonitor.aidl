/*
 * Copyright 2026 KylinSoft Co., Ltd.
 * SPDX-License-Identifier: Apache-2.0
 */

package com.kylinos.xkernelvm;

import com.kylinos.xkernelvm.El2Counters;
import com.kylinos.xkernelvm.VmCpuTime;

/**
 * Read-only figures for monitor apps: the hypervisor's EL2 cycles per CPU and the CPU time of the
 * x-kernel VM. Every value is cumulative; a client computes rates from two samples. Served by the
 * native control service next to IXKernelVmControl; nothing here changes the VM.
 *
 * Callers need uid 0 or 1000, or the permission com.kylinos.xkernelvm.permission.READ_VM_STATS
 * (or CONTROL_VM).
 */
interface IXKernelVmMonitor {
    /** El2Counters.status: samples holds every CPU whose counter could be read. */
    const int EL2_COUNTING = 0;
    /** El2Counters.status: no counter can be read now; reason says why. */
    const int EL2_UNAVAILABLE = 1;

    /**
     * The EL2 cycle counters. Reads never load or prepare the driver. The first read starts
     * counters only when the VM lifecycle allows it and the driver is already available;
     * otherwise EL2_UNAVAILABLE explains why. Counters stop once no client has asked for a while,
     * so poll at least every few seconds while showing them.
     */
    El2Counters getEl2Counters();

    /** The CPU time the x-kernel VM has used so far. */
    VmCpuTime getVmCpuTime();
}
