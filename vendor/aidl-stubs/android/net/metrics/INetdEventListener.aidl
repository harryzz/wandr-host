// Stub for android.net.metrics.INetdEventListener — the real one lives in
// frameworks/base (not vendored). Referenced by IDnsResolver.registerEventListener,
// which wart never calls (we only call setResolverConfiguration / createNetworkCache).
// A body-less interface satisfies rsbinder-aidl's type resolution + keeps the
// transaction codes aligned without pulling the real definition + its deps.
package android.net.metrics;
interface INetdEventListener {}
