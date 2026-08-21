use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use super::*;

const PROFILE_REVISION: &str = "repository-revision:phase2-profile";

fn fixture_root(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/execution_protocol_v1/profile_discovery")
        .join(name)
        .join("repository")
}

fn collect_fixture_files(root: &Path, directory: &Path, files: &mut Vec<PathBuf>) {
    let mut entries = fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("read fixture directory {}: {error}", directory.display()))
        .map(|entry| entry.expect("read fixture directory entry").path())
        .collect::<Vec<_>>();
    entries.sort();

    for path in entries {
        if path.is_dir() {
            collect_fixture_files(root, &path, files);
        } else if path.is_file() {
            path.strip_prefix(root).unwrap_or_else(|error| {
                panic!(
                    "fixture path {} is under {}: {error}",
                    path.display(),
                    root.display()
                )
            });
            files.push(path);
        }
    }
}

fn fixture_observations(name: &str) -> Vec<RepositoryFileObservation> {
    let root = fixture_root(name);
    let mut files = Vec::new();
    collect_fixture_files(&root, &root, &mut files);
    files
        .into_iter()
        .map(|path| {
            let relative = path
                .strip_prefix(&root)
                .expect("fixture file is below repository root")
                .iter()
                .map(|component| component.to_str().expect("fixture paths are UTF-8"))
                .collect::<Vec<_>>()
                .join("/");
            let content = fs::read(&path)
                .unwrap_or_else(|error| panic!("read fixture file {}: {error}", path.display()));
            RepositoryFileObservation::from_bytes(relative, content)
                .expect("checked-in fixture is a bounded repository observation")
        })
        .collect()
}

fn fixture_inventory(name: &str, revision: &str) -> RepositoryInventory {
    RepositoryInventory::new(
        RepositoryRevisionId::new(revision),
        fixture_observations(name),
    )
    .expect("checked-in fixture is a valid bounded inventory")
}

fn fixture_profile(name: &str) -> RepositoryProfile {
    build_repository_profile(&fixture_inventory(name, PROFILE_REVISION))
        .expect("fixture produces a deterministic repository profile")
}

fn ecosystems(profile: &RepositoryProfile) -> BTreeSet<EcosystemKind> {
    profile
        .ecosystems
        .iter()
        .map(|capability| capability.ecosystem)
        .collect()
}

fn validation_commands(profile: &RepositoryProfile) -> BTreeSet<ValidationCommandKind> {
    profile
        .validation_candidates
        .iter()
        .map(|candidate| candidate.command)
        .collect()
}

#[test]
fn checked_in_fixtures_produce_known_ecosystem_profiles_and_candidate_commands() {
    let rust = fixture_profile("rust_minimal");
    assert_eq!(ecosystems(&rust), BTreeSet::from([EcosystemKind::Rust]));
    assert_eq!(
        validation_commands(&rust),
        BTreeSet::from([
            ValidationCommandKind::CargoBuild,
            ValidationCommandKind::CargoTest,
        ])
    );
    assert_eq!(rust.source_roots, vec![ProfilePath::new("src").unwrap()]);

    let node = fixture_profile("node_minimal");
    assert_eq!(ecosystems(&node), BTreeSet::from([EcosystemKind::Node]));
    assert_eq!(
        validation_commands(&node),
        BTreeSet::from([
            ValidationCommandKind::NpmTest,
            ValidationCommandKind::NpmTypecheck,
        ])
    );
    assert_eq!(node.source_roots, vec![ProfilePath::new("src").unwrap()]);

    let python = fixture_profile("python_minimal");
    assert_eq!(ecosystems(&python), BTreeSet::from([EcosystemKind::Python]));
    assert_eq!(
        validation_commands(&python),
        BTreeSet::from([ValidationCommandKind::PythonPytest])
    );
    assert_eq!(python.source_roots, vec![ProfilePath::new("src").unwrap()]);
    assert_eq!(python.test_roots, vec![ProfilePath::new("tests").unwrap()]);

    let go = fixture_profile("go_minimal");
    assert_eq!(ecosystems(&go), BTreeSet::from([EcosystemKind::Go]));
    assert_eq!(
        validation_commands(&go),
        BTreeSet::from([
            ValidationCommandKind::GoBuildAll,
            ValidationCommandKind::GoTestAll,
        ])
    );
    assert_eq!(go.source_roots, vec![ProfilePath::root()]);
    assert_eq!(go.test_roots, vec![ProfilePath::root()]);

    let mixed = fixture_profile("mixed_rust_node");
    assert_eq!(
        ecosystems(&mixed),
        BTreeSet::from([EcosystemKind::Rust, EcosystemKind::Node])
    );
    assert_eq!(
        validation_commands(&mixed),
        BTreeSet::from([
            ValidationCommandKind::CargoBuild,
            ValidationCommandKind::CargoTest,
            ValidationCommandKind::NpmTest,
        ])
    );

    for profile in [&rust, &node, &python, &go, &mixed] {
        profile.validate().expect("canonical fixture profile");
        assert!(!profile.validation_candidates.is_empty());
        assert!(!profile.has_executable_command_authority());
        assert!(
            profile
                .validation_candidates
                .iter()
                .all(|candidate| candidate.authority == CommandAuthority::CandidateOnly)
        );
        assert!(
            profile
                .validation_candidates
                .iter()
                .all(|candidate| matches!(
                    candidate.provenance,
                    CommandProvenance::ParsedProjectMetadata { .. }
                ))
        );
    }
}

