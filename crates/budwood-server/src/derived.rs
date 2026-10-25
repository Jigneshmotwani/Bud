//! A small, generic mechanism for derived data.
//!
//! The changeset log is the source of truth. Anything computed from it (blame
//! today; other indexes later) is a [`Derivation`]: a pure function from a key
//! to a value, computed from some inputs read out of history plus the values of
//! other keys it depends on. Values are memoized in the `derived` SQLite table
//! under `(repo, kind, key)`, so each one is computed at most once per server.
//!
//! Derivations are *incremental*: a value usually depends on the same kind of
//! value for an older revision (blame of a file at revision 9 depends on its
//! blame at the previous revision that touched it), so computing a new value
//! costs only the work for what changed since the newest memoized ancestor.
//!
//! [`derive`] evaluates lazily, on first request, and walks dependencies with
//! an explicit stack rather than recursion, so arbitrarily long histories can't
//! overflow the thread's stack. Eager derivation is the same call made ahead of
//! time (for example by a background job after each commit).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;

use anyhow::Context;
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::service::{Service, ServiceError, ServiceResult};

/// A kind of derived data.
pub trait Derivation {
    /// Names the kind in storage. Bump the version suffix whenever the value
    /// format or the algorithm changes, so old memoized values are ignored.
    const KIND: &'static str;

    type Key: Clone;
    /// Read from history before dependencies are derived, and handed to
    /// [`Derivation::compute`] afterwards.
    type Inputs;
    type Value: Serialize + DeserializeOwned;

    /// The storage key. Must be unique per key within a repository.
    fn encode_key(key: &Self::Key) -> String;

    /// Gathers the inputs for `key` and lists the keys whose values it needs.
    /// Dependencies must be well-founded (for example, refer only to older
    /// revisions); a cycle is reported as an internal error.
    fn inputs(svc: &Service, repo: &str, key: &Self::Key) -> ServiceResult<(Self::Inputs, Vec<Self::Key>)>;

    /// Computes the value of `key`. `deps` holds the values of the keys listed
    /// by [`Derivation::inputs`], in the same order.
    fn compute(
        svc: &Service,
        repo: &str,
        key: &Self::Key,
        inputs: Self::Inputs,
        deps: Vec<Self::Value>,
    ) -> ServiceResult<Self::Value>;
}

struct Frame<D: Derivation> {
    key: D::Key,
    encoded: String,
    /// `None` until the frame's inputs have been gathered.
    pending: Option<(D::Inputs, Vec<D::Key>)>,
}

/// Returns the value of `key`, deriving (and memoizing) it and any missing
/// dependencies first.
pub fn derive<D: Derivation>(svc: &Service, repo: &str, key: &D::Key) -> ServiceResult<D::Value> {
    let encoded = D::encode_key(key);
    if let Some(value) = load::<D>(svc, repo, &encoded)? {
        return Ok(value);
    }
    // Values computed in this call and not yet consumed by their dependents.
    // A dependency chain consumes each value once, so this stays small.
    let mut fresh: HashMap<String, D::Value> = HashMap::new();
    // Keys computed in this call, to skip duplicate frames in a DAG.
    let mut computed: HashSet<String> = HashSet::new();
    // Frames whose inputs are gathered but whose value isn't computed yet.
    // Because evaluation is depth-first, these are the ancestors of the top
    // frame, so meeting one again as a dependency means a cycle.
    let mut in_progress: HashSet<String> = HashSet::new();
    let mut stack: Vec<Frame<D>> = vec![Frame { key: key.clone(), encoded, pending: None }];

    while let Some(frame) = stack.last_mut() {
        if frame.pending.is_none() {
            if computed.contains(&frame.encoded) {
                stack.pop();
                continue;
            }
            in_progress.insert(frame.encoded.clone());
            let (inputs, deps) = D::inputs(svc, repo, &frame.key)?;
            let mut missing: Vec<Frame<D>> = Vec::new();
            for dep in &deps {
                let enc = D::encode_key(dep);
                if in_progress.contains(&enc) {
                    return Err(ServiceError::Internal(anyhow::anyhow!("{} dependency cycle at {enc}", D::KIND)));
                }
                if computed.contains(&enc)
                    || missing.iter().any(|f| f.encoded == enc)
                    || is_memoized::<D>(svc, repo, &enc)?
                {
                    continue;
                }
                missing.push(Frame { key: dep.clone(), encoded: enc, pending: None });
            }
            frame.pending = Some((inputs, deps));
            stack.extend(missing);
            continue;
        }

        let Frame { key, encoded, pending } = stack.pop().expect("stack is non-empty");
        let (inputs, deps) = pending.expect("inputs gathered");
        let mut values = Vec::with_capacity(deps.len());
        for dep in &deps {
            let enc = D::encode_key(dep);
            let value = match fresh.remove(&enc) {
                Some(v) => v,
                None => load::<D>(svc, repo, &enc)?
                    .ok_or_else(|| ServiceError::Internal(anyhow::anyhow!("{} dependency {enc} vanished", D::KIND)))?,
            };
            values.push(value);
        }
        let value = D::compute(svc, repo, &key, inputs, values)?;
        let json = serde_json::to_string(&value).context("encoding derived value")?;
        svc.meta.derived_put(repo, D::KIND, &encoded, &json)?;
        svc.derivations.fetch_add(1, Ordering::Relaxed);
        in_progress.remove(&encoded);
        if stack.is_empty() {
            return Ok(value);
        }
        computed.insert(encoded.clone());
        fresh.insert(encoded, value);
    }
    unreachable!("the requested key is the last frame popped")
}

