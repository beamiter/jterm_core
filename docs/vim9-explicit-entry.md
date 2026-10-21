# Native Vim entry and lifetime

Native IDE construction consumes a `Vim9EntryRequest`. Frontends mint this
request at an IDE button/menu/shortcut action, after parsing an explicit
IDE launch flag, or through the managed shell's exact `--vim-shell` argv route.
The latter is the automatic native GUI entry for an interactive `vim` command;
it forwards the resolved executable, cwd and arguments without parsing a shell
command string. `Vim9Session::start` does not accept an ordinary filesystem
path. Cwd, title, passive shell command text (including vim/nvim), process observation,
bridge traffic, late/cross-tab events and session restore cannot authorize
entry. The request is neither cloneable nor serializable and is consumed once.

Every entry has a fresh non-secret UUID, separate from its bridge credential.
The credential still authenticates the per-editor transport. Native paint
messages queued outside that transport must carry the entry UUID and match an
active controller before being applied. A fresh entry cannot accept an old
entry's queued paint, even when revisions happen to be higher.

On actual editor exit, failed spawn or frontend destruction, adapters revoke
the controller. Revocation applies to all clones, stops its worker, clears
paint, pending commands and editor metadata, and rejects decoded events that
arrive later. Wire JSON cannot restore this permission. Re-entering requires
a fresh user/CLI request and creates a new listener, credential and identity.

A refused quit of a modified buffer is not an exit and must not revoke the
controller. A disconnected bridge likewise must not cause a frontend to
force-close a still-running editor: retain its explicit window/PTY so the user
can save and quit through Vim, but do not accept more native metadata or paint
for the revoked connection. Ordinary terminal tab/session snapshots do not
serialize IDE entry permission.

The boundary tests live in `src/vim9_entry.rs`, the revoke/re-entry and late
frame tests in `src/vim9.rs`, and the shell metadata matrix in
`tests/cwd_mode_isolation.rs`. Exact-argv launch and shell fallback regressions
are in `runtime/vim9/test_shell.py`. Automatic GUI launch does not make terminal
metadata or ordinary `cd` output an editor entry mechanism. See
[the shell entry contract](vim9-shell-scroll.md).
