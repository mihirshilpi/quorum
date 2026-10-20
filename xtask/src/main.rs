//! `cargo xtask <task>`: Quorum's quality gates and build helpers.
#![expect(clippy::print_stdout, reason = "xtask reports to the terminal")]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

mod icon;
use icon::icon;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let task = args.first().map(String::as_str).unwrap_or("help");
    let rest = args.get(1..).unwrap_or_default();
    match task {
        "ci" => ci(),
        "layers" => layers(),
        "assets" => assets(),
        "wasm" => wasm(),
        "ui" => ui_build(false),
        "dev" => dev(rest),
        "contributors" => contributors(rest),
        "fixtures" => fixtures(),
        "icon" => icon(),
        _ => {
            println!(
                "tasks: ci, layers, assets, fixtures [check], wasm, ui, dev [-- app args], contributors [--check]"
            );
            Ok(())
        }
    }
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().map(Path::to_path_buf).unwrap_or_default()
}

fn run(cmd: &mut Command) -> Result<()> {
    println!("$ {cmd:?}");
    let status = cmd.current_dir(root()).status().with_context(|| format!("spawning {cmd:?}"))?;
    if !status.success() {
        bail!("{cmd:?} failed with {status}");
    }
    Ok(())
}

fn cargo() -> Command {
    Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
}

fn ci() -> Result<()> {
    layers()?;
    assets()?;
    fixtures()?;
    contributors(&["--check".to_owned()])?;
    ui_build(false)?;
    run(cargo().args(["fmt", "--all", "--check"]))?;
    run(cargo().args(["clippy", "--workspace", "--all-targets", "--all-features", "--", "-D", "warnings"]))?;
    run(cargo().args(["test", "--workspace", "--all-features"]))?;
    wasm()?;
    println!("ci: all gates passed");
    Ok(())
}

/// Builds the Leptos UI with Trunk into `crates/ui/dist` (the Tauri frontend).
fn ui_build(release: bool) -> Result<()> {
    let mut cmd = Command::new("trunk");
    cmd.args(["build", "--config", "crates/ui/Trunk.toml"]);
    if release {
        cmd.arg("--release");
    }
    run(&mut cmd)
}

fn wasm() -> Result<()> {
    run(cargo().args(["check", "-p", "quorum-ui", "--target", "wasm32-unknown-unknown"]))
}

fn dev(rest: &[String]) -> Result<()> {
    ui_build(false)?;
    let mut cmd = cargo();
    cmd.args(["run", "-p", "quorum", "--"]);
    cmd.args(rest.iter().filter(|a| a.as_str() != "--"));
    run(&mut cmd)
}

/// Layer of every crate. A crate may depend only on crates in strictly lower layers, except
/// that crates at L4 and above may depend on `quorum-ipc`.
fn layer_table() -> BTreeMap<&'static str, u8> {
    BTreeMap::from([
        ("quorum-model", 0),
        ("quorum-sanitize", 0),
        ("quorum-graph", 1),
        ("quorum-auth", 1),
        ("quorum-store", 2),
        ("quorum-sync", 3),
        ("quorum-engine", 4),
        ("quorum-ipc", 0),
        ("quorum-ui", 5),
        ("quorum-mcp", 5),
        ("quorum-control", 5),
        ("quorum", 6),
        ("quorum-cli", 6),
    ])
}

/// UI and shell crates that must never appear below L4.
const UI_CRATES: &[&str] = &["tauri", "tauri-build", "wry", "tao", "leptos", "web-sys", "wasm-bindgen", "js-sys"];

fn manifests() -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for dir in ["crates", "apps"] {
        for entry in std::fs::read_dir(root().join(dir))? {
            let p = entry?.path().join("Cargo.toml");
            if p.exists() {
                out.push(p);
            }
        }
    }
    out.sort();
    Ok(out)
}

