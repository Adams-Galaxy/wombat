//! Canonical SHA-256 formatting.

use sha2::{Digest as _, Sha256};

pub(crate) fn sha256(bytes: impl AsRef<[u8]>) -> String {
    prefixed_hex(Sha256::digest(bytes))
}

pub(crate) fn hex_sha256(bytes: impl AsRef<[u8]>) -> String {
    hex(Sha256::digest(bytes))
}

pub(crate) fn prefixed_hex(bytes: impl AsRef<[u8]>) -> String {
    format!("sha256:{}", hex(bytes))
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    let bytes = bytes.as_ref();
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to a string cannot fail");
    }
    output
}
