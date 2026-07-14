//! Exit codes.
//!
//! Three failures look identical to a shell unless we make them distinct, and a
//! harness driving this port needs to tell them apart:
//!
//! * **64** — the command exists but is not ported yet. A script can see the
//!   port's progress without parsing prose.
//! * **2** — the command is real and ported, but *this workspace's backend*
//!   cannot serve it (`bd branch` on SQLite). Not a bug, not a gap: a contract.
//! * **1** — beads tried and genuinely failed.
//!
//! Conflating 64 with 1 is the mistake this module exists to prevent.

pub const OK: i32 = 0;
pub const FAILURE: i32 = 1;
/// The backend cannot do this — see [`bd_storage::Error::Unsupported`].
pub const CAPABILITY: i32 = 2;
/// Registered in the command tree, not yet implemented. Deliberately in the
/// sysexits.h range (`EX_USAGE`) purely because it is far away from 0/1/2.
pub const NOT_IMPLEMENTED: i32 = 64;
/// Bad invocation. Note this is *not* 2: clap's default would collide with
/// [`CAPABILITY`], and that collision is exactly what we are trying to avoid.
pub const USAGE: i32 = 1;

/// A failure whose message has already been printed, carrying the code to exit
/// with. Returned by the stub and capability paths, which format themselves
/// (they have to — the shape differs under `--json`).
#[derive(Debug)]
pub struct SilentExit(pub i32);

impl std::fmt::Display for SilentExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never rendered; main intercepts this before printing anything.
        write!(f, "exit {}", self.0)
    }
}

impl std::error::Error for SilentExit {}
