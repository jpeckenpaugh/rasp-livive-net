# Release and rollback — `rasp.livive.net`

Owner: Jarad Peckenpaugh · Maintained by PM · Last updated: 2026-10-28

This is the operational document for S-01/S-02. It describes what a deploy
actually does, the checklist that must pass before a release, how to roll back,
and what "verified" means for a static site with no server of its own.

Canonical task definitions live in [PLAN.md](../PLAN.md); the decisions that
constrain this page (D18, K1, D6) live in [DECISIONS.md](../DECISIONS.md).

---

## 1. What a deploy is

| | |
|---|---|
| Workflow | `.github/workflows/deploy.yml` (`Deploy`) |
| Trigger | push to `main`, or `workflow_dispatch` on any branch/tag |
| Published URL | **https://rasp.livive.net/** |
| Artifact root | `web/` — exactly what is uploaded, nothing outside it is published |
| Wasm | rebuilt from this commit by `scripts/build_wasm.sh` (D18: never committed) |
| Gate | `cargo test --workspace`, corpus `--check`, wasm present + well-formed, ≤ 1.5 MB gzipped, live `Content-Type: application/wasm` |
| Secrets | none; permissions are `contents: read`, `pages: write`, `id-token: write` |

The deploy job **fails closed**. A missing or oversized wasm, a corpus mismatch,
a failed test, or a wrong MIME type on the live URL turns the run red rather
than publishing a site that cannot instantiate WebAssembly.

Two consequences worth stating plainly:

- **The served bytes are built from the deployed commit.** There is no binary
  in git to drift, and no build cache that can substitute for it.
- **`CNAME` at the repo root is copied into `web/` by the workflow.** It is not
  duplicated in git. A second copy would be a second source of truth that can
  silently disagree about the domain.

---

## 2. One-time setup (before the first deploy)

These are repository settings, not workflow steps. A deploy cannot do them for
you, and GitHub does not apply a `CNAME` file to the Pages settings when the
publishing source is GitHub Actions.

1. **Settings → Pages → Build and deployment → Source: GitHub Actions.**
2. **Settings → Pages → Custom domain: `rasp.livive.net`**, then wait for the
   DNS check and enable **Enforce HTTPS** once a certificate is issued.
3. **DNS:** `rasp` → CNAME → `<org-or-user>.github.io` (no path, no wildcard).
4. **Settings → Environments → `github-pages`:** optionally add a protection
   rule limiting deployment to `main`. Rollback via a tag (section 5, path B)
   needs that tag to be allowed, so if you add the rule, add the release tag
   pattern to its allow-list or use path A instead.
5. Confirm DNS has propagated: `dig +short rasp.livive.net CNAME`.

Until step 2 is done the site still deploys — to
`https://<org>.github.io/rasp-livive-net/` — which is the single most confusing
failure mode available. Check the deployment's `page_url` in the run summary.

---

## 3. Release checklist

Run top to bottom. Do not skip a box because a previous release passed it; the
gates exist because each has failed at least once.

### 3.1 Pre-flight, locally, on the commit you are about to release

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
python3 scripts/vendor_corpus.py --check
bash scripts/build_wasm.sh
```

- [ ] All five commands exit 0.
- [ ] `scripts/build_wasm.sh` printed `wasm: ... B raw / ... B gzip`, and the
      gzip number is **≤ 1,500,000 B** (PLAN §8 / W-02). Paste it into the
      release record (section 7).
- [ ] `wasm-bindgen --version` reports **0.2.129**, matching
      `crates/rasp-wasm/Cargo.toml`. A mismatch is a version error, not a
      corrupt build; fix the CLI.
- [ ] `wasm-opt --version` reports binaryen **133** (the version CI pins).
- [ ] `web/wasm/` is gitignored and untracked (D18). `git status` must not show
      wasm, fixtures, or rendered audio.

Content gates:

- [ ] `web/index.html`, `web/registry.json`, `web/audio/` and (once U-01 lands)
      the app entry point exist. The deploy fails without index/registry.
- [ ] Attribution is present per D15 (`ATTRIBUTION.md` and the UI footer line).
- [ ] `docs/setup.md` §7 predates the W-02 gate and still says CI only prints
      wasm sizes; `.github/workflows/ci.yml` is authoritative until that section
      is updated.

### 3.2 Tag and deploy

```bash
git tag -a vX.Y.Z -m "rasp vX.Y.Z"
git push origin main vX.Y.Z
```

- [ ] The `Deploy` run on `main` is green end to end. A red **build** job means
      nothing was published. A red **deploy** job means the artifact was built
      but either the publish failed or the live checks (MIME / root 200) failed
      — read the job log before re-running, because the second case means a
      broken site may already be live.
- [ ] The run summary shows `wasm size: ... B raw / ... B gzip` and
      `size gate: PASS`. Record the gzip number; it is the number that matters,
      not the raw size.
- [ ] The `github-pages` environment records the deployment against the same
      commit SHA you tagged.

### 3.3 Verify what is actually served

Cold and warm, from a network that is not the runner:

```bash
# wasm must be served as application/wasm, or instantiateStreaming falls back
curl -sI https://rasp.livive.net/wasm/rasp_bg.wasm | grep -i '^content-type:'
# application/wasm

