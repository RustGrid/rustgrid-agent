# Hosted candidate executions

Candidate executions shorten the worker-debugging loop without weakening stable
release controls.

1. Run the `Candidate` workflow at the exact commit to test.
2. Wait for its checks and immutable prerelease publication to complete.
3. Copy the full 40-character commit SHA into AgentOps' **Agent candidate
   commit** field.
4. Run only the affected ticket. RustGrid resolves the matching
   `candidate-<commit>` release, verifies GitHub's SHA-256 digest, and persists
   the exact artifact in the execution dispatch record.
5. Once the ticket succeeds end to end, promote that commit through the normal
   protected stable release.

Candidate selection is restricted to platform superadmins. It applies only to
the selected execution and never changes the stable worker channel. Retries
preserve the previously selected artifact unless a platform superadmin
explicitly supplies a different candidate commit.

Canonical workflow contract v3 receives `agent_download_url` and
`agent_sha256` as required `workflow_dispatch` inputs. Existing repositories
need one workflow repair from v2 to v3; subsequent worker releases and
candidates do not require another installation update.

For stable executions, operators may set
`GITHUB_ACTIONS_AGENT_TRACK_LATEST_RELEASE=true` once in RustGrid. Each new
execution then resolves the latest protected, non-prerelease GitHub release and
persists its immutable URL and digest before dispatch. In-flight and replayed
dispatches never follow a moving release pointer.
