/*
 * Copyright 2026 KylinSoft Co., Ltd.
 * SPDX-License-Identifier: Apache-2.0
 */

package com.kylinos.xkernelvm;

import com.kylinos.xkernelvm.VmStatus;

/** Receives every visible change of the instance. */
oneway interface IXKernelVmCallback {
    void onStatusChanged(in VmStatus status);
}