fn layers() -> Result<()> {
    let table = layer_table();
    let mut errors = Vec::new();
    for manifest in manifests()? {
        let doc: toml::Table = std::fs::read_to_string(&manifest)?.parse()?;
        let name =
            doc.get("package").and_then(|p| p.get("name")).and_then(|n| n.as_str()).unwrap_or_default().to_owned();
        let Some(&layer) = table.get(name.as_str()) else {
            errors.push(format!("{name}: not in xtask layer table"));
            continue;
        };
        let deps = doc.get("dependencies").and_then(|d| d.as_table()).cloned().unwrap_or_default();
        for dep in deps.keys() {
            if let Some(&dl) = table.get(dep.as_str()) {
                let ipc_ok = dep == "quorum-ipc" && layer >= 4;
                if dl >= layer && !ipc_ok {
                    errors.push(format!("{name} (L{layer}) depends on {dep} (L{dl})"));
                }
            }
            if layer < 5 && UI_CRATES.contains(&dep.as_str()) {
                errors.push(format!("{name} (L{layer}) depends on UI crate {dep}"));
            }
        }
    }
    if errors.is_empty() {
        println!("layers: ok");
        Ok(())
    } else {
        bail!("layer violations:\n  {}", errors.join("\n  "))
    }
}

/// Every binary asset file must have a row in ATTRIBUTION.md.
fn assets() -> Result<()> {
    const EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "svg", "ico", "icns", "ttf", "otf", "woff", "woff2", "webp"];
    const SKIP: &[&str] = &["target", ".git", "temp", "dist", "node_modules"];
    let attribution = std::fs::read_to_string(root().join("ATTRIBUTION.md")).unwrap_or_default();
    let mut missing = Vec::new();
    let mut stack = vec![root()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let p = entry?.path();
            let fname = p.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_owned();
            if p.is_dir() {
                if !SKIP.contains(&fname.as_str()) && !fname.starts_with("target") {
                    stack.push(p);
                }
                continue;
            }
            let ext = p.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
            if EXTS.contains(&ext.as_str()) {
                let rel = p.strip_prefix(root()).unwrap_or(&p).to_string_lossy().replace('\\', "/");
                if !attribution.contains(&format!("`{rel}`")) {
                    missing.push(rel);
                }
            }
        }
    }
    if missing.is_empty() {
        println!("assets: ok");
        Ok(())
    } else {
        bail!("assets without an ATTRIBUTION.md row:\n  {}", missing.join("\n  "))
    }
}

/// Tenant data never enters the repo: every e-mail address in `fixtures/graph/**/*.json` must
/// use a reserved example domain, and every GUID must be an obvious fake
/// (`00000000-0000-…` style), so a recorded fixture that was not scrubbed fails CI.
fn fixtures() -> Result<()> {
    let mut problems = Vec::new();
    let mut stack = vec![root().join("fixtures/graph")];
    let mut files = 0usize;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries {
            let p = entry?.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            if p.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            files += 1;
            let text = std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
            let rel = p.strip_prefix(root()).unwrap_or(&p).to_string_lossy().replace('\\', "/");
            for addr in emails(&text) {
                if !allowed_domain(&addr) {
                    problems.push(format!("{rel}: address with a non-example domain: {addr}"));
                }
            }
            for guid in guids(&text) {
                if !fake_guid(&guid) {
                    problems.push(format!("{rel}: GUID that looks real (tenant/object id?): {guid}"));
                }
            }
        }
    }
    problems.sort();
    problems.dedup();
    if problems.is_empty() {
        println!("fixtures: ok ({files} files)");
        Ok(())
    } else {
        bail!(
            "fixtures may contain tenant data (use example.com / contoso.example and 00000000-… ids):\n  {}",
            problems.join("\n  ")
        )
    }
}

/// Every `local@domain` in `text` (simple scan, no regex).
fn emails(text: &str) -> Vec<String> {
    let local_ok = |c: char| c.is_ascii_alphanumeric() || "._%+-'".contains(c);
    let domain_ok = |c: char| c.is_ascii_alphanumeric() || ".-".contains(c);
    let mut out = Vec::new();
    for (at, _) in text.match_indices('@') {
        let before = text.get(..at).unwrap_or_default();
        let after = text.get(at + 1..).unwrap_or_default();
        let local: String =
            before.chars().rev().take_while(|c| local_ok(*c)).collect::<Vec<_>>().into_iter().rev().collect();
        let domain: String = after.chars().take_while(|c| domain_ok(*c)).collect();
        let domain = domain.trim_end_matches(['.', '-']).to_ascii_lowercase();
        // An address needs an alphabetic top-level label; `image001.png@01D0.0000` is a
        // Content-ID, not an address.
        let tld = domain.rsplit('.').next().unwrap_or_default();
        if local.is_empty() || !domain.contains('.') || tld.is_empty() || !tld.chars().all(|c| c.is_ascii_alphabetic())
        {
            continue;
        }
        out.push(format!("{local}@{domain}"));
    }
    out
}

