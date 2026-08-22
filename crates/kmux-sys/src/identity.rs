//! Cryptographic machine/user identity (issue #146).
//!
//! Every kmux participant — each client process and the daemon — owns one
//! Ed25519 keypair, persisted as PKCS#8 in the config dir (mode 0600) and shared
//! by all of that user's kmux processes on the machine. The *identity* presented
//! on the wire is the hex-encoded SHA-256 fingerprint of the public key, which
//! cryptographically guarantees uniqueness.
//!
//! Presenting a public key alone proves nothing — anyone could paste another
//! party's key. So the daemon issues a random [`random_nonce`] challenge, the
//! client signs it with [`Identity::sign`], and the daemon [`verify`]s the
//! signature against the presented public key before trusting the identity. This
//! proof-of-possession makes the identity unforgeable: no one can claim an
//! identity whose private key they do not hold.
//!
//! The wire fields that carry public keys, signatures, and nonces are plain
//! bytes in [`kmux_protocol::messages`] and are always compiled; only this keypair logic
//! is gated behind the `identity` feature (it pulls `ring` + `sha2`).

use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use sha2::{Digest, Sha256};

/// An Ed25519 identity keypair: the private key for signing challenges plus a
/// cached copy of the raw public key bytes.
pub struct Identity {
    key_pair: Ed25519KeyPair,
    public_key: Vec<u8>,
}

impl Identity {
    /// Load the persisted identity, generating and persisting a fresh keypair on
    /// first use. The key file is created mode 0600 in the config dir
    /// ([`crate::dirs::identity_key_path`]).
    pub fn load_or_create() -> anyhow::Result<Self> {
        let path = crate::dirs::identity_key_path()?;
        Self::load_or_create_at(&path)
    }

    /// [`load_or_create`](Self::load_or_create) against an explicit path, so the
    /// creation race below can be tested without touching the real config dir.
    ///
    /// # Errors
    ///
    /// If `path` cannot be read or created, or holds something that is not a
    /// valid PKCS#8 Ed25519 key. A key file that exists but does not parse is an
    /// error rather than a silent regeneration: replacing it would change this
    /// machine's identity without anyone asking.
    pub fn load_or_create_at(path: &Path) -> anyhow::Result<Self> {
        let pkcs8 = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => create_pkcs8(path)?,
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "failed to read identity key {}: {e}",
                    path.display()
                ));
            }
        };
        Self::from_pkcs8(&pkcs8)
            .map_err(|e| anyhow::anyhow!("invalid identity key {}: {e}", path.display()))
    }

    /// Generate a fresh, non-persisted identity. Useful for tests and for
    /// ephemeral signing contexts that should not touch the on-disk keypair.
    pub fn generate() -> Self {
        Self::from_pkcs8(&generate_pkcs8().expect("generate keypair"))
            .expect("generated pkcs8 is valid")
    }

    /// Build an identity from persisted PKCS#8 bytes.
    pub fn from_pkcs8(pkcs8: &[u8]) -> Result<Self, String> {
        let key_pair = Ed25519KeyPair::from_pkcs8(pkcs8).map_err(|e| e.to_string())?;
        let public_key = key_pair.public_key().as_ref().to_vec();
        Ok(Self {
            key_pair,
            public_key,
        })
    }

    /// The raw Ed25519 public key bytes (32 bytes) sent in the `Auth` handshake.
    pub fn public_key_bytes(&self) -> &[u8] {
        &self.public_key
    }

    /// This identity's fingerprint: hex-encoded SHA-256 of the public key.
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.public_key)
    }

    /// Sign a server-issued challenge `nonce`. The daemon [`verify`]s the result
    /// against [`Identity::public_key_bytes`] to prove possession of the key.
    pub fn sign(&self, nonce: &[u8]) -> Vec<u8> {
        self.key_pair.sign(nonce).as_ref().to_vec()
    }
}

/// The identity fingerprint for an arbitrary public key: hex-encoded SHA-256.
pub fn fingerprint(public_key: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(public_key);
    hex_encode(&hasher.finalize())
}

/// An abbreviated fingerprint for display (first 12 hex chars).
pub fn short(fingerprint: &str) -> &str {
    &fingerprint[..fingerprint.len().min(12)]
}

