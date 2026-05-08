//! Cascada-managed external sidecars. Currently houses the TradingView
//! proxy supervisor (Python + mitmproxy + the bundled `cascada_addon.py`).
//! Add new sidecar managers here when other browser-based platforms need
//! the same lifecycle treatment (spawn / restart / capture stdout / kill
//! on app exit).

pub mod tv_proxy;

pub use tv_proxy::{TvProxyManager, TvProxyStatus};
