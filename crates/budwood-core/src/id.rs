use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::Error;

// Domain-separation tags: a blob, a tree and a changeset with identical bytes
// never share an id.
const BLOB_TAG: &[u8] = b"budwood:blob:v1\0";
const TREE_TAG: &[u8] = b"budwood:tree:v1\0";
const CHANGESET_TAG: &[u8] = b"budwood:changeset:v1\0";

/// A 256-bit BLAKE3 content address.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId([u8; 32]);

impl ObjectId {
    pub const LEN: usize = 32;

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// First 12 hex characters, for display.
    pub fn short(&self) -> String {
        hex::encode(&self.0[..6])
    }

    pub fn from_hex(s: &str) -> Result<Self, Error> {
        let mut bytes = [0u8; 32];
        hex::decode_to_slice(s, &mut bytes).map_err(|_| Error::InvalidId(s.to_string()))?;
        Ok(Self(bytes))
    }

    pub fn for_blob(data: &[u8]) -> Self {
        Self::tagged(BLOB_TAG, data)
    }

    pub fn for_tree(encoded: &[u8]) -> Self {
        Self::tagged(TREE_TAG, encoded)
    }

    pub fn for_changeset(encoded: &[u8]) -> Self {
        Self::tagged(CHANGESET_TAG, encoded)
    }

    fn tagged(tag: &[u8], data: &[u8]) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(tag);
        hasher.update(&(data.len() as u64).to_le_bytes());
        hasher.update(data);
        Self(*hasher.finalize().as_bytes())
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ObjectId({})", self.short())
    }
}

impl FromStr for ObjectId {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Error> {
        Self::from_hex(s)
    }
}

impl Serialize for ObjectId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for ObjectId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        let id = ObjectId::for_blob(b"hello");
        assert_eq!(ObjectId::from_hex(&id.to_hex()).unwrap(), id);
        assert_eq!(id.short().len(), 12);
    }

    #[test]
    fn domains_are_separated() {
        assert_ne!(ObjectId::for_blob(b"x"), ObjectId::for_tree(b"x"));
        assert_ne!(ObjectId::for_tree(b"x"), ObjectId::for_changeset(b"x"));
    }

    #[test]
    fn rejects_bad_hex() {
        assert!(ObjectId::from_hex("abc").is_err());
        assert!(ObjectId::from_hex(&"z".repeat(64)).is_err());
    }
}
