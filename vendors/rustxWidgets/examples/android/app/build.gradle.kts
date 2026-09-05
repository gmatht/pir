plugins {
    id("com.android.application")
    kotlin("android") version "1.9.0" apply false
}

android {
    namespace = "com.example"
    compileSdk = 33

    defaultConfig {
        applicationId = "com.example"
        minSdk = 24
        targetSdk = 33
        versionCode = 1
        versionName = "1.0"

        ndk {
            abiFilters += listOf("x86_64")
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
        }
    }

    sourceSets {
        getByName("main") {
            jniLibs.srcDirs("../../../rustxwidgets/target/x86_64-linux-android/release/")
        }
    }
}

dependencies {
    implementation("androidx.appcompat:appcompat:1.6.1")
}