curl -sI https://rasp.livive.net/ | head -1          # HTTP/2 200
curl -sI https://rasp.livive.net/registry.json | grep -i '^access-control-allow-origin:'
# access-control-allow-origin: *     (required by the pp.livive.net consumer)
curl -sI https://rasp.livive.net/audio/af_heart/hello.weba | head -1   # HTTP/2 200
```

- [ ] `content-type: application/wasm` — the deploy job checks this too, but
      verify from outside GitHub's network.
- [ ] Site root returns 200 over HTTPS, with a valid certificate for
      `rasp.livive.net` (not `*.github.io`).
- [ ] `access-control-allow-origin: *` is present (K2; the game fetches these
      cross-origin).
- [ ] One clip fetch and one wasm load succeed in a browser with the console
      open: 0 errors, 0 MIME warnings.
- [ ] `web/qa.html` is reachable but carries `noindex, nofollow`.

### 3.4 Expected headers, and the ones that must not appear

Pages sends `cache-control: max-age=600`, `etag`, `vary: Accept-Encoding`,
`access-control-allow-origin: *`, and `content-type` by extension. It will not
send custom headers, so:

- **No `Cross-Origin-Opener-Policy` / `Cross-Origin-Embedder-Policy`.** This is
  K1: no `SharedArrayBuffer`, no wasm threads. A design that starts working
  locally with COOP/COEP set by a dev server will fail in production.
- No immutable caching. A release is visible within ~10 minutes at the CDN, and
  there is no purge. If a stale asset is suspected, re-check in 10 minutes
  before debugging.

---

## 4. What CI can and cannot tell you

| Check | Where | Proves |
|---|---|---|
| `cargo test --workspace` | build job | the commit's Rust is good |
| `vendor_corpus.py --check` | build job | `web/audio/` matches `web/registry.json` (D6/R8) |
| magic `00 61 73 6d` + size gate | build job | a real module of the right size is in the artifact |
| `Content-Type: application/wasm` | deploy job, live URL | browsers can use `instantiateStreaming` |
| root returns 200 | deploy job, live URL | the artifact has an index and the domain resolves |

It cannot tell you that the audio sounds right, that Safari behaves, or that a
phone is usable. Those are HR-6 / S-03, in `docs/qa/`.

---

## 5. Rollback

**Principle:** the wasm is built from source (D18), so a rollback is *deploy an
older commit*, never *restore an old binary*. This is why the process below
works even when the old artifact has expired.

### 5.1 Find the last good commit

- **Deployment history:** repo → **Environments → `github-pages`** → the list of
  deployments, newest first, each with commit SHA, actor, and timestamp. This is
  the authoritative record of what was live and when. `Settings → Pages` also
  links the most recent successful run.
- **CLI equivalent:**
  ```bash
  gh run list --workflow=deploy.yml --limit 20
  gh api 'repos/{owner}/{repo}/deployments?environment=github-pages' \
    --jq '.[] | [.created_at, .sha, .ref] | @tsv'
  ```
- The commit you want is the last one whose run was green **and** whose site was
  confirmed good in section 3.3.

### 5.2 Path A — revert on `main` (default)

Fastest and leaves a readable history:

```bash
git revert --no-edit <bad-sha>
git push origin main
```

The push triggers `Deploy` for the new revert commit. The revert commit's SHA,
not the old one, becomes what is live — which is fine: D18 means the bytes are
rebuilt from the tree, and the tree is the good tree. Use this unless the bad
commit is entangled with other work.

### 5.3 Path B — pin and deploy a known commit/tag

Use when `main` has moved on and a revert would be surgery, or when you want to
deploy an exact release tag without changing `main`:

```bash
# tag an older commit if it was never tagged
git tag -a vX.Y.Z-rollback <good-sha> -m "rollback to <good-sha>"
git push origin vX.Y.Z-rollback

