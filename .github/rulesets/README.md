# Branch rulesets

Applied with the GitHub API (see the runbook in `docs/src/developer-guide/github-setup.md`):

```sh
for f in .github/rulesets/dev.json .github/rulesets/main.json; do
  gh api -X POST "repos/esteinig/ont-offload/rulesets" --input "$f"
done
```

| Ruleset | Branch | Merge methods | Rules |
| --- | --- | --- | --- |
| `dev` | `dev` | squash, rebase | PR required, 1 approval (code owners), resolved threads, required checks, no force-push or deletion. Admins may bypass only through a PR |
| `main` | `main` | merge commit | Same checks plus the release gate in `trace`; admins (the `RELEASE_TOKEN` owner) may bypass so the Release workflow can push the version bump and tag |

Required status checks are the CI job names: `lint`, `test`, `build (ubuntu-latest)`, `build (macos-latest)`, `trace`, `supply-chain`, `docs`, `conventions`. If a job is renamed, update both files and re-apply them (`gh api -X PUT repos/esteinig/ont-offload/rulesets/<id> --input <file>`).

Solo maintainer: GitHub does not let you approve your own PR. Either keep the admin PR bypass (default here) or set `required_approving_review_count` to 0.
