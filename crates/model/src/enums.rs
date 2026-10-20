//! Graph string enums that never fail to parse.

/// A Graph string enum: known values map to variants, anything else to `Unknown(String)`.
///
/// Implemented by every enum in this crate that mirrors a Graph enumeration.
pub trait GraphEnum: Sized {
    /// Parses a Graph value; unknown values become `Unknown`. Matching ignores ASCII case.
    fn from_graph(value: &str) -> Self;

    /// Returns the Graph wire value.
    fn as_graph(&self) -> &str;
}

/// The longest `Unknown` value kept; longer hostile values are truncated.
pub(crate) const MAX_UNKNOWN_LEN: usize = 256;

macro_rules! graph_enum {
    (
        $(#[$meta:meta])*
        $name:ident {
            $( $(#[$vmeta:meta])* $variant:ident = $wire:literal ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        ///
        /// Unknown values from Graph are kept in `Unknown` instead of failing the parse.
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        #[non_exhaustive]
        pub enum $name {
            $( $(#[$vmeta])* $variant, )+
            /// A value this version of Quorum does not know (kept verbatim, truncated to 256 bytes).
            Unknown(String),
        }

        impl $crate::enums::GraphEnum for $name {
            fn from_graph(value: &str) -> Self {
                $( if value.eq_ignore_ascii_case($wire) { return Self::$variant; } )+
                let mut raw = value.to_owned();
                $crate::de::truncate_on_char_boundary(&mut raw, $crate::enums::MAX_UNKNOWN_LEN);
                Self::Unknown(raw)
            }

            fn as_graph(&self) -> &str {
                match self {
                    $( Self::$variant => $wire, )+
                    Self::Unknown(raw) => raw,
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str($crate::enums::GraphEnum::as_graph(self))
            }
        }

        impl std::str::FromStr for $name {
            type Err = std::convert::Infallible;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(<Self as $crate::enums::GraphEnum>::from_graph(s))
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str($crate::enums::GraphEnum::as_graph(self))
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let raw = <std::borrow::Cow<'de, str> as serde::Deserialize>::deserialize(d)?;
                Ok(<Self as $crate::enums::GraphEnum>::from_graph(&raw))
            }
        }
    };
}

pub(crate) use graph_enum;
