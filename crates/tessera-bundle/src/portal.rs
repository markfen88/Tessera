//! What the installer reads of the operator's portal configuration (`web.toml` and the
//! drop-ins of `web.d`), and how the speaker profile's portal drop-in is fitted to it. Nothing
//! here touches the file system or runs a program.
use toml::{Table, Value};

/// What the operator's files say that the speaker profile must respect.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Settings {
    /// An `[[appliance.service]]` list is configured. A list in a drop-in replaces the whole
    /// list of the earlier files, so the profile may not write one over it.
    pub services: bool,
    /// `[network] interface`: the wired port the operator named.
    pub interface: Option<String>,
}

/// Read `texts`, the files in the order the portal reads them (`web.toml`, then `web.d` by
/// name); a later file wins. A file that does not parse is skipped: `tessera-web check` names
/// it later.
#[must_use]
pub fn read(texts: &[String]) -> Settings {
    let mut settings = Settings::default();
    for text in texts {
        let Ok(table) = text.parse::<Table>() else {
            continue;
        };
        if let Some(list) = table
            .get("appliance")
            .and_then(|appliance| appliance.get("service"))
        {
            settings.services = list.as_array().is_some_and(|list| !list.is_empty());
        }
        if let Some(Value::String(interface)) = table
            .get("network")
            .and_then(|network| network.get("interface"))
        {
            settings.interface = Some(interface.clone());
        }
    }
    settings
}

/// The profile's drop-in without its `[[appliance.service]]` tables, which the operator's own
/// list replaces. A comment says where the list is, and what to add to it.
#[must_use]
pub fn without_services(dropin: &str) -> String {
    let mut out = String::new();
    let mut skipping = false;
    let mut noted = false;
    for line in dropin.split_inclusive('\n') {
        let header = line.trim_end().split('#').next().unwrap_or("").trim_end();
        if header.starts_with('[') {
            skipping = header == "[[appliance.service]]";
            if skipping && !noted {
                out.push_str(
                    "# Your own [[appliance.service]] list (web.toml or web.d) is used as it is; the\n\
                     # installer wrote none. To show the EQ engine switch, add\n\
                     #   [[appliance.service]]\n\
                     #   unit = \"camilladsp\"\n\
                     #   label = \"EQ engine\"\n\
                     # to it, then regenerate the polkit rules (`tessera-web appliance polkit`).\n\n",
                );
                noted = true;
            }
        }
        if !skipping {
            out.push_str(line);
        }
    }
    out
}

/// The EQ engine's GUI link of the profile's drop-in: the template's `{host}` (the address the
/// page was reached at) becomes `address`, the management address the GUI listens on; without
/// an address (the GUI is off or listens on this machine only) the `gui_url` line and the
/// comment above it are dropped, and the Speaker page shows no link.
#[must_use]
pub fn with_gui_link(dropin: &str, address: Option<&str>) -> String {
    let lines: Vec<&str> = dropin.split_inclusive('\n').collect();
    let Some(at) = lines.iter().position(|line| line.starts_with("gui_url")) else {
        return dropin.to_owned();
    };
    let mut first = at;
    while first > 0 && lines[first - 1].starts_with('#') {
        first -= 1;
    }
    let mut out = String::new();
    for (index, line) in lines.iter().enumerate() {
        match address {
            None if (first..=at).contains(&index) => {}
            Some(address) if index == first => {
                out.push_str(&link_comment(address));
                if first == at {
                    out.push_str(&line.replace("{host}", &bracketed(address)));
                }
            }
            Some(_) if (first..at).contains(&index) => {}
            Some(address) if index == at => {
                out.push_str(&line.replace("{host}", &bracketed(address)));
            }
            _ => out.push_str(line),
        }
    }
    out
}

fn link_comment(address: &str) -> String {
    format!(
        "# The GUI listens on the management address {address} only (set in\n\
         # /etc/camilladsp/camillagui.yml): change both if that address changes.\n"
    )
}

/// The link to the GUI on `address`, as the portal's `gui_url` states it.
#[must_use]
pub fn gui_url(address: &str) -> String {
    format!(
        "http://{}:{}/gui/index.html",
        bracketed(address),
        crate::dsp::GUI_PORT
    )
}

