// Stub for android.content.AttributionSourceState — the real one lives
// in frameworks/base/core/java/android/content/. Referenced by
// aaudio.StreamRequest. An attempted full-shape version in B5 failed
// with BAD_TYPE because rsbinder-aidl 0.7.0 can't emit the recursive
// `AttributionSourceState[] next` field (Vec<Box<Self>> lacks
// Serialize/Deserialize), and substituting `int[]` for `next` broke
// the wire format. Reverting to empty: the AAudio service auto-fills
// pid/uid from the binder caller context, which gets us as far as
// the SHARED-mode dispatch — the remaining MMAP-only behavior on
// the Pixel 2 XL is a device limitation, not a parcel-shape issue.
package android.content;
parcelable AttributionSourceState;