fn load<D: Derivation>(svc: &Service, repo: &str, key: &str) -> ServiceResult<Option<D::Value>> {
    let Some(json) = svc.meta.derived_get(repo, D::KIND, key)? else { return Ok(None) };
    let value = serde_json::from_str(&json).with_context(|| format!("decoding {} value {key}", D::KIND))?;
    Ok(Some(value))
}

fn is_memoized<D: Derivation>(svc: &Service, repo: &str, key: &str) -> ServiceResult<bool> {
    Ok(svc.meta.derived_get(repo, D::KIND, key)?.is_some())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::meta::MetaStore;
    use crate::store::MemObjectStore;

    /// Triangular numbers, as a chain: t(n) = t(n-1) + n.
    struct Triangle;

    impl Derivation for Triangle {
        const KIND: &'static str = "test.triangle.v1";
        type Key = u64;
        type Inputs = u64;
        type Value = u64;

        fn encode_key(key: &u64) -> String {
            key.to_string()
        }

        fn inputs(_: &Service, _: &str, key: &u64) -> ServiceResult<(u64, Vec<u64>)> {
            Ok((*key, if *key == 0 { vec![] } else { vec![key - 1] }))
        }

        fn compute(_: &Service, _: &str, _: &u64, n: u64, deps: Vec<u64>) -> ServiceResult<u64> {
            Ok(deps.first().copied().unwrap_or(0) + n)
        }
    }

    /// Fibonacci: a DAG where values are shared by two dependents.
    struct Fib;

    impl Derivation for Fib {
        const KIND: &'static str = "test.fib.v1";
        type Key = u64;
        type Inputs = ();
        type Value = u64;

        fn encode_key(key: &u64) -> String {
            key.to_string()
        }

        fn inputs(_: &Service, _: &str, key: &u64) -> ServiceResult<((), Vec<u64>)> {
            Ok(((), if *key < 2 { vec![] } else { vec![key - 1, key - 2] }))
        }

        fn compute(_: &Service, _: &str, key: &u64, _: (), deps: Vec<u64>) -> ServiceResult<u64> {
            Ok(if *key < 2 { *key } else { deps[0] + deps[1] })
        }
    }

    /// Depends on itself.
    struct Cycle;

    impl Derivation for Cycle {
        const KIND: &'static str = "test.cycle.v1";
        type Key = u64;
        type Inputs = ();
        type Value = u64;

        fn encode_key(key: &u64) -> String {
            key.to_string()
        }

        fn inputs(_: &Service, _: &str, key: &u64) -> ServiceResult<((), Vec<u64>)> {
            Ok(((), vec![(key + 1) % 3]))
        }

        fn compute(_: &Service, _: &str, _: &u64, _: (), _: Vec<u64>) -> ServiceResult<u64> {
            Ok(0)
        }
    }

    fn service() -> Service {
        Service::new(Arc::new(MemObjectStore::default()), MetaStore::open_in_memory().unwrap()).unwrap()
    }

    #[test]
    fn long_chains_are_iterative_and_memoized() {
        let svc = service();
        // Deep enough to overflow the stack if evaluation were recursive.
        assert_eq!(derive::<Triangle>(&svc, "r", &200_000).unwrap(), 200_000 * 200_001 / 2);
        assert_eq!(svc.derivations(), 200_001);
        assert_eq!(svc.meta.derived_count("r", Triangle::KIND).unwrap(), 200_001);

        // Memo hits compute nothing; extending the chain computes only the new part.
        assert_eq!(derive::<Triangle>(&svc, "r", &10).unwrap(), 55);
        assert_eq!(derive::<Triangle>(&svc, "r", &200_002).unwrap(), 200_002 * 200_003 / 2);
        assert_eq!(svc.derivations(), 200_003);

        // Memo entries are per repository.
        assert_eq!(derive::<Triangle>(&svc, "other", &3).unwrap(), 6);
        assert_eq!(svc.derivations(), 200_007);
    }

    #[test]
    fn shared_dependencies_are_computed_once() {
        let svc = service();
        assert_eq!(derive::<Fib>(&svc, "r", &90).unwrap(), 2_880_067_194_370_816_120);
        assert_eq!(svc.derivations(), 91);
    }

    #[test]
    fn cycles_are_errors() {
        let svc = service();
        assert!(matches!(derive::<Cycle>(&svc, "r", &0), Err(ServiceError::Internal(_))));
    }
}