/// Reserved example domains, plus Teams thread-id suffixes and OData annotations that
/// look like addresses but are not.
fn allowed_domain(addr: &str) -> bool {
    let domain = addr.rsplit('@').next().unwrap_or_default();
    const EXACT: &[&str] = &[
        "example.com",
        "example.org",
        "example.net",
        "contoso.example",
        "thread.tacv2",
        "thread.v2",
        "thread.skype",
        "unq.gbl.spaces",
    ];
    EXACT.contains(&domain)
        || [".example", ".example.com", ".example.org", ".example.net"].iter().any(|s| domain.ends_with(s))
        || domain.starts_with("odata.")
        || domain.starts_with("microsoft.graph.")
}

/// Every 8-4-4-4-12 hex GUID in `text`.
fn guids(text: &str) -> Vec<String> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let is_hex = |c: u8| c.is_ascii_hexdigit();
    let mut i = 0usize;
    while i + 36 <= b.len() {
        let window = b.get(i..i + 36).unwrap_or_default();
        let shape_ok = window.iter().enumerate().all(|(k, c)| match k {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => is_hex(*c),
        });
        let left_ok = i == 0 || b.get(i - 1).is_some_and(|c| !is_hex(*c) && *c != b'-');
        let right_ok = b.get(i + 36).is_none_or(|c| !is_hex(*c) && *c != b'-');
        if shape_ok && left_ok && right_ok {
            out.push(String::from_utf8_lossy(window).into_owned());
            i += 36;
        } else {
            i += 1;
        }
    }
    out
}

/// A fake GUID: the second group is `0000` and the last group starts with eight zeros
/// (`00000000-0000-0000-0000-0000000000f1`, `5b6c8a52-0000-4000-8000-000000000001`).
fn fake_guid(guid: &str) -> bool {
    let parts: Vec<&str> = guid.split('-').collect();
    parts.get(1).is_some_and(|p| *p == "0000") && parts.get(4).is_some_and(|p| p.starts_with("00000000"))
}

/// Generates `contributors/contributors.json` from git history, or checks it with `--check`.
fn contributors(rest: &[String]) -> Result<()> {
    let check = rest.iter().any(|a| a == "--check");
    let people_path = root().join("contributors/people.toml");
    let people: toml::Table =
        std::fs::read_to_string(&people_path).unwrap_or_default().parse().context("people.toml")?;
    if people.get("people").is_some_and(|p| !p.is_table()) {
        bail!("contributors/people.toml: `people` must be a table of [people.<github-username>] entries");
    }
    let out = Command::new("git").args(["shortlog", "-sne", "HEAD"]).current_dir(root()).output();
    let mut list = Vec::new();
    if let Ok(out) = out {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let line = line.trim();
            let Some((count, who)) = line.split_once('\t') else { continue };
            let name = who.split(" <").next().unwrap_or(who).trim();
            list.push(serde_json::json!({ "name": name, "commits": count.trim().parse::<u64>().unwrap_or(0) }));
        }
    }
    let json = serde_json::to_string_pretty(
        &serde_json::json!({ "generated_by": "cargo xtask contributors", "contributors": list }),
    )?;
    let path = root().join("contributors/contributors.json");
    if check {
        if !path.exists() {
            bail!("contributors/contributors.json missing; run `cargo xtask contributors`");
        }
        println!("contributors: ok");
        return Ok(());
    }
    std::fs::write(&path, json + "\n")?;
    println!("contributors: wrote {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_scan_flags_real_looking_data_only() {
        let text = r#"{"a":"avery@example.com","b":"x@sub.contoso.example","c":"bob@realcorp.com",
            "@odata.nextLink":"u","cid":"image001.png@01D00000.00000000","chat":"19:abc@thread.v2"}"#;
        let bad: Vec<String> = emails(text).into_iter().filter(|a| !allowed_domain(a)).collect();
        assert_eq!(bad, ["bob@realcorp.com"]);
        let ids = guids("t=00000000-0000-0000-0000-0000000000f1 r=72f988bf-86f1-41af-91ab-2d7cd011db47");
        assert_eq!(ids.len(), 2);
        assert!(fake_guid(ids.first().map_or("", String::as_str)));
        assert!(!fake_guid(ids.get(1).map_or("", String::as_str)));
        assert!(fake_guid("5b6c8a52-0000-4000-8000-000000000001"));
    }
}
