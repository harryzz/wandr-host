//! Host-delegated TLS for guests, with Signal's pinned CA trusted (task 66).
//!
//! Wires `wasi:tls@0.2.0-draft` into every guest linker via a custom
//! [`wasmtime_wasi_tls::TlsProvider`] whose trust store is the webpki public
//! roots PLUS Signal's self-signed service CA. A guest (e.g. a future Signal
//! client) then reaches the network over host-delegated `wasi:sockets` +
//! `wasi:tls` with **no TLS/crypto compiled into the guest**. Proven end-to-end
//! in `repros/wasi-tls-{probe,runner}` before this wiring.
//!
//! Runs on wandr-host's *sync* store: the `wasi-tls` host fns are sync and spawn
//! the async connect via `wasmtime_wasi::runtime::spawn` onto the ambient tokio
//! runtime, with the sync `wasi:io/poll` linker blocking on readiness — the same
//! model sync `wasi:sockets` already uses.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use anyhow::Result;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use wasmtime_wasi::WasiCtxBuilder;
use wasmtime_wasi_tls::{
    Error as TlsError, TlsProvider, TlsStream, TlsTransport, WasiTlsCtx, WasiTlsCtxBuilder,
};

/// Signal's self-signed service CA (`O=Signal Messenger, LLC, CN=Signal
/// Messenger`), pinned at build time. Mirrors the PEM bundled by
/// presage/libsignal-service-rs; production should track Signal's own source.
const SIGNAL_CA_PEM: &[u8] = include_bytes!("../certs/signal-messenger-ca.pem");

/// Grant a guest outbound network + DNS.
///
/// **Capability decision (single source of truth):** this opens outbound
/// TCP/TLS to all addresses for every guest. It is latent — only guests that
/// import `wasi:sockets` / `wasi:tls` can use it, and the current skiko-UI
/// guests don't. Tightening to a per-app allowlist from `package.toml` (via
/// `WasiCtxBuilder::socket_addr_check`) is the intended follow-up.
pub fn grant_network(builder: &mut WasiCtxBuilder) {
    builder.inherit_network();
    builder.allow_ip_name_lookup(true);
}

/// Build the per-store [`WasiTlsCtx`] with the Signal-aware trust store.
pub fn wasi_tls_ctx() -> WasiTlsCtx {
    WasiTlsCtxBuilder::new()
        .provider(Box::new(
            SignalTlsProvider::new().expect("build Signal TLS provider from embedded CA"),
        ))
        .build()
}

/// Register the `wasi:tls` host imports into a guest linker.
pub fn add_to_linker(linker: &mut wasmtime::component::Linker<crate::HostState>) -> Result<()> {
    let mut opts = wasmtime_wasi_tls::p2::LinkOptions::default();
    opts.tls(true);
    wasmtime_wasi_tls::p2::add_to_linker(linker, &opts)?;
    // Task 115 — the 0.3 (p3, native-async) twin, additive next to p2
    // (dual-serve). Shares the same WasiTlsCtx / SignalTlsProvider trust store:
    // both variants resolve the provider through WasiTlsView.
    #[cfg(feature = "p3-async")]
    wasmtime_wasi_tls::p3::add_to_linker(linker)?;
    Ok(())
}

// ---- custom provider: webpki public roots + Signal's pinned CA -------------

/// Newtype so we can impl wasi-tls's `TlsStream` on a foreign rustls stream
/// (orphan rule). Delegates I/O to the inner stream, which is `Unpin`.
struct SignalTlsStream(tokio_rustls::client::TlsStream<Box<dyn TlsTransport>>);

impl AsyncRead for SignalTlsStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
    }
}
impl AsyncWrite for SignalTlsStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }
}
impl TlsStream for SignalTlsStream {}

struct SignalTlsProvider {
    config: Arc<rustls::ClientConfig>,
}

impl SignalTlsProvider {
    fn new() -> Result<Self> {
        // ring crypto provider — install once per process (no-op if already set).
        let _ = rustls::crypto::ring::default_provider().install_default();

        let mut roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let mut added = 0usize;
        let mut rd = std::io::BufReader::new(SIGNAL_CA_PEM);
        for cert in rustls_pemfile::certs(&mut rd) {
            roots.add(cert?)?;
            added += 1;
        }
        anyhow::ensure!(added > 0, "no Signal CA cert parsed from embedded PEM");
        log::info!(
            "signal_tls: trust store = {} public roots + {} Signal CA",
            webpki_roots::TLS_SERVER_ROOTS.len(),
            added
        );
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(Self {
            config: Arc::new(config),
        })
    }
}

impl TlsProvider for SignalTlsProvider {
    // NB: wasi-tls's `BoxFutureTlsStream` alias is pub(crate); write it expanded.
    fn connect(
        &self,
        server_name: String,
        transport: Box<dyn TlsTransport>,
    ) -> Pin<Box<dyn Future<Output = Result<Box<dyn TlsStream>, TlsError>> + Send>> {
        let config = Arc::clone(&self.config);
        Box::pin(async move {
            let domain = rustls::pki_types::ServerName::try_from(server_name)
                .map_err(|_| TlsError::msg("invalid server name"))?;
            let stream = tokio_rustls::TlsConnector::from(config)
                .connect(domain, transport)
                .await
                .map_err(|e| TlsError::msg(e.to_string()))?;
            Ok(Box::new(SignalTlsStream(stream)) as Box<dyn TlsStream>)
        })
    }
}
