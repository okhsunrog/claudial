//! Host daemon: reads Claude Code usage and pushes it to the device.
//!
//! Not implemented yet. The shape it has to take is already fixed by how the
//! credential works: Claude Code rotates the OAuth access token, so the token
//! is re-read from the credential store on every poll rather than cached at
//! startup. That is the whole reason this runs on the host instead of on the
//! device, which has no way to refresh a token it was handed once.

use clawdmeter_icd::UsageSnapshot;

fn main() -> anyhow::Result<()> {
    let _ = UsageSnapshot::UNKNOWN;
    anyhow::bail!("clawdmeter-host is not implemented yet")
}
