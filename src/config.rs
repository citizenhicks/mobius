//! Typed loading of owner-local configuration embedded in the binary.

use std::path::PathBuf;
use std::sync::OnceLock;

use serde::de::DeserializeOwned;

static OVERRIDE_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Sets the directory whose files replace same-named embedded catalogs.
///
/// Must run before the first catalog lookup; later calls are ignored.
pub fn set_override_dir(dir: PathBuf) {
    let _ = OVERRIDE_DIR.set(dir);
}

/// Decodes `name` from the override directory when present and valid, else the embedded copy.
pub fn overridable<T: DeserializeOwned>(
    name: &str,
    text: &str,
    validate: impl Fn(&T) -> crate::Result<()>,
) -> T {
    if let Some(path) = OVERRIDE_DIR.get().map(|dir| dir.join(name))
        && path.is_file()
    {
        let loaded = std::fs::read_to_string(&path)
            .map_err(|error| error.to_string())
            .and_then(|text| toml::from_str::<T>(&text).map_err(|error| error.to_string()))
            .and_then(|value| {
                validate(&value)
                    .map(|()| value)
                    .map_err(|error| error.to_string())
            });
        match loaded {
            Ok(value) => return value,
            Err(error) => tracing::warn!(
                path = %path.display(), %error, "ignoring catalog override, using the bundled catalog"
            ),
        }
    }
    let value = embedded(text);
    validate(&value).unwrap_or_else(|error| panic!("invalid embedded {name}: {error}"));
    value
}

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
    #[test]
    fn override_file_replaces_embedded_unless_invalid() {
        let dir = tempfile::tempdir().expect("tempdir");
        super::set_override_dir(dir.path().into());
        let positive = |value: &u8| {
            (*value > 0)
                .then_some(())
                .ok_or_else(|| crate::Error::Config("zero".into()))
        };
        let load = |name: &str| -> toml::Table {
            super::overridable(name, "n = 1", |table: &toml::Table| {
                positive(&u8::try_from(table["n"].as_integer().unwrap_or(0)).unwrap_or(0))
            })
        };
        std::fs::write(dir.path().join("good.toml"), "n = 7").expect("write");
        std::fs::write(dir.path().join("bad.toml"), "n = 0").expect("write");
        std::fs::write(dir.path().join("broken.toml"), "n =").expect("write");
        assert_eq!(load("good.toml")["n"].as_integer(), Some(7));
        assert_eq!(load("bad.toml")["n"].as_integer(), Some(1));
        assert_eq!(load("broken.toml")["n"].as_integer(), Some(1));
        assert_eq!(load("missing.toml")["n"].as_integer(), Some(1));
    }

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
