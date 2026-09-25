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
    install_pkcs8(path, &generate_pkcs8()?, write_private, |from, to| {
        std::fs::hard_link(from, to)
    })
}

/// [`create_pkcs8`] for a given key, with the private write and the link taken
/// as parameters so their failure paths can be driven by a test.
fn install_pkcs8(
    path: &Path,
    pkcs8: &[u8],
    write: impl Fn(&Path, &[u8]) -> anyhow::Result<()>,
    link: impl Fn(&Path, &Path) -> std::io::Result<()>,
) -> anyhow::Result<Vec<u8>> {
    // Same directory, so the link stays on one filesystem. The name carries both
    // the pid and a per-process counter: two racing *processes* must not share a
    // staging file, and neither must two racing threads inside one.
    let seq = NEXT_STAGING.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let staging = path.with_extension(format!("tmp.{}.{seq}", std::process::id()));
    // The staging file is removed however this goes — including a write that
    // failed partway, which would otherwise leave part of a private key behind.
    let linked = write(&staging, pkcs8).map(|()| link(&staging, path));
    let _ = std::fs::remove_file(&staging);

    match linked? {
        Ok(()) => Ok(pkcs8.to_vec()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => adopt_existing(path),
        // A filesystem without hard links (some network and FUSE mounts refuse
        // `link(2)` outright). Fall back to creating the key in place: still
        // exclusive, so two racers cannot both win, though a reader racing the
        // write can see it partial — the cost the link exists to avoid, paid
        // only where the link cannot be had.
        Err(link_err) => match write(path, pkcs8) {
            Ok(()) => Ok(pkcs8.to_vec()),
            Err(_) if path.exists() => adopt_existing(path),
            Err(e) => Err(anyhow::anyhow!(
                "failed to create identity key {} (hard link: {link_err}; direct: {e})",
                path.display()
            )),
        },
    }
}

/// Another process created the key first. Its key is the machine's identity;
/// ours was never visible to anyone.
fn adopt_existing(path: &Path) -> anyhow::Result<Vec<u8>> {
    std::fs::read(path)
        .map_err(|e| anyhow::anyhow!("failed to read identity key {}: {e}", path.display()))
}

/// Write `bytes` to a fresh mode-0600 file, refusing to overwrite one. A file
/// this call created and then failed to fill is removed again, so a failure
/// never leaves a partial key behind.
fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| anyhow::anyhow!("failed to create {}: {e}", path.display()))?;
    // Durable before it becomes visible under its real name.
    fill_or_remove(path, move || {
        file.write_all(bytes)?;
        file.sync_all()
    })
}

/// Run `fill` against a file this process just created at `path`; if it
/// fails, remove the file so no partial content survives. `fill` owns (and
/// drops) the open handle, so the file is closed before it is removed.
fn fill_or_remove(path: &Path, fill: impl FnOnce() -> std::io::Result<()>) -> anyhow::Result<()> {
    let filled = fill().map_err(|e| anyhow::anyhow!("failed to write {}: {e}", path.display()));
    if filled.is_err() {
        let _ = std::fs::remove_file(path);
    }
    filled
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

    /// The names in `dir`, sorted.
    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("read_dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// A staging write that fails partway (a full disk) must not leave part of
    /// a private key behind in the config directory.
    #[test]
    fn a_failed_staging_write_leaves_no_partial_key_behind() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("identity.pk8");
        let half_then_fail = |at: &Path, bytes: &[u8]| {
            std::fs::write(at, &bytes[..bytes.len() / 2]).expect("half");
            Err(anyhow::anyhow!("no space left on device"))
        };

        let err = install_pkcs8(&path, b"pkcs8-bytes", half_then_fail, |_, _| {
            panic!("nothing to link after a failed write")
        })
        .expect_err("the write failed");
        assert!(err.to_string().contains("no space left"), "{err}");
        assert!(entries(tmp.path()).is_empty(), "{:?}", entries(tmp.path()));
    }

    /// `write_private` refuses a file it did not create and must not remove it.
    #[test]
    fn write_private_leaves_an_existing_file_alone() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("key");
        std::fs::write(&path, b"someone else's").expect("write");
        // create_new refuses an existing file and must not remove it.
        write_private(&path, b"ours").expect_err("exists");
        assert_eq!(std::fs::read(&path).expect("read"), b"someone else's");
    }

    /// A file `write_private` created and then failed to fill (a full disk
    /// mid-write) is removed, so no partial key survives. The failing writer
    /// is injected after a real create, the same order `write_private` uses.
    #[test]
    fn write_private_removes_a_file_it_created_but_could_not_fill() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("key");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .expect("create");

        let err = fill_or_remove(&path, move || {
            file.write_all(b"half")?;
            Err(std::io::Error::other("no space left on device"))
        })
        .expect_err("the fill failed");

        assert!(err.to_string().contains("no space left"), "{err}");
        assert!(!path.exists(), "partial key left behind");
        assert!(entries(tmp.path()).is_empty(), "{:?}", entries(tmp.path()));
    }

    /// A fill that succeeds keeps the file and its bytes.
    #[test]
    fn fill_or_remove_keeps_a_file_it_filled() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("key");
        let mut file = std::fs::File::create(&path).expect("create");

        fill_or_remove(&path, move || file.write_all(b"whole")).expect("filled");

        assert_eq!(std::fs::read(&path).expect("read"), b"whole");
    }

    /// A filesystem without hard links refuses `link(2)` outright. That must
    /// not make the identity impossible to create: the key is created in
    /// place instead, still exclusively and still 0600, and no staging file
    /// remains.
    #[test]
    fn a_filesystem_without_hard_links_falls_back_to_creating_in_place() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("identity.pk8");

        let key = install_pkcs8(&path, b"pkcs8-bytes", write_private, |_, _| {
            Err(std::io::ErrorKind::Unsupported.into())
        })
        .expect("created in place");

        assert_eq!(key, b"pkcs8-bytes");
        assert_eq!(std::fs::read(&path).expect("read"), b"pkcs8-bytes");
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(entries(tmp.path()), ["identity.pk8"]);
    }

    /// On that fallback, a racer that created the key first still wins: the
    /// loser adopts the key on disk rather than failing or overwriting it.
    #[test]
    fn the_in_place_fallback_adopts_a_key_created_first() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("identity.pk8");
        let winner = path.clone();

        let key = install_pkcs8(&path, b"loser", write_private, move |_, _| {
            // The winner lands between our staging write and our fallback.
            std::fs::write(&winner, b"winner").expect("winner");
            Err(std::io::ErrorKind::PermissionDenied.into())
        })
        .expect("adopts");

        assert_eq!(key, b"winner");
        assert_eq!(std::fs::read(&path).expect("read"), b"winner");
    }
}
