/*
 * Copyright 2026 KylinSoft Co., Ltd.
 * SPDX-License-Identifier: Apache-2.0
 */

package com.kylinos.xkernelvm;

/** The EL2 cycle counter of one CPU. */
@RustDerive(Clone=true, PartialEq=true)
parcelable El2CycleSample {
    /** The CPU number, as Linux numbers it. */
    int cpu;
    /**
     * Cycles this CPU has spent in EL2 since the counters started, counted at the CPU's clock
     * of the moment: divide by the frequency the CPU ran at, not its maximum, to get time. Starts
     * again from a lower value when the counters restart.
     */
    long cycles;
    /** CLOCK_MONOTONIC nanoseconds when the counter was read, on that CPU. */
    long timestampNs;
}
