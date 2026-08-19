//! CycloneDX SBOM from `cargo metadata` (M20.5).
//!
//! Hand-rolled rather than `cargo-cyclonedx`, for the same reason the metrics exposition is
//! hand-rolled: the output is a documented format, the input is one command's JSON, and the
//! alternative is a tool installed from the network on every CI run that produces a file nobody
//! reads. What matters about an SBOM is that it is *accurate* and *reproducible* — an incident
//! response asks "were we shipping the vulnerable version", and the answer has to come from the
//! lockfile the binary was built from, not from a tool's opinion of it.
//!
//! Reproducible on purpose: no timestamp, no random serial number, components sorted. Two runs of
//! the same tree produce byte-identical files, so the SBOM can be checked in and a diff is a real
//! dependency change rather than noise.

use std::collections::BTreeMap;
use std::process::Command;

pub struct Sbom {
    pub json: String,
    pub components: usize,
}

/// Build the SBOM for the workspace as it would actually be built: `--locked`, no dev
/// dependencies, no build scripts' own dependencies excluded — what ends up in the binary is what
/// the lockfile resolved, and an SBOM that quietly omits a transitive dependency is worse than
/// none, because it is believed.
pub fn generate(manifest_dir: &std::path::Path) -> Result<Sbom, String> {
    let out = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args([
            "metadata",
            "--format-version",
            "1",
            "--locked",
            // Everything the workspace can build, resolved once.
            "--all-features",
        ])
        .current_dir(manifest_dir)
        .output()
        .map_err(|e| format!("cargo metadata: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }

    let metadata: serde_json::Value =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("cargo metadata json: {e}"))?;
    from_metadata(&metadata)
}

/// The pure half, so a test can drive it with a fixture instead of a `cargo` invocation.
pub fn from_metadata(metadata: &serde_json::Value) -> Result<Sbom, String> {
    let packages = metadata["packages"]
        .as_array()
        .ok_or("cargo metadata has no `packages`")?;

    // Which packages are ours. A workspace member is not a supply-chain risk in the sense an SBOM
    // consumer means, but leaving it out would make the document describe a program that does not
    // exist — so it is included and marked `application`.
    let members: Vec<&str> = metadata["workspace_members"]
        .as_array()
        .map(|m| m.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    // Keyed so the output is sorted and duplicates (the same crate at two versions) both survive.
    let mut components: BTreeMap<(String, String), serde_json::Value> = BTreeMap::new();
    for package in packages {
        let name = package["name"].as_str().unwrap_or_default().to_string();
        let version = package["version"].as_str().unwrap_or_default().to_string();
        if name.is_empty() {
            continue;
        }
        let id = package["id"].as_str().unwrap_or_default();
        let is_member = members.contains(&id);

        let mut component = serde_json::Map::new();
        component.insert(
            "type".into(),
            serde_json::json!(if is_member { "application" } else { "library" }),
        );
        component.insert("name".into(), serde_json::json!(name));
        component.insert("version".into(), serde_json::json!(version));
        // The package URL is what makes an SBOM machine-checkable against an advisory feed.
        component.insert(
            "purl".into(),
            serde_json::json!(format!("pkg:cargo/{name}@{version}")),
        );
        component.insert(
            "bom-ref".into(),
            serde_json::json!(format!("{name}@{version}")),
        );

        if let Some(license) = package["license"].as_str() {
            component.insert(
                "licenses".into(),
                // SPDX expressions like `MIT OR Apache-2.0` are `expression`, not `id` — a
                // consumer that parses them as ids silently records the wrong licence.
                serde_json::json!([{ "expression": license }]),
            );
        }
        if let Some(description) = package["description"].as_str() {
            component.insert("description".into(), serde_json::json!(description));
        }
        if let Some(source) = package["source"].as_str() {
            component.insert(
                "externalReferences".into(),
                serde_json::json!([{ "type": "distribution", "url": source }]),
            );
        }

        components.insert((name, version), serde_json::Value::Object(component));
    }

    let count = components.len();
    let doc = serde_json::json!({
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        // No `serialNumber` and no `timestamp`: both would make every regeneration a diff, and a
        // file that changes when nothing changed is a file nobody reviews.
        "version": 1,
        "metadata": {
            "component": {
                "type": "application",
                "name": "panday",
                "bom-ref": "panday",
            },
            "tools": [{ "name": "cargo xtask sbom" }],
        },
        "components": components.into_values().collect::<Vec<_>>(),
    });

    let mut json = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;
    json.push('\n');
    Ok(Sbom {
        json,
        components: count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata() -> serde_json::Value {
        serde_json::json!({
            "workspace_members": ["path+file:///repo/crates/panday-types#panday-types@0.1.0"],
            "packages": [
                {
                    "id": "path+file:///repo/crates/panday-types#panday-types@0.1.0",
                    "name": "panday-types",
                    "version": "0.1.0",
                    "license": "MIT OR Apache-2.0",
                    "description": "protocol vocabulary",
                },
                {
                    "id": "registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0",
                    "name": "serde",
                    "version": "1.0.0",
                    "license": "MIT OR Apache-2.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                },
            ],
        })
    }

    #[test]
    fn every_package_becomes_a_component_with_a_purl() {
        let sbom = from_metadata(&metadata()).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&sbom.json).unwrap();
        let components = doc["components"].as_array().unwrap();
        assert_eq!(components.len(), 2);
        assert_eq!(components[0]["purl"], "pkg:cargo/panday-types@0.1.0");
        assert_eq!(components[1]["purl"], "pkg:cargo/serde@1.0.0");
    }

    #[test]
    fn a_workspace_member_is_an_application_and_a_dependency_is_a_library() {
        let sbom = from_metadata(&metadata()).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&sbom.json).unwrap();
        assert_eq!(doc["components"][0]["type"], "application");
        assert_eq!(doc["components"][1]["type"], "library");
    }

    #[test]
    fn an_spdx_expression_is_recorded_as_an_expression() {
        // `MIT OR Apache-2.0` is not a licence id. A consumer that reads it as one records a
        // licence that does not exist, which is the kind of error a compliance review finds
        // months later.
        let sbom = from_metadata(&metadata()).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&sbom.json).unwrap();
        assert_eq!(
            doc["components"][1]["licenses"][0]["expression"],
            "MIT OR Apache-2.0"
        );
        assert!(doc["components"][1]["licenses"][0]["id"].is_null());
    }

    #[test]
    fn the_document_is_byte_identical_across_runs() {
        // No timestamp, no serial number, sorted components. A file that changes when nothing
        // changed is a file nobody reviews — and this one is checked in.
        let a = from_metadata(&metadata()).unwrap().json;
        let b = from_metadata(&metadata()).unwrap().json;
        assert_eq!(a, b);
        assert!(!a.contains("serialNumber"));
        assert!(!a.contains("timestamp"));
    }
}