/// A drop-in that has no `gui_url` gets one at the end of its `[appliance.camilladsp]` table,
/// with the comment the profile writes above it: where `with_gui_link` puts it on the first
/// run. `None` when the drop-in has a `gui_url` already (it is the operator's), has no such
/// table, is not valid TOML, or the result would not say what was meant: the caller then
/// names the line.
#[must_use]
pub fn add_gui_link(dropin: &str, address: &str) -> Option<String> {
    let parsed: Table = dropin.parse().ok()?;
    let camilla = parsed.get("appliance")?.get("camilladsp")?.as_table()?;
    if camilla.contains_key("gui_url") {
        return None;
    }
    let lines: Vec<&str> = dropin.split_inclusive('\n').collect();
    let header = lines
        .iter()
        .position(|line| line.trim_end() == "[appliance.camilladsp]")?;
    // The table ends at the next header, less the blank lines that separate them.
    let mut end = lines
        .iter()
        .enumerate()
        .skip(header + 1)
        .find(|(_, line)| line.trim_start().starts_with('['))
        .map_or(lines.len(), |(index, _)| index);
    while end > header + 1 && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    let url = gui_url(address);
    let mut out: String = lines[..end].concat();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&link_comment(address));
    out.push_str(&format!("gui_url = \"{url}\"\n"));
    out.push_str(&lines[end..].concat());
    let linked: Table = out.parse().ok()?;
    let said = linked["appliance"]["camilladsp"]
        .get("gui_url")
        .and_then(Value::as_str);
    (said == Some(url.as_str())).then_some(out)
}

