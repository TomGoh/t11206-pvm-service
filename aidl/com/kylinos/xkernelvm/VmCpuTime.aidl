/*
 * Copyright 2026 KylinSoft Co., Ltd.
 * SPDX-License-Identifier: Apache-2.0
 */

package com.kylinos.xkernelvm;

/**
 * The CPU time of the x-kernel VM: user plus system time of its crosvm processes, all threads
 * (crosvm runs the vCPUs in a child of the process the service starts). The VM's vCPUs run on
 * these threads, so this includes the guest's own run time, the hypervisor's handling of its exits
 * and crosvm's device emulation.
 */
@RustDerive(Clone=true, PartialEq=true)
parcelable VmCpuTime {
    /** A VM process exists; the other fields are 0 when false. */
    boolean running;
    /** VmStatus.instance of the VM measured; a new value means a new VM, so start over. */
    long instance;
    /** Nanoseconds of CPU time so far (clock-tick resolution, 10 ms on Android). */
    long cpuTimeNs;
    /** CLOCK_MONOTONIC nanoseconds when cpuTimeNs was read. */
    long timestampNs;
    /**
     * Identifies the set of processes counted. It changes when a process comes or goes (e.g.
     * crosvm's vCPU child found after the start); cpuTimeNs then jumps or drops by that process's
     * total, so start over as for a new instance.
     */
    long processSet;
}
