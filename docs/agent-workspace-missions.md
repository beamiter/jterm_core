# Multi-repository Missions

A Mission is a saved acceptance goal and a set of exact repository candidates.
It links existing task conversations and native run identities. The session
host remains the sole SQLite writer and execution owner.

## Native workflow

Open **Missions** from the Agent Workspace. Anvil and Forge open the native
workspace canvas; Ember and Frost use their native inspection surfaces.

1. Create a five-repository plan with the absolute top-level checkout paths
   for `jterm_core`, `anvil`, `forge`, `ember`, and `frost`.
2. Set the acceptance goal, target refs and required checks. `validation` is
   the built-in check supplied by an existing hosted task validation run.
3. Inspect evidence. Missing paths, dirty inputs, unavailable remote refs and
   mismatched dependency pins remain visible.
4. Use **Use current candidates** to explicitly freeze clean observed HEADs.
   This preserves the initial baseline and updates the four frontend core-pin
   requirements to the selected core candidate. It does not modify Cargo files.
5. Link existing hosted tasks whose exact worktree paths match the plan.
   Continue, validate and accept tasks using their existing task controls.
6. Inspect again to view committed, pushed, integrated, validated and pin
   statuses for the same candidates. Click the task identity to review its
   existing diff, output and approval surface.

Changing the plan increments its revision and invalidates its stored report.
Conflicting windows retain their drafts. An uncertain write is read back with
the same stable Mission ID and is never automatically resubmitted.

## What the observations mean

- Local fingerprints reuse the bounded task-review capture, including tracked,
  staged and untracked inputs. They exclude ignored build output. Inspection
  is a dated observation, not a filesystem lock or attribution of edits.
- Manifest and lockfile pins are read from the observed commit independently.
  A source override or another repository is not proof of the required core pin.
- Public GitHub target refs are observed through a fixed HTTPS read. No push,
  fetch, credential import, branch creation or merge happens during inspection.
  Private/offline targets remain unverified. An advanced target is accepted
  only if candidate ancestry can be proven using existing local objects.
- Local validation is labelled `local`; it cannot satisfy a different required
  environment merely because the plan asks for one. Physical GPU, Wayland,
  IME and remote execution coverage require separate evidence.
- Historical validation cannot validate a changed candidate, dependency set,
  baseline, task run or acceptance contract. A later failure is not hidden by
  an older pass. Missing checks remain incomplete.
- Multi-repository delivery is not atomic. Partial integration remains visible;
  inspecting or editing a Mission never resets or rolls back other repositories.

This release records and inspects delivery. It does not add an unattended
multi-repository push/merge executor, remote Agent execution, cloud dispatch
or a model-account gateway.

## Host upgrade and storage

Missions use the existing host and migrate its database to schema 6. An older
host cannot reopen schema 6. Legacy wire commands retain protocol 2 so an
already-running older host can finish its work. New Mission commands may be
unavailable until that host is deliberately restarted after active work ends.
No frontend kills the host, steals its lease, resumes a historical approval or
replays a model request to complete an upgrade.

The saved catalog is bounded to 64 Missions, 128 KiB per plan and 512 KiB per
inspection. All UI updates run through the shared asynchronous `MissionWorkbench`;
Git observation workers never write SQLite or block the host event loop.
