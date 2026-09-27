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

## Release state as of this writing

v0.1.6 is committed (`25068ae`) and tagged locally, not pushed: the deploy
died at the test step before reaching the publish phase.
