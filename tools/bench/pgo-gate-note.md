## Kernel PGO

Baseline is this commit built the way every x86-64 release build is —
with `-C profile-use` against the fetched profile, the newest
`kernel-profile.yml` collection on the default branch (`docs/pgo.md`).
Candidate is the same commit built against this run's own
`helios-kernel-profdata`, which the `profile-generate` job collected from
an instrumented boot of it. Both are `profile-use` builds of one commit,
booted back to back for every workload on this runner, so what separates
the two columns is the profile and nothing else: the fetched counts
against this run's, with every class covered — `net` included, since the
collection provisions the tap backend and runs the whole suite on it
(#315).

What the pairing measures is whether the fetched profile still describes
this kernel. A candidate that does not beat the baseline says it does; a
candidate that does says the profile has aged, which is the argument for
the collection the next release ships. What a profile buys over none at
all is a different question with a different control:
`baseline_kernel_build=release` pairs the profile-guided kernel against
the same commit built without one.

This comparison reports and does not gate. Its verdict is about the
profile in force, not about the pull request that happened to run it.
