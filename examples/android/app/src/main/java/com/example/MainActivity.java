package com.example;

import android.app.Activity;
import android.os.Bundle;
import android.widget.LinearLayout;
import android.widget.TextView;

public class MainActivity extends Activity {
    static {
        System.loadLibrary("rustxwidgets_android_demo");
    }

    // Pass the activity AND its content view to Rust
    private static native void nativeInit(Activity activity, LinearLayout rootLayout);

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);

        // Create a root layout that Rust will populate
        LinearLayout root = new LinearLayout(this);
        root.setOrientation(LinearLayout.VERTICAL);
        setContentView(root);

        // Call Rust to populate the UI with both activity and root layout
        nativeInit(this, root);
    }
}
