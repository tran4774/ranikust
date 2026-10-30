//! Small serde helpers shared by the image types.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Deserializer};

/// Reads `null` as the type's default; Go writes a nil slice as `null`.
pub(crate) fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// A value Go's `encoding/json` leaves out under `omitempty`: an empty
/// string, `false`, zero, an empty slice or map.
pub(crate) trait GoEmpty {
    fn go_empty(&self) -> bool;
}

impl GoEmpty for String {
    fn go_empty(&self) -> bool {
        self.is_empty()
    }
}

impl GoEmpty for bool {
    fn go_empty(&self) -> bool {
        !*self
    }
}

impl GoEmpty for i64 {
    fn go_empty(&self) -> bool {
        *self == 0
    }
}

impl<T> GoEmpty for Vec<T> {
    fn go_empty(&self) -> bool {
        self.is_empty()
    }
}

impl<T> GoEmpty for BTreeSet<T> {
    fn go_empty(&self) -> bool {
        self.is_empty()
    }
}

impl<K, V> GoEmpty for BTreeMap<K, V> {
    fn go_empty(&self) -> bool {
        self.is_empty()
    }
}

/// `skip_serializing_if` for an optional field Go writes `omitempty`: absent
/// or empty is left out, as BuildKit's typed round trip of a config drops it
/// (Q-132, `ADR-0032`).
pub(crate) fn omit_empty<T: GoEmpty>(value: &Option<T>) -> bool {
    value.as_ref().is_none_or(GoEmpty::go_empty)
}