#[test]
fn profile_identity_is_order_independent_and_revision_and_content_sensitive() {
    let observations = fixture_observations("rust_minimal");
    let canonical = RepositoryInventory::new(
        RepositoryRevisionId::new(PROFILE_REVISION),
        observations.clone(),
    )
    .expect("canonical inventory");
    let mut reversed_observations = observations.clone();
    reversed_observations.reverse();
    let reversed = RepositoryInventory::new(
        RepositoryRevisionId::new(PROFILE_REVISION),
        reversed_observations,
    )
    .expect("reversed inventory");
    let canonical_profile = build_repository_profile(&canonical).expect("canonical profile");
    let reversed_profile = build_repository_profile(&reversed).expect("reversed profile");
    assert_eq!(canonical_profile, reversed_profile);
    assert_eq!(
        serde_json::to_vec(&canonical_profile).expect("serialize canonical profile"),
        serde_json::to_vec(&reversed_profile).expect("serialize reversed profile")
    );

    let other_revision = RepositoryInventory::new(
        RepositoryRevisionId::new("repository-revision:phase2-profile-next"),
        observations.clone(),
    )
    .expect("same content at another revision");
    let other_revision_profile =
        build_repository_profile(&other_revision).expect("other revision profile");
    assert_ne!(
        canonical_profile.profile_id,
        other_revision_profile.profile_id
    );
    assert_ne!(
        canonical_profile.inventory_fingerprint,
        other_revision_profile.inventory_fingerprint
    );

    let mut changed_observations = observations;
    changed_observations.retain(|file| file.path().as_str() != "Cargo.toml");
    changed_observations.push(
        RepositoryFileObservation::from_bytes(
            "Cargo.toml",
            b"[package]\nname = \"profile-rust-fixture-changed\"\nversion = \"0.1.0\"\n",
        )
        .expect("changed metadata observation"),
    );
    let changed_inventory = RepositoryInventory::new(
        RepositoryRevisionId::new(PROFILE_REVISION),
        changed_observations,
    )
    .expect("changed content inventory");
    let changed_profile = build_repository_profile(&changed_inventory).expect("changed profile");
    assert_ne!(canonical_profile.profile_id, changed_profile.profile_id);
    assert_ne!(
        canonical_profile.inventory_fingerprint,
        changed_profile.inventory_fingerprint
    );
}

