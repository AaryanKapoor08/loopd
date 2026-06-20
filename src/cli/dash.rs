//! `loop dash` — the live TUI cockpit. A thin client: it just launches the
//! `dashboard` module, which polls the daemon over HTTP and renders.

use anyhow::Result;

pub fn dash() -> Result<()> {
    crate::dashboard::run()
}
