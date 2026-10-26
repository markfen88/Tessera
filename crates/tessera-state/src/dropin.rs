//! Drop-in files of the portal's `web.toml`: `/etc/tessera/web.d/*.toml`, read
//! after `web.toml` in name order. One definition of which files count and how
//! their tables merge, shared by `tessera-web`, which runs with the result, and
//! `tessera-network`, which must see the same listen address and `[network]`
//! policy the portal does. (The daemon's own `config.d` is [`crate::loader`].)

use std::path::{Path, PathBuf};

use toml::{Table, Value};

/// Largest drop-in file read.
pub const MAX_BYTES: u64 = 1 << 20;

/// The directory of the drop-ins of `config_file`: named like the file with
/// `.d` for its extension (`web.toml` -> `web.d`).
#[must_use]
pub fn directory(config_file: &Path) -> PathBuf {
    config_file.with_extension("d")
}

/// Does a file of that name count as a drop-in? Visible `*.toml` files do;
/// hidden files and other extensions are ignored.
#[must_use]
pub fn counts(name: &str) -> bool {
    !name.starts_with('.') && name.ends_with(".toml")
}

/// Merge `over` into `base`: tables merge key by key; any other value, arrays
/// of tables such as `[[appliance.service]]` included, replaces the earlier one
/// as a whole.
pub fn merge(base: &mut Table, over: Table) {
    for (key, value) in over {
        match (base.get_mut(&key), value) {
            (Some(Value::Table(b)), Value::Table(o)) => merge(b, o),
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(text: &str) -> Table {
        text.parse().unwrap()
    }

    #[test]
    fn the_directory_is_named_after_the_file() {
        assert_eq!(
            directory(Path::new("/etc/tessera/web.toml")),
            Path::new("/etc/tessera/web.d")
        );
    }

    #[test]
    fn only_visible_toml_files_count() {
        assert!(counts("60-speaker.toml"));
        assert!(!counts(".hidden.toml"));
        assert!(!counts("notes.txt"));
        assert!(!counts("60-speaker.toml.bak"));
        assert!(!counts("toml"));
    }

    #[test]
    fn tables_merge_and_everything_else_replaces() {
        let mut base = table(
            "listen = \"a\"\n[appliance]\nenabled = false\nlabel = \"x\"\n[[appliance.service]]\nunit = \"one\"\n",
        );
        merge(
            &mut base,
            table(
                "listen = \"b\"\n[appliance]\nenabled = true\n[[appliance.service]]\nunit = \"two\"\n",
            ),
        );
        assert_eq!(base["listen"].as_str(), Some("b"));
        let appliance = base["appliance"].as_table().unwrap();
        assert_eq!(appliance["enabled"].as_bool(), Some(true));
        assert_eq!(appliance["label"].as_str(), Some("x"));
        let services = appliance["service"].as_array().unwrap();
        assert_eq!(services.len(), 1, "an array in a later file replaces it");
        assert_eq!(services[0]["unit"].as_str(), Some("two"));
    }
}
