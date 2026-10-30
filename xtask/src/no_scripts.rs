use crate::{Failure, Result, output};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::Command;

// Each exception must name one exact repository-relative path and an external
// requirement. Transitional scripts are deliberately not exceptions. See
// packaging/README.md, "Script policy". The intended final allowlist is empty.
const ALLOWLIST: &[(&str, &str)] = &[];

fn words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    for character in text.chars() {
        if escaped {
            word.push(character);
            escaped = false;
        } else if character == '\\' && quote != Some('\'') {
            escaped = true;
        } else if quote == Some(character) {
            quote = None;
        } else if matches!(character, '\'' | '"') && quote.is_none() {
            quote = Some(character);
        } else if character.is_whitespace() && quote.is_none() {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
        } else {
            word.push(character);
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

fn shell(name: &str) -> bool {
    matches!(
        name.rsplit('/').next().unwrap_or(name),
        "sh" | "bash" | "dash" | "ash" | "ksh" | "zsh" | "fish" | "csh" | "tcsh" | "mksh" | "yash"
    )
}

fn env_shell(tokens: &[String]) -> bool {
    let mut index = 0;
    while let Some(word) = tokens.get(index) {
        if word == "--" {
            return tokens.get(index + 1).is_some_and(|word| shell(word));
        }
        if matches!(
            word.as_str(),
            "-u" | "--unset" | "-C" | "--chdir" | "-a" | "--argv0"
        ) {
            index += 2;
            continue;
        }
        if let Some(command) = word
            .strip_prefix("--split-string=")
            .or_else(|| word.strip_prefix("-S").filter(|value| !value.is_empty()))
        {
            let mut expanded = words(command);
            expanded.extend_from_slice(&tokens[index + 1..]);
            return env_shell(&expanded);
        }
        if word.starts_with('-') || word.contains('=') {
            index += 1;
            continue;
        }
        return shell(word);
    }
    false
}

fn shell_shebang(line: &[u8]) -> bool {
    let Some(line) = line.strip_prefix(b"#!") else {
        return false;
    };
    let text = String::from_utf8_lossy(line);
    let tokens = words(&text);
    let Some(interpreter) = tokens.first() else {
        return false;
    };
    let name = interpreter.rsplit('/').next().unwrap_or(interpreter);
    if shell(name) {
        return true;
    }
    if name == "env" {
        return env_shell(&tokens[1..]);
    }
    name == "busybox" && tokens.get(1).is_some_and(|word| shell(word))
}

fn script_extension(path: &Path) -> bool {
    path.extension().is_some_and(|extension| {
        extension == "sh"
            || extension == "py"
            || (matches!(extension.to_str(), Some("js" | "cjs" | "mjs"))
                && path != Path::new("crates/tessera-web/assets/generated/live.js"))
    })
}

pub(crate) fn check(root: &Path) -> Result<()> {
    // Include tracked and untracked source, respecting the repository's normal
    // ignored build-output boundaries. NUL separators preserve arbitrary names.
    let files = output(Command::new("git").current_dir(root).args([
        "ls-files",
        "--cached",
        "--others",
        "--exclude-standard",
        "-z",
    ]))?;
    let mut failed = false;
    for bytes in files
        .stdout
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
    {
        let name = std::str::from_utf8(bytes)
            .map_err(|_| Failure::new("repository filename is not UTF-8"))?;
        if ALLOWLIST.iter().any(|(path, _)| *path == name) {
            continue;
        }
        let path = root.join(name);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        // This exact generated asset is checked against its TypeScript source
        // by web-assets --check. Other JavaScript files are authored source.
        let mut is_script = script_extension(Path::new(name));
        if !is_script && metadata.is_file() {
            let file = File::open(&path)?;
            let mut reader = BufReader::new(file.take(4096));
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line)?;
            is_script = shell_shebang(&line);
        }
        if is_script {
            eprintln!("script is not allowed: {name:?}");
            failed = true;
        }
    }
    if failed {
        Err(Failure::new(
            "repository contains scripts outside the documented allowlist",
        ))
    } else {
        println!("PASS: no scripts outside the documented allowlist");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_detection_covers_direct_env_and_busybox_interpreters() {
        for line in [
            "#!/bin/sh\n",
            "#! /usr/bin/bash -eu\n",
            "#!/usr/bin/env sh\r\n",
            "#!/usr/bin/env -S bash -e\n",
            "#!/usr/bin/env KEY=value /bin/dash\n",
            "#!/bin/busybox sh\n",
            "#!/usr/bin/fish\n",
            "#!/usr/bin/env -S 'bash' -e\n",
            "#!/usr/bin/env --split-string=sh -e\n",
            "#!/usr/bin/env -u UNUSED -S dash\n",
            "#!/usr/bin/env -Sbash -e\n",
            "#!/usr/bin/env --split-string='KEY=value bash -e'\n",
        ] {
            assert!(shell_shebang(line.as_bytes()), "{line:?}");
        }
        for line in [
            "",
            "# shell",
            "#!/usr/bin/env node\n",
            "#!/usr/bin/env python3 sh\n",
            "#!/bin/busybox cat\n",
        ] {
            assert!(!shell_shebang(line.as_bytes()), "{line:?}");
        }
    }

    #[test]
    fn extensions_apply_even_without_an_executable_bit_or_shebang() {
        assert!(script_extension(Path::new("packaging/build.sh")));
        assert!(script_extension(Path::new("tools/helper.py")));
        assert!(script_extension(Path::new("assets/handwritten.js")));
        assert!(script_extension(Path::new("tests/handwritten.cjs")));
        assert!(script_extension(Path::new("tools/handwritten.mjs")));
        assert!(!script_extension(Path::new(
            "crates/tessera-web/assets/generated/live.js"
        )));
        assert!(!script_extension(Path::new("assets/live.ts")));
        assert!(!script_extension(Path::new("docs/shell.md")));
        assert!(!script_extension(Path::new("src/main.rs")));
    }
}