/// An IPv6 address needs brackets in a URL.
fn bracketed(address: &str) -> String {
    if address.contains(':') && !address.starts_with('[') {
        format!("[{address}]")
    } else {
        address.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEMPLATE: &str =
        include_str!("../../../packaging/bundle/profiles/speaker/web.d/60-speaker.toml.in");

    #[test]
    fn the_last_file_decides_the_service_list_and_the_port() {
        let files =
            |texts: &[&str]| read(&texts.iter().map(|t| (*t).to_owned()).collect::<Vec<_>>());
        assert_eq!(files(&[]), Settings::default());
        assert_eq!(files(&["listen = \"0.0.0.0:8090\"\n"]), Settings::default());
        let list = "[appliance]\nenabled = true\n\n[[appliance.service]]\nunit = \"tesserad\"\nlabel = \"Audio\"\n";
        assert!(files(&[list]).services);
        // A later file with its own list replaces it; an empty one clears it.
        assert!(files(&["listen = \"x\"\n", list, "[appliance]\nenabled = true\n"]).services);
        assert!(!files(&[list, "[appliance]\nservice = []\n"]).services);
        assert_eq!(
            files(&[
                "[network]\ninterface = \"eth0\"\n",
                "[network]\nconfirm = \"token_only\"\n"
            ]),
            Settings {
                services: false,
                interface: Some("eth0".into())
            }
        );
        assert_eq!(
            files(&[
                "[network]\ninterface = \"eth0\"\n",
                "[network]\ninterface = \"end0\"\n"
            ])
            .interface
            .as_deref(),
            Some("end0")
        );
        // A file that does not parse says nothing; the others still count.
        assert!(files(&["[appliance\n", list]).services);
        assert_eq!(files(&["interface = [\n"]), Settings::default());
    }

    #[test]
    fn the_profiles_service_list_gives_way_to_the_operators() {
        assert_eq!(TEMPLATE.matches("[[appliance.service]]").count(), 6);
        let kept = without_services(TEMPLATE);
        assert!(!kept.contains("[[appliance.service]]\nunit"), "{kept}");
        // Everything else stays, and the result is the same drop-in minus the six tables.
        assert!(kept.contains("[appliance]\nenabled = true\n"));
        assert!(kept.contains("[appliance.camilladsp]\nenabled = true\n"));
        assert_eq!(
            kept.matches("add\n#   [[appliance.service]]").count(),
            1,
            "{kept}"
        );
        let parsed: Table = kept.parse().unwrap();
        let appliance = parsed["appliance"].as_table().unwrap();
        assert!(appliance.get("service").is_none());
        assert!(appliance.contains_key("camilladsp"));
        // A drop-in without a list is not touched.
        assert_eq!(
            without_services("[appliance]\nenabled = true\n"),
            "[appliance]\nenabled = true\n"
        );
    }

    #[test]
    fn the_gui_link_names_the_management_address_or_is_left_out() {
        let linked = with_gui_link(TEMPLATE, Some("192.0.2.208"));
        assert!(
            linked.contains("gui_url = \"http://192.0.2.208:5005/gui/index.html\"\n"),
            "{linked}"
        );
        assert!(!linked.contains("{host}"), "{linked}");
        let parsed: Table = linked.parse().unwrap();
        assert_eq!(
            parsed["appliance"]["camilladsp"]["gui_url"].as_str(),
            Some("http://192.0.2.208:5005/gui/index.html")
        );
        assert!(
            with_gui_link(TEMPLATE, Some("fd00::5")).contains("http://[fd00::5]:5005/"),
            "an IPv6 address is bracketed"
        );
        let bare = with_gui_link(TEMPLATE, None);
        assert!(!bare.contains("gui_url"), "{bare}");
        assert!(!bare.contains("{host}"), "{bare}");
        let parsed: Table = bare.parse().unwrap();
        assert!(parsed["appliance"]["camilladsp"].get("gui_url").is_none());
        // Every other line of the template survives.
        let other = |text: &str| -> Vec<String> {
            text.lines()
                .filter(|line| !line.starts_with('#') && !line.starts_with("gui_url"))
                .map(str::to_owned)
                .collect()
        };
        assert_eq!(other(&bare), other(TEMPLATE));
        assert_eq!(other(&linked), other(TEMPLATE));
        assert_eq!(
            with_gui_link("[appliance]\n", Some("10.0.0.1")),
            "[appliance]\n"
        );
    }

    #[test]
    fn a_dropin_written_without_the_address_gets_the_link_and_nothing_else() {
        // The drop-in as the profile writes it while the GUI is off or on loopback.
        let bare = with_gui_link(TEMPLATE, None);
        let linked = add_gui_link(&bare, "192.0.2.208").unwrap();
        let parsed: Table = linked.parse().unwrap();
        assert_eq!(
            parsed["appliance"]["camilladsp"]["gui_url"].as_str(),
            Some("http://192.0.2.208:5005/gui/index.html")
        );
        // What the first run writes with the address is what the second run adds: the same
        // lines in the same place.
        assert_eq!(linked, with_gui_link(TEMPLATE, Some("192.0.2.208")));
        // Nothing but those two comment lines and the key is new.
        let added: Vec<_> = linked
            .lines()
            .filter(|line| !bare.lines().any(|old| old == *line))
            .collect();
        assert_eq!(added.len(), 3, "{added:?}");
        assert!(added[2].starts_with("gui_url = "));
        // An IPv6 address is bracketed, as in the template.
        assert!(
            add_gui_link(&bare, "fd00::5")
                .unwrap()
                .contains("gui_url = \"http://[fd00::5]:5005/gui/index.html\"\n")
        );
        assert_eq!(gui_url("10.0.0.1"), "http://10.0.0.1:5005/gui/index.html");
    }

    #[test]
    fn a_link_of_the_operators_or_a_drop_in_without_the_table_is_left_alone() {
        let mine =
            "[appliance.camilladsp]\nenabled = true\ngui_url = \"http://speaker.lan:5005/\"\n";
        assert_eq!(add_gui_link(mine, "10.0.0.1"), None);
        assert_eq!(
            add_gui_link("[appliance]\nenabled = true\n", "10.0.0.1"),
            None
        );
        assert_eq!(add_gui_link("[appliance.camilladsp\n", "10.0.0.1"), None);
        assert_eq!(add_gui_link("", "10.0.0.1"), None);
        // The table written as dotted keys has no header line to put the key under.
        assert_eq!(
            add_gui_link("[appliance]\ncamilladsp.enabled = true\n", "10.0.0.1"),
            None
        );
        // A header that ends the file without a newline still gets a valid key.
        let linked = add_gui_link("[appliance.camilladsp]", "10.0.0.1").unwrap();
        assert!(linked.parse::<Table>().is_ok(), "{linked}");
        assert!(linked.contains("\ngui_url = \"http://10.0.0.1:5005/gui/index.html\"\n"));
    }
}