/// Verify that `signature` over `nonce` was produced by the private key matching
/// `public_key`. Returns `false` on any malformed input or mismatch.
pub fn verify(public_key: &[u8], nonce: &[u8], signature: &[u8]) -> bool {
    UnparsedPublicKey::new(&ED25519, public_key)
        .verify(nonce, signature)
        .is_ok()
}

/// Best-effort local hostname, a friendly label for the identity claim. Falls
/// back to `"unknown"`.
pub fn local_hostname() -> String {
    nix::unistd::gethostname()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Best-effort local OS username, a friendly label for the identity claim. Falls
/// back to `$USER`, then `"unknown"`.
pub fn local_username() -> String {
    nix::unistd::User::from_uid(nix::unistd::getuid())
        .ok()
        .flatten()
        .map(|u| u.name)
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Generate a fresh random 32-byte challenge nonce (server side).
pub fn random_nonce() -> [u8; 32] {
    use ring::rand::SecureRandom as _;
    let rng = ring::rand::SystemRandom::new();
    let mut nonce = [0u8; 32];
    rng.fill(&mut nonce).expect("system RNG must not fail");
    nonce
}

fn generate_pkcs8() -> anyhow::Result<Vec<u8>> {
    let rng = ring::rand::SystemRandom::new();
    let doc = Ed25519KeyPair::generate_pkcs8(&rng)
        .map_err(|_| anyhow::anyhow!("failed to generate identity keypair"))?;
    Ok(doc.as_ref().to_vec())
}

/// Create the identity key file, or adopt the one a concurrent process created.
///
/// Returns the PKCS#8 bytes that are actually on disk afterwards, which may not
/// be the ones generated here.
///
/// Every kmux process on a machine shares one key file, and they start
/// concurrently — a GUI and a CLI, or several panes' worth of `kmux notify`.
/// The previous version opened the target `truncate(true)` and wrote in place,
/// which gives a reader two ways to lose: it can read a zero-length file
/// between the truncate and the write, or a prefix of the key mid-write. Either
/// is an "invalid identity key" for a process that did nothing wrong, and the
/// daemon then refuses its handshake.
///
/// So: write the whole key to a private temporary file in the same directory,
/// then `hard_link` it into place. `link(2)` fails with `EEXIST` rather than
/// replacing, which makes it an atomic create-if-absent — the file is never
/// visible at `path` in a partial state, and a loser adopts the winner's key
/// instead of holding one that disagrees with disk.
/// Distinguishes staging files written by two threads of one process.
static NEXT_STAGING: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn create_pkcs8(path: &Path) -> anyhow::Result<Vec<u8>> {
    let pkcs8 = generate_pkcs8()?;
    // Same directory, so the link stays on one filesystem. The name carries both
    // the pid and a per-process counter: two racing *processes* must not share a
    // staging file, and neither must two racing threads inside one.
    let seq = NEXT_STAGING.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let staging = path.with_extension(format!("tmp.{}.{seq}", std::process::id()));
    write_private(&staging, &pkcs8)?;

    let linked = std::fs::hard_link(&staging, path);
    // The staging file has served its purpose either way.
    let _ = std::fs::remove_file(&staging);

    match linked {
        Ok(()) => Ok(pkcs8),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Another process got there first. Its key is the machine's
            // identity; ours was never visible to anyone.
            std::fs::read(path)
                .map_err(|e| anyhow::anyhow!("failed to read identity key {}: {e}", path.display()))
        }
        Err(e) => Err(anyhow::anyhow!(
            "failed to create identity key {}: {e}",
            path.display()
        )),
    }
}

/// Write `bytes` to a fresh mode-0600 file, refusing to overwrite one.
fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| anyhow::anyhow!("failed to create {}: {e}", path.display()))?;
    file.write_all(bytes)
        .map_err(|e| anyhow::anyhow!("failed to write {}: {e}", path.display()))?;
    // Durable before it becomes visible under its real name.
    file.sync_all()
        .map_err(|e| anyhow::anyhow!("failed to flush {}: {e}", path.display()))?;
    Ok(())
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_identity() -> Identity {
        Identity::generate()
    }

    #[test]
    fn sign_verify_round_trip() {
        let id = new_identity();
        let nonce = random_nonce();
        let sig = id.sign(&nonce);
        assert!(verify(id.public_key_bytes(), &nonce, &sig));
    }

    #[test]
    fn signature_from_other_key_is_rejected() {
        let a = new_identity();
        let b = new_identity();
        let nonce = random_nonce();
        let sig = a.sign(&nonce);
        // b's public key must not validate a's signature.
        assert!(!verify(b.public_key_bytes(), &nonce, &sig));
    }

    #[test]
    fn tampered_nonce_is_rejected() {
        let id = new_identity();
        let nonce = random_nonce();
        let sig = id.sign(&nonce);
        let mut other = nonce;
        other[0] ^= 0xff;
        assert!(!verify(id.public_key_bytes(), &other, &sig));
    }

    #[test]
    fn fingerprint_is_stable_and_64_hex_chars() {
        let id = new_identity();
        let fp = id.fingerprint();
        assert_eq!(fp.len(), 64, "sha-256 hex is 64 chars");
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
        // Stable for the same public key.
        assert_eq!(fp, fingerprint(id.public_key_bytes()));
        // Distinct keys yield distinct fingerprints.
        assert_ne!(fp, new_identity().fingerprint());
    }

    #[test]
    fn short_is_a_prefix() {
        let fp = new_identity().fingerprint();
        assert_eq!(short(&fp), &fp[..12]);
    }

    #[test]
    fn two_random_nonces_differ() {
        assert_ne!(random_nonce(), random_nonce());
    }

    #[test]
    fn load_or_create_persists_and_is_stable() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // SAFETY: single-threaded test; no concurrent env access.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", tmp.path()) };

        let first = Identity::load_or_create().expect("create");
        let second = Identity::load_or_create().expect("load");
        // Same persisted key → same identity on reload.
        assert_eq!(first.fingerprint(), second.fingerprint());

        // Key file exists, mode 0600.
        use std::os::unix::fs::PermissionsExt as _;
        let path = crate::dirs::identity_key_path().expect("path");
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "identity key must be mode 0600");

        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
    }

    /// Concurrent first use is the normal case: a GUI and a CLI start together,
    /// or several panes run `kmux notify` at once. Every racer must end up with
    /// the *same* identity, and none may see a half-written key.
    #[test]
    fn racing_first_use_converges_on_one_identity() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("identity.pk8");

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let prints: Vec<String> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let barrier = std::sync::Arc::clone(&barrier);
                    let path = path.clone();
                    scope.spawn(move || {
                        barrier.wait();
                        Identity::load_or_create_at(&path)
                            .expect("no racer may see a partial key")
                            .fingerprint()
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("thread"))
                .collect()
        });

        assert_eq!(prints.len(), 8);
        assert!(
            prints.windows(2).all(|w| w[0] == w[1]),
            "all racers must adopt one identity, got {prints:?}"
        );
        // And it is the one on disk, so the next process agrees too.
        let reloaded = Identity::load_or_create_at(&path).expect("reload");
        assert_eq!(reloaded.fingerprint(), prints[0]);
    }

    /// The key file must never be observable in a partial state, so no staging
    /// file may be left behind under a name a reader might mistake for it.
    #[test]
    fn creating_the_key_leaves_no_staging_file_behind() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("identity.pk8");
        Identity::load_or_create_at(&path).expect("create");

        let left: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("read_dir")
            .filter_map(|e| e.ok().map(|e| e.file_name()))
            .collect();
        assert_eq!(
            left,
            vec![std::ffi::OsString::from("identity.pk8")],
            "{left:?}"
        );
    }

    /// The key is a private key. Mode 0600 is part of the contract, and the
    /// staging-then-link route must not lose it.
    #[test]
    fn the_created_key_is_readable_only_by_its_owner() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("identity.pk8");
        Identity::load_or_create_at(&path).expect("create");
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    /// A key file that is present but not a valid PKCS#8 document is an error,
    /// not something to silently regenerate over — regenerating would change the
    /// machine's identity behind the user's back.
    #[test]
    fn a_corrupt_key_file_is_an_error_rather_than_a_silent_reset() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("identity.pk8");
        std::fs::write(&path, b"not a key").expect("write");
        let err = Identity::load_or_create_at(&path)
            .map(|_| ())
            .expect_err("corrupt key");
        assert!(err.to_string().contains("invalid identity key"), "{err}");
    }
}
