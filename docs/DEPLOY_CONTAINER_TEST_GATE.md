# deploy.sh test gate vs. the AlmaLinux-8 container

Observed 2026-09-27 while releasing v0.1.6. The host suite is green
(607 tests, 0 failures). The container run is not, and the causes are all
environmental — recorded here so a future release does not mistake them for
regressions, and does not spend an hour re-diagnosing them.

## What happens

`./scripts/deploy.sh --release` runs the test gate *inside* AlmaLinux-8 so the
binary links against a glibc 2.17 floor. On this host that gate does not
converge:

  * With `--test-threads=8` (what the script uses) the run **hangs** — the
    suite had not finished after 50 minutes, and the deploy was killed at the
    3000 s timeout (`DEPLOY_EXIT: 124`).
  * With `--test-threads=1` it finishes in ~97 s and reports **9 failures**.

Both outcomes are the same set of tests, so the hang is a superset of the
failures, not a separate problem.

## The three causes, all environmental

1. **`rg` is not runnable in the container** (3 failures). The container
   inherits a PATH containing host Windows entries, so `Command::new("rg")`
   fails with `Permission denied (os error 13)` instead of the expected
   `NotFound` — the fallback that in-process `grep` relies on never triggers.

   Affected: `search_tests::grep_finds_content_and_skips_ignored_and_deps`,
   `grep_modes_and_flags`, `secret_paths_are_filtered_out_of_results`.

2. **Tests read the container's real `~/.pi/`** (5 failures). The container
   user is `john`, so these resolve against `/home/john/.pi/agent/`, which has
   no `auth.json` and a different `models-store.json` than the host's.
   They are not hermetic; they pass on the host and cannot pass in the
   container without seeding that file.

   Affected: `goal_bootstrap_tests::subagent_runs_a_turn_and_reports_its_reply`,
   `subagent_batch_runs_concurrently_and_merges_in_order`,
   `serial_baseline_exceeds_the_batch_wall_clock`,
   `rewind_undoes_a_real_turn_that_wrote_a_file`,
   `reload_tests::reload_never_moves_active_model_off_label` (gets 2 providers,
   expects 1).

3. **`zellij` e2e tests hang on the host** (skipped in the container, where
   zellij is not installed and the tests self-skip). Confirmed pre-existing:
   unmodified `f7da818` hangs identically in a clean worktree. Not a
   regression from any of the v0.1.6 work.

## What to do

The container's *purpose* is the glibc floor and the zig link, not the test
suite. A reasonable fix, in preference order:

  * Make the affected tests hermetic (seed a temp `PI_DIR`, or skip when the
    catalog/auth store is absent) — fixes cause 2, and is a real improvement.
  * Have the script detect a non-runnable `rg` and skip the three `grep`
    tests, or make the `NotFound`-only `SearchOutcome::Missing` arm also
    treat `PermissionDenied` as missing so the in-process fallback engages
    (fixes cause 1, and is arguably a real bug: the fallback should engage
    whenever the binary cannot be *executed*, not only when it is absent).
  * Until then, run the deploy with `--no-tests` and rely on the host suite
    (which is green) for test coverage — the container build is still needed
    and still does its job.

## 4. The no-API-key smoke test cannot pass on this host (pre-existing)

With `--no-tests --no-clippy` the deploy reaches step 7 and dies:

    one-shot with no API key unexpectedly succeeded (expected graceful failure)

This is a flaw in the smoke test's isolation, not in the binary. The test runs

    env -u ANTHROPIC_API_KEY -u OPENAI_API_KEY -u PI_MODEL PI_DIR=<throwaway> pir

expecting "no credentials, no providers". That is not what happens, because
`load_providers` -> `load_from_auth_fallback` -> `space_bunny_api_key` ->
`commandcode_store_key` reads **`operator_home()/.commandcode/auth.json`**
(`src/config.rs`). That path is deliberately outside `pi_dir()`, so `PI_DIR`
cannot redirect it: the operator's real Command Code subscription key is found
regardless, a `commandcode` provider is manufactured, and the one-shot
legitimately succeeds and exits 0.

That `cfg!(test)` guard was added for the *test* suite ("a developer's own
subscription key leaks into any test that builds a catalog"), but the deploy
smoke test runs a real release binary, where `cfg!(test)` is false, so the leak
is live. Verified by reproducing with a fully clean `env -i` plus a throwaway
`PI_DIR`: still `commandcode/stealth/space-bunny-alpha`, still exit 0.

Consequences: this gate has almost certainly not been meaningful on any host
that has `~/.commandcode/auth.json`. It is unrelated to the v0.1.6 changes.

A real fix is one of:

  * Have the smoke test also override `PIR_TEST_COMMANDCODE_STORE`/add an env
    that points `commandcode_store_key` at nothing, or run it with a throwaway
    `HOME` (the check is `cfg!(test)`, so instead the test needs `HOME` set to
    a directory with no `.commandcode`).
  * Or accept that this box has a usable provider and change the assertion to
    "must not panic" rather than "must exit non-zero", which is the property
    the test actually cares about (the comment says so: "must fail gracefully
    (no panic)").

The simplest immediate unblock: run the deploy with `--fast` (which sets both
`--no-tests` and `--no-clippy`) and do the smoke check by hand, or temporarily
move `~/.commandcode/auth.json` aside for the duration of the deploy.

## Release state as of this writing

v0.1.6 is committed (`25068ae`, plus `f6cbb72` for this doc) and tagged locally.
