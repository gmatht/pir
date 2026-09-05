package com.example;

import android.view.View;

/**
 * Bridge class that Android can call when a user clicks a View.
 * The native method dispatchCallback is registered by Rust via JNI RegisterNatives
 * and invokes the corresponding Rust closure.
 */
public class RustCallback implements View.OnClickListener {
    private final long callbackId;

    public RustCallback(long id) {
        this.callbackId = id;
    }

    @Override
    public void onClick(View v) {
        nativeDispatchCallback(callbackId);
    }

    private static native void nativeDispatchCallback(long id);
}
