# GitHub setup runbook

Sets up `esteinig/ont-offload` on GitHub from the command line with `gh`: repository, branches, settings, Pages, secrets, rulesets and pinned actions. Run it once, from the repository root, as a repository admin.

## 0. Prerequisites

```sh
gh auth login                       # scopes: repo, workflow
gh auth status
export OWNER=esteinig REPO=ont-offload
export R="$OWNER/$REPO"
```

Decide two values first and put them in the workflows if they differ from the defaults:

- `MEDTRACE_REPOSITORY` and `MEDTRACE_VERSION` in `.github/workflows/{ci,release,docs}.yml` (defaults `esteinig/medtrace`, `0.6.0`): the repository that publishes medtrace release binaries and the version CI pins.
- Private or public repository (examples below use `--private`).

## 1. Local repository

The delivered folder is already a git repository with `main` and `dev` and one commit. If you start from the files without history:

```sh
git init -b main
cargo generate-lockfile && cargo xtask ci     # trace/soup need medtrace; lint and tests do not
git add -A && git commit -m "chore: initial scaffold"
git switch -c dev
cog install-hook --all                        # if cocogitto is installed
```

## 2. Create the repository and push both branches

```sh
gh repo create "$R" --private --source . --remote origin \
  --description "Verified offload of ONT sequencing runs to mounted network storage"
git push -u origin main
git push -u origin dev
```

## 3. Repository settings

```sh
gh api -X PATCH "repos/$R" \
  -f default_branch=dev \
  -F delete_branch_on_merge=true \
  -F allow_squash_merge=true -F allow_rebase_merge=true -F allow_merge_commit=true \
  -F allow_auto_merge=true \
  -f squash_merge_commit_title=PR_TITLE -f squash_merge_commit_message=PR_BODY \
  -f merge_commit_title=PR_TITLE -f merge_commit_message=PR_BODY \
  -F has_wiki=false -F has_projects=false

gh repo edit "$R" --add-topic nanopore --add-topic sequencing --add-topic iec62304 --add-topic rust
```

Security features:

```sh
gh api -X PUT "repos/$R/vulnerability-alerts"
gh api -X PUT "repos/$R/automated-security-fixes"
gh api -X PUT "repos/$R/private-vulnerability-reporting"
```

## 4. Actions permissions

Workflows get a read-only token by default and request more per job:

```sh
gh api -X PUT "repos/$R/actions/permissions/workflow" \
  -f default_workflow_permissions=read -F can_approve_pull_request_reviews=true
```

## 5. GitHub Pages and its environment

```sh
gh api -X POST "repos/$R/pages" -f build_type=workflow
# Allow deployments from main only
gh api -X PUT "repos/$R/environments/github-pages" \
  -F "deployment_branch_policy[protected_branches]=false" \
  -F "deployment_branch_policy[custom_branch_policies]=true"
gh api -X POST "repos/$R/environments/github-pages/deployment-branch-policies" -f name=main -f type=branch
```

## 6. Secrets

`RELEASE_TOKEN` lets the Release workflow push the version bump and tag to `main` past its ruleset and open the back-merge PR. Create a fine-grained token in the browser (Settings, Developer settings, Fine-grained tokens) for this repository only, owned by a repository admin, with **Contents: read and write** and **Pull requests: read and write**, then:

```sh
gh secret set RELEASE_TOKEN --repo "$R"        # paste the token when prompted
```

If the medtrace release repository is private, add a token that can read it and pass it to the `setup-medtrace` action's `token:` input in the three workflows:

```sh
gh secret set MEDTRACE_TOKEN --repo "$R"
# then in each "uses: ./.github/actions/setup-medtrace" step add:  token: ${{ secrets.MEDTRACE_TOKEN }}
```

## 7. Labels

```sh
gh label create compliance --color 5319e7 --description "Register, risk or traceability change" --repo "$R" --force
gh label create safety     --color b60205 --description "Possible safety impact" --repo "$R" --force
gh label create release    --color 0e8a16 --description "Release preparation" --repo "$R" --force
```

