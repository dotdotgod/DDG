//! RAM-only session credentials; exact old credentials are distinguishable from forgeries.

use ddg_agents_proto::id::{fill_csprng, parse_session_id};
use ddg_agents_proto::rpc::{ErrorKind, RpcError};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::io;
use std::sync::atomic::{Ordering, compiler_fence};
use zeroize::Zeroizing;

pub const MAX_SNAPSHOT: usize = 64 * 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::*;
    const A: &str = "ses_00000000000000000000000001";
    const B: &str = "ses_00000000000000000000000002";

    #[test]
    fn authenticates_only_exact_session_and_generation() {
        let mut store = TokenStore::default();
        let a = store.issue(A, 1).unwrap();
        let b = store.issue(B, 1).unwrap();
        let invalid = |token: &str, store: &TokenStore| {
            assert_eq!(
                store.verify(token).unwrap_err().data.code.as_deref(),
                Some("token_invalid")
            );
        };
        assert_ne!(a, b);
        assert_eq!(
            store.verify(&a).unwrap(),
            AuthenticatedCaller {
                session_id: A.into(),
                run_generation: 1
            }
        );
        invalid(&a.replace(A, B), &store);
        invalid(&a.replace(".1.", ".01."), &store);
        invalid(&a.replace(".1.", ".2."), &store);
        invalid(&a.to_uppercase(), &store);
        let new = store.issue(A, 2).unwrap();
        assert_eq!(
            store.verify(&a).unwrap_err().data.code.as_deref(),
            Some("token_revoked")
        );
        invalid(&b.replace(B, A), &store);
        assert_eq!(store.verify(&new).unwrap().run_generation, 2);
        store.revoke(A, 2);
        assert_eq!(
            store.verify(&new).unwrap_err().data.code.as_deref(),
            Some("token_revoked")
        );
        assert_eq!(store.verify(&b).unwrap().session_id, B);
    }

    #[test]
    fn snapshot_keeps_only_live_generations_active() {
        let mut before = TokenStore::default();
        let revoked = before.issue(A, 1).unwrap();
        let live = before.issue(A, 2).unwrap();
        let dead = before.issue(B, 1).unwrap();
        let wire = before.snapshot().unwrap();
        let mut after = TokenStore::restore(&wire).unwrap();
        after.reconcile_with_live_ptys(&[ddg_agents_proto::terminal::PtyInfo {
            session_id: A.into(),
            run_generation: 2,
            ..serde_json::from_value(serde_json::json!({
                "pty_id": 1, "pid": 2, "cols": 80, "rows": 24
            }))
            .unwrap()
        }]);
        assert_eq!(after.verify(&live).unwrap().run_generation, 2);
        for token in [revoked, dead] {
            assert_eq!(
                after.verify(&token).unwrap_err().data.code.as_deref(),
                Some("token_revoked")
            );
        }
        assert!(TokenStore::restore(b"{}").is_err());
    }

    #[test]
    fn drop_zeroes_secret_in_place() {
        let mut secret = std::mem::ManuallyDrop::new(Secret([0xab; 32]));
        let ptr: *mut Secret = &mut *secret;
        unsafe {
            std::ptr::drop_in_place(ptr);
            // ManuallyDrop keeps the storage alive, and each u8 remains initialized.
            assert_eq!(std::ptr::read_volatile(ptr.cast::<[u8; 32]>()), [0; 32]);
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct AuthenticatedCaller {
    pub session_id: String,
    pub run_generation: u64,
}

#[derive(Serialize, Deserialize)]
struct Secret([u8; 32]);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenSnapshot<S> {
    active: Vec<(String, u64, S)>,
    revoked: Vec<(String, u64, S)>,
}

impl Drop for Secret {
    fn drop(&mut self) {
        for byte in &mut self.0 {
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
        compiler_fence(Ordering::SeqCst);
    }
}

fn invalid_snapshot() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid token snapshot")
}

fn ct_eq(a: &Secret, b: &Secret) -> bool {
    a.0.iter().zip(&b.0).fold(0, |diff, (x, y)| diff | (x ^ y)) == 0
}

struct Credential {
    generation: u64,
    secret: Box<Secret>,
    revoked: bool,
}

#[derive(Default)]
pub struct TokenStore(HashMap<String, Vec<Credential>>);

impl TokenStore {
    pub fn snapshot(&self) -> io::Result<Zeroizing<Vec<u8>>> {
        let mut snapshot = TokenSnapshot {
            active: Vec::new(),
            revoked: Vec::new(),
        };
        for (id, credentials) in &self.0 {
            for entry in credentials {
                let bucket = if entry.revoked {
                    &mut snapshot.revoked
                } else {
                    &mut snapshot.active
                };
                bucket.push((id.clone(), entry.generation, entry.secret.as_ref()));
            }
        }
        // Count first so growing the buffer cannot leave a freed copy of the secrets.
        struct Counter(usize);
        impl io::Write for Counter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0 = self
                    .0
                    .checked_add(bytes.len())
                    .filter(|n| *n <= MAX_SNAPSHOT)
                    .ok_or_else(|| io::Error::other("token snapshot exceeds pipe limit"))?;
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut size = Counter(0);
        serde_json::to_writer(&mut size, &snapshot).map_err(|_| invalid_snapshot())?;
        let mut bytes = Zeroizing::new(Vec::with_capacity(size.0));
        serde_json::to_writer(&mut *bytes, &snapshot).map_err(|_| invalid_snapshot())?;
        Ok(bytes)
    }

    pub fn restore(bytes: &[u8]) -> io::Result<Self> {
        let snapshot: TokenSnapshot<Box<Secret>> =
            serde_json::from_slice(bytes).map_err(|_| invalid_snapshot())?;
        let mut store = Self::default();
        for (entries, revoked) in [(snapshot.active, false), (snapshot.revoked, true)] {
            for (session, generation, secret) in entries {
                parse_session_id(&session).map_err(|_| invalid_snapshot())?;
                let credentials = store.0.entry(session).or_default();
                if generation == 0
                    || (!revoked && !credentials.is_empty())
                    || credentials
                        .iter()
                        .any(|c| c.generation == generation && ct_eq(&c.secret, &secret))
                {
                    return Err(invalid_snapshot());
                }
                credentials.push(Credential {
                    generation,
                    secret,
                    revoked,
                });
            }
        }
        Ok(store)
    }

    pub fn reconcile_with_live_ptys(&mut self, ptys: &[ddg_agents_proto::terminal::PtyInfo]) {
        let live: HashSet<_> = ptys
            .iter()
            .map(|p| (p.session_id.as_str(), p.run_generation))
            .collect();
        for (id, credentials) in &mut self.0 {
            for entry in credentials {
                entry.revoked |= !live.contains(&(id.as_str(), entry.generation));
            }
        }
    }

    pub fn issue(&mut self, session_id: &str, generation: u64) -> Result<String, RpcError> {
        let mut secret = Box::new(Secret([0; 32]));
        fill_csprng(&mut secret.0)
            .map_err(|_| RpcError::new(ErrorKind::Internal, "OS randomness unavailable", None))?;
        let mut wire = format!("{session_id}.{generation}.");
        for byte in &secret.0 {
            write!(wire, "{byte:02x}").expect("write to string");
        }
        let credentials = self.0.entry(session_id.to_owned()).or_default();
        for credential in &mut *credentials {
            credential.revoked = true;
        }
        credentials.push(Credential {
            generation,
            secret,
            revoked: false,
        });
        Ok(wire)
    }

    pub fn revoke(&mut self, session_id: &str, generation: u64) {
        if let Some(credentials) = self.0.get_mut(session_id) {
            for credential in credentials
                .iter_mut()
                .filter(|entry| entry.generation == generation)
            {
                credential.revoked = true;
            }
        }
    }

    pub fn verify(&self, token: &str) -> Result<AuthenticatedCaller, RpcError> {
        let invalid = || RpcError::token_invalid("invalid session token");
        let (session_id, rest) = token.split_once('.').ok_or_else(invalid)?;
        parse_session_id(session_id).map_err(|_| invalid())?;
        let (generation, hex) = rest.split_once('.').ok_or_else(invalid)?;
        if generation.starts_with('0')
            || !generation.bytes().all(|b| b.is_ascii_digit())
            || hex.len() != 64
        {
            return Err(invalid());
        }
        let run_generation = generation.parse().map_err(|_| invalid())?;
        let nibble = |b| match b {
            b'0'..=b'9' => Ok(b - b'0'),
            b'a'..=b'f' => Ok(b - b'a' + 10),
            _ => Err(invalid()),
        };
        let mut secret = Secret([0; 32]);
        for (dst, pair) in secret.0.iter_mut().zip(hex.as_bytes().as_chunks::<2>().0) {
            *dst = nibble(pair[0])? << 4 | nibble(pair[1])?;
        }
        let mut active = false;
        let mut revoked = false;
        if let Some(credentials) = self.0.get(session_id) {
            for entry in credentials
                .iter()
                .filter(|entry| entry.generation == run_generation)
            {
                let matches = ct_eq(&entry.secret, &secret);
                active |= matches & !entry.revoked;
                revoked |= matches & entry.revoked;
            }
        }
        if active {
            Ok(AuthenticatedCaller {
                session_id: session_id.to_owned(),
                run_generation,
            })
        } else if revoked {
            Err(RpcError::token_revoked("session token revoked"))
        } else {
            Err(invalid())
        }
    }
}
