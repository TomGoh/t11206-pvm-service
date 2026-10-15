/*
 * Copyright 2026 KylinSoft Co., Ltd.
 * SPDX-License-Identifier: Apache-2.0
 */

package com.kylinos.xkernelvm;

import com.kylinos.xkernelvm.ConsoleOutput;

/** Receives the serial console output of the x-kernel VM. */
oneway interface IXKernelVmConsoleCallback {
    /**
     * New output. A piece at offset 0 with no data starts the console of a new instance: the
     * client clears its view. A piece that does not start where the client's view ends means
     * output was lost; the client attaches again to resynchronise.
     */
    void onConsoleOutput(in ConsoleOutput output);
}
