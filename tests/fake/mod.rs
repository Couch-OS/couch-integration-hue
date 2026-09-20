//! Everything the tests address instead of a real Hue bridge.
//!
//! There is exactly one fake, it listens on 127.0.0.1 with a port the
//! operating system chooses, and it is the only thing any test in this
//! repository connects to. No test discovers anything, browses mDNS, or sends
//! a byte to any other address.
pub mod bridge;
pub mod slot;
