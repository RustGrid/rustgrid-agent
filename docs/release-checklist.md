# Release checklist

## Source and governance

- [ ] Version and `CHANGELOG.md` are updated.
- [ ] CI, dependency policy, secret scan, and container scan pass.
- [ ] The `production-release` environment accepts only `v*` tags, requires an
      independent reviewer, disables self-approval and admin bypass, and contains
      the reviewed release variables.
- [ ] `HOMEBREW_TAP_TOKEN` is a fine-grained token limited to
      `RustGrid/homebrew-tap` with Contents read/write access.
- [ ] Sandbox template is pinned by verified digest and effective network policy is reviewed.
- [ ] Security and compatibility impacts are documented.
- [ ] Release notes include known limitations and rollback instructions.

## Artifact verification

- [ ] Source package and release binaries install and report the tagged version.
- [ ] Checksums, SPDX SBOM, signatures/attestations, and Homebrew formula are attached.
- [ ] Container image is pinned by digest, scanned, signed/attested, and runs as non-root.
- [ ] The formula passes audit, source install, and test in the public tap.

## Production certification

- [ ] `docs/staging-certification.md` is completed against the exact image digest.
- [ ] Evidence includes run IDs, PR URLs, ordered events, sanitized logs, and resource-limit results.
- [ ] Evidence includes concurrent isolation, orphan cleanup, secret-argv inspection, and workspace-quota termination.
- [ ] The release candidate has operated in staging for the agreed soak period without unresolved P0/P1 findings.
- [ ] Rollback to the previous image has been exercised.
- [ ] An operator and security reviewer approve promotion.

The GitHub release workflow packages artifacts, but a maintainer must use a protected release environment and confirm this checklist before approving the release job.
