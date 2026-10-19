/*
 * Copyright 2026 KylinSoft Co., Ltd.
 * SPDX-License-Identifier: Apache-2.0
 */

package com.kylinos.xkernelvm;

import com.kylinos.xkernelvm.El2CycleSample;

/** One reading of the hypervisor's EL2 cycle counters. */
@RustDerive(Clone=true, PartialEq=true)
parcelable El2Counters {
    /** IXKernelVmMonitor.EL2_COUNTING or EL2_UNAVAILABLE. */
    int status;
    /** Why no counter can be read; empty while counting. */
    @utf8InCpp String reason;
    /** One entry per online CPU whose counter was read, by CPU number. */
    El2CycleSample[] samples;
}
