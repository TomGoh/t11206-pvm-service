/*
 * Copyright 2026 KylinSoft Co., Ltd.
 * SPDX-License-Identifier: Apache-2.0
 */

package com.kylinos.xkernelvm;

/** The EL2 cycle counter of one CPU. */
@RustDerive(Clone=true, PartialEq=true)
parcelable El2CycleSample {
    /**
     * The CPU, as the driver lists it: the hypervisor's CPU id. It equals Linux's CPU number on
     * T11206, and clients use it as such (e.g. to find the CPU's cpufreq policy).
     */
    int cpu;
    /**
     * Cycles this CPU has spent in EL2 while the counters ran, counted at the CPU's clock of the
     * moment: divide by the frequency the CPU ran at, not its maximum, to get time. A restart
     * continues from the stopped value; a count that drops (its counter was lost) is a new
     * baseline, not a negative interval.
     */
    long cycles;
    /** CLOCK_MONOTONIC nanoseconds when the counter was read, on that CPU. */
    long timestampNs;
}
