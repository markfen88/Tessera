use std::path::{Path, PathBuf};
use std::process::ExitCode;
use tessera_package::{Hook, apply, hook};

fn run() -> tessera_bundle::Result<()> {
    let arguments: Vec<_> = std::env::args().collect();
    let name = Path::new(&arguments[0])
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let root = std::env::var_os("DPKG_ROOT")
        .filter(|root| !root.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| "/".into());
    if root != Path::new("/") {
        return Err(tessera_bundle::Error(
            "native package hooks require dpkg to enter the target chroot".into(),
        ));
    }
    if name == "tessera-package" {
        return match arguments.get(1).map(String::as_str) {
            Some("configure-core") => apply(
                &root,
                "tessera",
                Hook::Postinst,
                &["configure".into()],
                &mut tessera_bundle::Native,
            ),
            Some("configure-web") => apply(
                &root,
                "tessera-web",
                Hook::Postinst,
                &["configure".into()],
                &mut tessera_bundle::Native,
            ),
            _ => Err(tessera_bundle::Error(
                "usage: tessera-package configure-core|configure-web".into(),
            )),
        };
    }
    let package = std::env::var("DPKG_MAINTSCRIPT_PACKAGE")
        .map_err(|_| tessera_bundle::Error("missing DPKG_MAINTSCRIPT_PACKAGE".into()))?;
    // dpkg names control hooks preinst/postinst/... and invokes them directly.
    // Their executable payload is this ELF, never an interpreter script.
    let name = std::env::var("DPKG_MAINTSCRIPT_NAME")
        .unwrap_or_else(|_| name.rsplit('.').next().unwrap_or(name).to_owned());
    apply(
        &root,
        &package,
        hook(&name)?,
        &arguments[1..],
        &mut tessera_bundle::Native,
    )
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("tessera-package: {error}");
            ExitCode::FAILURE
        }
    }
}