#[test]
fn unknown_and_sparse_repositories_fall_back_without_inventing_validation_commands() {
    for fixture in ["unknown_text", "sparse_no_evidence"] {
        let profile = fixture_profile(fixture);

        assert_eq!(
            ecosystems(&profile),
            BTreeSet::from([EcosystemKind::Unknown])
        );
        assert!(profile.validation_candidates.is_empty());
        assert!(!profile.has_executable_command_authority());
        assert!(profile.metadata_files.is_empty());
        assert!(profile.uncertainties.iter().any(|uncertainty| {
            uncertainty.kind == ProfileUncertaintyKind::NoKnownEcosystem
                && uncertainty.path.is_none()
                && uncertainty.evidence_id.is_none()
        }));
    }
}

#[test]
fn generated_output_policy_requires_repository_evidence_not_a_filename_heuristic() {
    let profile = fixture_profile("generated_openapi");
    let generated_client =
        ProfilePath::new("generated/openapi-client/src/apis/DefaultApi.ts").unwrap();
    let ordinary_summary = ProfilePath::new("src/generated_summary.rs").unwrap();

    assert_eq!(
        profile.generated_disposition(&generated_client),
        GeneratedPathDisposition::ReadOnlyGeneratedOutput
    );
    assert!(profile.generated_rules.iter().any(|rule| {
        rule.path == generated_client
            && matches!(
                rule.provenance,
                GeneratedRuleProvenance::FileMarker { .. }
                    | GeneratedRuleProvenance::GeneratorConfiguration { .. }
            )
    }));
    assert_eq!(
        profile.generated_disposition(&ordinary_summary),
        GeneratedPathDisposition::OrdinarySource
    );
    assert!(
        profile
            .generated_rules
            .iter()
            .all(|rule| rule.path != ordinary_summary)
    );
}

#[test]
fn profile_json_is_strict_canonical_and_formatted_output_never_contains_content() {
    let profile = fixture_profile("mixed_rust_node");
    assert!(profile.ecosystems.windows(2).all(|pair| pair[0] <= pair[1]));
    assert!(
        profile
            .source_roots
            .windows(2)
            .all(|pair| pair[0] <= pair[1])
    );
    assert!(profile.test_roots.windows(2).all(|pair| pair[0] <= pair[1]));
    assert!(
        profile
            .metadata_files
            .windows(2)
            .all(|pair| pair[0] <= pair[1])
    );
    assert!(
        profile
            .dependency_files
            .windows(2)
            .all(|pair| pair[0] <= pair[1])
    );
    assert!(
        profile
            .validation_candidates
            .windows(2)
            .all(|pair| pair[0] <= pair[1])
    );
    assert_eq!(
        serde_json::from_slice::<RepositoryProfile>(
            &serde_json::to_vec(&profile).expect("serialize repository profile")
        )
        .expect("strict profile JSON roundtrip"),
        profile
    );

    let mut unknown_field = serde_json::to_value(&profile).expect("serialize profile value");
    unknown_field
        .as_object_mut()
        .expect("profile JSON object")
        .insert("unexpected_profile_field".into(), serde_json::json!(true));
    let error = serde_json::from_value::<RepositoryProfile>(unknown_field)
        .expect_err("unknown repository profile field must be rejected");
    assert!(error.to_string().contains("unexpected_profile_field"));

    const SECRET: &str = "rg-profile-secret-sentinel-91eb1fd0";
    let secret_observation =
        RepositoryFileObservation::from_bytes("config/private.txt", SECRET.as_bytes())
            .expect("bounded private observation");
    let observation_debug = format!("{secret_observation:?}");
    let secret_inventory = RepositoryInventory::new(
        RepositoryRevisionId::new("repository-revision:secret-redaction"),
        vec![secret_observation],
    )
    .expect("private inventory");
    let inventory_debug = format!("{secret_inventory:?}");
    let secret_profile =
        build_repository_profile(&secret_inventory).expect("private repository profile");
    let profile_debug = format!("{secret_profile:?}");
    let profile_json = serde_json::to_string(&secret_profile).expect("serialize private profile");

    for formatted in [
        observation_debug.as_str(),
        inventory_debug.as_str(),
        profile_debug.as_str(),
        profile_json.as_str(),
    ] {
        assert!(!formatted.contains(SECRET));
    }
    assert!(observation_debug.contains("<redacted>"));
}