gh workflow run deploy.yml --ref vX.Y.Z-rollback
```

- `--ref` accepts a **branch or tag**, not a bare SHA. Create a tag (as above)
  or dispatch from the Actions UI with the tag selected.
- This runs the workflow *as it exists in that ref*. If the deploy workflow
  itself was broken by the bad commit, the ref's version of it is used — which
  is usually what you want, but it means an old tag will not pick up later
  hardening of `deploy.yml`.
- The run rebuilds the wasm and re-runs the full gate. It is not a shortcut
  around CI; it is a normal deploy of an older tree.

### 5.4 Path C — re-run the previous successful run

Equivalent to B when the good run is still listed: Actions → `Deploy` → the
green run for the good commit → **Re-run all jobs**. It rebuilds from that
commit's tree and re-uploads a fresh artifact. Prefer B when you need to say
exactly which ref was deployed.

### 5.5 Artifacts and the deployment history

- The Pages artifact is kept for **7 days** (`retention-days` in
  `deploy.yml`, deliberately longer than the 1-day default). Download it from
  the run to diff what was shipped, or to recover the exact `wasm`/`registry`
  for a post-mortem.
- It is **not** a redeploy button: `actions/deploy-pages` deploys the artifact
  uploaded in the same run. The 7 days is forensics, not a rollback mechanism.
  Rollback is always B or C (or A).
- After the retention window the *deployment record* still names the commit, so
  the rollback path survives even though the bytes do not.

### 5.6 After any rollback

- [ ] The rollback run is green.
- [ ] Repeat section 3.3 (MIME, root 200, CORS header, one clip, browser load).
- [ ] Confirm the deployment history now shows the rollback commit.
- [ ] Record it in section 7 with the reason, and open a task for the root cause.
- [ ] If the site is broken *before* any deploy (DNS, certificate, Pages outage),
      do not redeploy repeatedly: check GitHub status first, then re-check the
      custom-domain setting and `Enforce HTTPS`. Re-issuing a certificate can
      take minutes to an hour.

### 5.7 Last resort: take the site down

There is no "unpublish" step, and deleting the deployment does not remove the
CDN copy immediately. To stop serving the app, deploy a minimal maintenance page
as a normal commit to `main` (same workflow, same gates), or disable Pages in
Settings. Both are visible, intentional acts — do not try to hide a broken
release by deleting workflow runs.

---

## 6. Known limits and risks

| Risk | Detail | Mitigation |
|---|---|---|
| Domain not configured | GitHub ignores the artifact `CNAME` for the custom-domain setting with an Actions source | Section 2 is a one-time manual step; check `page_url` after the first deploy |
| No custom headers (K1) | no COOP/COEP, no `SharedArrayBuffer`, no wasm threads; no cache purge | single-threaded design; ~10 min CDN TTL accepted |
| Artifact size | Pages allows up to 10 GB per artifact / 1 GB published site; this site is ≈ 8 MB of assets (7.0 MB corpus + 0.5 MB registry + the wasm) | far below; the wasm gate is the real budget |
| Wasm growth | every dependency is paid for on first load | 1.5 MB gzip gate in both workflows, printed every run |
| Action versions | `deploy.yml` is SHA-pinned because it holds `pages: write`/`id-token: write`; `ci.yml` is tag-pinned by policy because it is read-only | bump SHA + comment together; never bump without a green deploy |
| Rollback depends on retention | artifacts expire after 7 days | rollback rebuilds from source (path A/B/C), so it does not depend on the artifact |
| Concurrent deploys | a second push cancels the in-flight run | Pages serves one deployment; the cancelled run never publishes |
| Branch deploys | `workflow_dispatch` can deploy any branch | restrict the `github-pages` environment if that matters |

---

## 7. Release record

Append-only. One row per release, including rollbacks. The gzip number is the
one printed by `size gate`, not the raw wasm size.

| Date | Version / tag | Commit | wasm gzip (B) | Deploy run | Verified by | Notes |
|---|---|---|---|---|---|---|
| 2026-10-28 | — (workflow only) | — | 264,861 (258.7 KiB) | — | — | S-01: workflow added and verified locally; size is a moving target while DSP work is in flight. First live deploy pending Pages settings (section 2) |
