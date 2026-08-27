# prep — command-preview / safe-find extension

An extension for `pir` that replaces the builtin `bash` tool with an enhanced
version. Off by default; enable with `PIR_PREP=1`.

It must be registered **before** the builtin (the build script does this
automatically: `prep` is always linked first, so its `bash` shadows the
builtin's — the model never sees two `bash` tools).

## What it does

* **Tail/head preview.** When a command pipes through `tail -n N` or
  `head -n N`, it runs the command *twice*: the real command (as written), and
  a preview of the unfiltered stream feeding the filter (first 12 lines). The
  preview is annotated as `[prep] tail -n N preview ...`, so you can see what is
  being discarded without re-running the command by hand. This is what "give us
  a preview of what is going into `tail`" means.
* **Safe `find`.** A leading `find /` (a whole-filesystem walk) is rewritten to
  the fast, indexed `locate` (e.g. `find / -name '*.rs'` → `locate */*.rs*`). A
  scoped `find .` / `find src` is left untouched, and if `locate` isn't
  installed the command runs as written with a note. This avoids a command that
  would otherwise walk the entire tree.

Everything else (`read_file`, `write_file`, `edit_file`, `list_dir`, `job_*`,
`update_goal`, `commit`, the `wt_*` tools) is still served by the builtin /
other extensions — this extension only overrides `bash`.

## Environment

* `PIR_PREP=1` — enable the extension (replaces `bash` with the enhanced one).

## Notes

* `locate` keeps an indexed database; if it's never been built
  (`updatedb`/`plocate`), results may be empty. That's a system setup detail,
  not an extension bug.
* The preview doubles the work for tail/head commands; it's capped to a few
  lines so it stays cheap.