## 8. Pin actions to commit SHAs

Every third-party action is marked `# pin to a commit SHA`. Resolve and pin them in one go, then commit on a branch:

```sh
git switch -c ci/pin-actions dev
for ref in actions/checkout@v4 actions/upload-artifact@v4 actions/download-artifact@v4 \
           actions/upload-pages-artifact@v3 actions/deploy-pages@v4 \
           dtolnay/rust-toolchain@master Swatinem/rust-cache@v2 taiki-e/install-action@v2 \
           EmbarkStudios/cargo-deny-action@v2 cocogitto/cocogitto-action@v3; do
  repo=${ref%@*}; tag=${ref#*@}
  sha=$(gh api "repos/$repo/commits/$tag" --jq .sha)
  sed -i "s|uses: ${ref} # pin to a commit SHA|uses: ${repo}@${sha} # ${tag}|" .github/workflows/*.yml
done
grep -n "pin to a commit SHA" .github/workflows/*.yml || echo "all pinned"
git commit -am "ci: pin actions to commit SHAs" -m "Refs: SWI-CLI"
git push -u origin ci/pin-actions
gh pr create --base dev --fill
```

Dependabot (`.github/dependabot.yml`) keeps the pins and crates up to date with weekly PRs into `dev`.

## 9. First CI run, then rulesets

Let CI run once on the PR so the check names exist, then apply the rulesets:

```sh
gh pr checks --watch
for f in .github/rulesets/dev.json .github/rulesets/main.json; do
  gh api -X POST "repos/$R/rulesets" --input "$f"
done
gh api "repos/$R/rulesets" --jq '.[] | "\(.id) \(.name) \(.enforcement)"'
```

Required checks: `lint`, `test`, `build (ubuntu-latest)`, `build (macos-latest)`, `trace`, `supply-chain`, `docs`, `conventions`. Until medtrace is installable in CI, `trace`, `supply-chain` and `conventions` fail; either set `enforcement` to `"evaluate"` in both files first, or remove those three contexts and add them back when medtrace is wired in. To update a ruleset later:

```sh
id=$(gh api "repos/$R/rulesets" --jq '.[] | select(.name=="dev") | .id')
gh api -X PUT "repos/$R/rulesets/$id" --input .github/rulesets/dev.json
```

Solo maintainer: GitHub does not count your own approval. The rulesets let admins bypass the approval through a PR; alternatively set `required_approving_review_count` to 0.

## 10. Register and SOUP

With medtrace installed locally:

```sh
git switch -c compliance/initial-soup dev
medtrace check                      # align any register fields it reports
cargo xtask soup --write
git add compliance && git commit -m "compliance: add SOUP items from Cargo.lock" -m "Refs: SWI-CLI"
git push -u origin compliance/initial-soup && gh pr create --base dev --fill
```

## 11. Day-to-day and releases

```sh
# work
git switch -c feat/copy-pipeline dev
# ... commits with Refs: trailers ...
gh pr create --base dev --fill && gh pr merge --squash --auto

# release
gh pr create --base main --head dev --title "chore(release): prepare v0.1.0" --body "Release gate"
gh pr merge --merge
gh workflow run release.yml --ref main -f dry_run=true   # check first
gh workflow run release.yml --ref main
gh run watch
```

## Checklist

- [ ] `MEDTRACE_REPOSITORY` / `MEDTRACE_VERSION` set in the workflows
- [ ] Repository created, `main` and `dev` pushed, default branch `dev`
- [ ] Merge settings, security features, Actions permissions
- [ ] Pages enabled, `github-pages` environment limited to `main`
- [ ] `RELEASE_TOKEN` (and `MEDTRACE_TOKEN` if needed) set
- [ ] Actions pinned to SHAs
- [ ] Rulesets applied after the first CI run
- [ ] SOUP generated and register checked with medtrace
