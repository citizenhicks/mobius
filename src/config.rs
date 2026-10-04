//! Typed loading of owner-local configuration embedded in the binary.

use serde::de::DeserializeOwned;

/// Decodes a complete embedded TOML document into its owning type.
///
/// # Panics
/// Panics if the bundled document is invalid. External configuration must use a
/// fallible decoder; this function is only for build-owned, tested source data.
pub fn embedded<T: DeserializeOwned>(text: &str) -> T {
    toml::from_str(text).unwrap_or_else(|error| panic!("invalid embedded TOML: {error}"))
}

/// Declares defaulted settings backed by a complete owner-local TOML document.
///
/// The private complete decoder does not invoke `Default`, preventing recursive
/// initialization when Serde fills omitted external settings. Use `copy;` for
/// settings containing only `Copy` fields.
/// A `static NAME: Type = text;` declaration shares the same loader for complete
/// embedded catalogs without adding external deserialization defaults.
#[macro_export]
macro_rules! embedded_config {
    ($(#[$attribute:meta])* $visibility:vis static $name:ident: $type:ty = $text:expr;) => {
        $(#[$attribute])*
        $visibility static $name: std::sync::LazyLock<$type> =
            std::sync::LazyLock::new(|| $crate::config::embedded($text));
    };
    (copy;
        $(#[$attribute:meta])* $visibility:vis struct $name:ident {
            $($(#[$field_attribute:meta])* $field_visibility:vis $field:ident: $type:ty,)*
        }
        defaults = $text:expr;
    ) => {
        $(#[$attribute])*
        #[derive(serde::Serialize, serde::Deserialize)]
        #[serde(default, deny_unknown_fields)]
        $visibility struct $name {
            $($(#[$field_attribute])* $field_visibility $field: $type,)*
        }

        impl Default for $name {
            fn default() -> Self {
                #[derive(serde::Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Complete {
                    $($field: $type,)*
                }
                static DEFAULTS: std::sync::LazyLock<$name> = std::sync::LazyLock::new(|| {
                    let complete: Complete = $crate::config::embedded($text);
                    $name { $($field: complete.$field,)* }
                });
                *DEFAULTS
            }
        }
    };
}

#[cfg(test)]
mod tests {
    embedded_config! {
        copy;
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        struct Settings {
            timeout: u64,
            count: usize,
        }
        defaults = "timeout = 12\ncount = 4";
    }

    #[test]
    fn complete_defaults_and_partial_external_settings_do_not_recurse() {
        let partial: Settings = toml::from_str("count = 2").unwrap();
        assert_eq!(
            partial,
            Settings {
                timeout: 12,
                count: 2
            }
        );
        assert!(toml::from_str::<Settings>("cout = 2").is_err());
        assert_eq!(
            super::embedded::<Settings>("timeout = 8\ncount = 1").timeout,
            8
        );
    }
}
