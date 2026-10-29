//! Small serde helpers shared by the image types.

use serde::{Deserialize, Deserializer};

/// Reads `null` as the type's default; Go writes a nil slice as `null`.
pub(crate) fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}
