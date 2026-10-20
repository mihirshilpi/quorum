//! Lenient deserialisation helpers for hostile Graph JSON.

use serde::{Deserialize, Deserializer};

/// Deserialises `null` as `T::default()` (Graph sends `null` for empty arrays and flags).
pub(crate) fn null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// Truncates `s` to at most `max` bytes on a char boundary.
pub(crate) fn truncate_on_char_boundary(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}
