/*
 * Copyright 2026 KylinSoft Co., Ltd.
 * SPDX-License-Identifier: Apache-2.0
 */

package com.kylinos.xkernelvm;

/**
 * A piece of the serial console output of one VM instance. Output is addressed by instance (the
 * VmStatus instance it belongs to) and offset (bytes of that instance's output before this
 * piece), so a client can drop what it has seen and notice a gap.
 */
@RustDerive(Clone=true, PartialEq=true)
parcelable ConsoleOutput {
    long instance;
    long offset;
    byte[] data;
}
