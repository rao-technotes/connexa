plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "app.connexa.android"
    compileSdk = 34

    defaultConfig {
        applicationId = "app.connexa.android"
        minSdk = 26
        targetSdk = 34
        versionCode = 1
        versionName = "0.1.0"
    }

    buildFeatures {
        buildConfig = true
    }

    buildTypes {
        release {
            isMinifyEnabled = false
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = "17"
    }
}

// The web client is bundled from clients/ (npm run build:android) into assets/www.
val webAssets = file("src/main/assets/www/index.html")
tasks.named("preBuild") {
    doFirst {
        check(webAssets.exists()) {
            "Missing web assets. Run `npm run build:android` in clients/ first."
        }
    }
}

dependencies {
    implementation("androidx.webkit:webkit:1.11.0")
}
