// Stub for android.os.PersistableBundle — the real definition lives in
// frameworks/base/core/java/android/os/PersistableBundle.aidl which we
// don't vendor. This stub satisfies AIDL imports from HAL AIDLs that
// reference PersistableBundle (e.g. android.hardware.vibrator.VendorEffect).
// We never construct or pass a PersistableBundle across binder so the
// missing fields/methods are harmless — the type is only used by the
// `performVendorEffect()` method which our host doesn't call.
package android.os;
parcelable PersistableBundle;
